//! The HTTP exchange behind a request span: headers, cookies, query, bodies and the
//! phase timeline, for the desktop's Request / Response / Timeline tabs.
//!
//! Everything here is written under the SAME attribute keys for an inbound server
//! request and for an outbound curl call (`observer::capture_curl_result` encodes
//! through this module), so one desktop model renders both.
//!
//! Three rules shape the whole module:
//!
//! * **Never change what the service sends.** The response body is taken from what a
//!   framework bridge hands over, or from an output buffer the application already
//!   had; capture never starts a buffer of its own, so streamed and `X-Sendfile`
//!   responses are untouched.
//! * **Bounded by bytes, not by count.** One 8 MiB upload is the same threat to the
//!   spool as a thousand headers, so bodies are truncated to a byte cap and maps are
//!   capped on entries AND bytes, each leaving a `chronos.dropped` note so a reader
//!   never mistakes a bounded map for a complete one.
//! * **Redact before it is ever stored.** Masking happens here, at capture, not in
//!   the reader — a secret that reaches the spool has already leaked.

use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::HashMap;

pub const REQUEST_HEADERS: &str = "http.request.headers";
pub const REQUEST_COOKIES: &str = "http.request.cookies";
pub const REQUEST_QUERY: &str = "http.request.query";
pub const REQUEST_BODY: &str = "http.request.body";
pub const RESPONSE_HEADERS: &str = "http.response.headers";
pub const RESPONSE_BODY: &str = "http.response.body";
pub const TIMELINE: &str = "http.timeline";

// Current OTel semantic-convention keys, DUAL-EMITTED next to the legacy
// spellings (`http.method`, `http.status_code`, `http.url`, `db.statement`)
// rather than replacing them: the engine's span index, the service map and the
// desktop all still read the legacy keys, so removing them is a coordinated
// migration — adding the stable names is not. One constant per key so the
// emit sites in `observer.rs` and `lib.rs` cannot drift in spelling.
pub const REQUEST_METHOD: &str = "http.request.method";
pub const RESPONSE_STATUS_CODE: &str = "http.response.status_code";
pub const URL_FULL: &str = "url.full";
pub const DB_QUERY_TEXT: &str = "db.query.text";

/// The desktop's marker for a map that hit a cap. Surfaced as a note, never as a header.
const DROPPED_KEY: &str = "chronos.dropped";

/// What a masked value is replaced with. Fixed-width rather than value-length so the
/// mask cannot leak how long the secret was.
/// What a masked value is replaced with. Shared with the deterministic profiler's
/// Tier 3 argument capture so a `$apiToken` PARAMETER is masked exactly like an
/// `Authorization` HEADER — one mask, one vocabulary, one thing for a reader to learn.
pub(crate) const MASK: &str = "********";

/// Bytes of body kept INLINE on the span attribute.
///
/// The preview, in other words. It is sized so the overwhelming majority of
/// bodies are whole on the span and need no second fetch at all; anything past it
/// goes to `body_spool` and is loaded on request.
const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024;

/// Bytes of body captured in TOTAL — the preview plus what is spooled behind it.
///
/// Not the same kind of number as the one above. That one trades a round trip;
/// this one trades spool disk on a volume every application in the organisation
/// shares, and index bytes behind it, so the default is a working figure rather
/// than the largest thing that could be made to work.
///
/// Setting it to the inline cap (or below) turns whole-body storage off: the span
/// keeps its preview and nothing is spooled. That is one knob rather than a
/// second flag that could disagree with it.
const DEFAULT_MAX_BODY_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on the configurable total, whatever the setting asks for.
const MAX_BODY_TOTAL_CEILING: usize = 512 * 1024 * 1024;
const MAX_MAP_ENTRIES: usize = 128;
const MAX_MAP_BYTES: usize = 16 * 1024;
const MAX_VALUE_BYTES: usize = 2 * 1024;

/// Substrings that mark a header, cookie or query parameter as carrying a credential.
/// Matched case-insensitively against the KEY, so `X-Refresh-Token` and
/// `refresh_token` are both caught.
const DEFAULT_REDACT_PATTERNS: [&str; 9] = [
    "authorization",
    "password",
    "credential",
    "private_key",
    "client_secret",
    "access_token",
    "refresh_token",
    "secret",
    "token",
];

#[derive(Clone, Debug)]
pub struct HttpCaptureConfig {
    /// Master switch for the whole module (`CHRONOS_PHP_HTTP_CAPTURE`).
    pub enabled: bool,
    /// Whether request/response BODIES are copied, separately from headers: a body is
    /// where the personal data lives, so a service can keep the cheap, safe half of
    /// capture without the expensive, sensitive half.
    pub capture_bodies: bool,
    /// Whether to fall back to `ob_get_contents()` for a response body no bridge
    /// supplied. Off by default: it only sees a body when the application already had
    /// output buffering on, and reading it is a full copy of the response.
    pub response_buffer: bool,
    /// Bytes kept inline on the span attribute.
    pub max_body_bytes: usize,
    /// Bytes captured in total, inline plus spooled. At or below `max_body_bytes`,
    /// nothing is spooled and a long body is simply previewed.
    pub max_body_total_bytes: usize,
    /// Whether to mask credential-looking values. On by default; a service opts out
    /// only for a local debugging session.
    pub redact: bool,
    pub redact_patterns: Vec<String>,
}

impl Default for HttpCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_bodies: false,
            response_buffer: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_body_total_bytes: DEFAULT_MAX_BODY_TOTAL_BYTES,
            redact: true,
            redact_patterns: DEFAULT_REDACT_PATTERNS
                .iter()
                .map(|p| (*p).to_owned())
                .collect(),
        }
    }
}

impl HttpCaptureConfig {
    pub fn resolve() -> Self {
        let defaults = Self::default();
        let enabled = crate::settings::flag("CHRONOS_PHP_HTTP_CAPTURE", true);
        if !enabled {
            return Self {
                enabled: false,
                ..defaults
            };
        }
        let patterns = redaction_patterns().clone();
        let max_body_bytes = env_usize("CHRONOS_PHP_HTTP_CAPTURE_MAX_BODY", DEFAULT_MAX_BODY_BYTES);

        Self {
            enabled: true,
            capture_bodies: crate::settings::flag("CHRONOS_PHP_HTTP_CAPTURE_BODIES", true),
            response_buffer: crate::settings::flag(
                "CHRONOS_PHP_HTTP_CAPTURE_RESPONSE_BUFFER",
                false,
            ),
            max_body_bytes,
            // Never below the inline cap: a total under the preview would make the
            // preview itself a lie about what was captured.
            max_body_total_bytes: env_usize(
                "CHRONOS_PHP_HTTP_CAPTURE_MAX_BODY_TOTAL",
                DEFAULT_MAX_BODY_TOTAL_BYTES,
            )
            .clamp(max_body_bytes, MAX_BODY_TOTAL_CEILING.max(max_body_bytes)),
            redact: crate::settings::flag("CHRONOS_PHP_HTTP_CAPTURE_REDACT", true),
            redact_patterns: patterns,
        }
    }
}

/// A body worth keeping whole, on its way to `body_spool`.
///
/// Produced here rather than there because this module is what decides what
/// capture is allowed to keep; the spool only decides how it is written down.
pub struct StoredBody {
    /// `request` or `response` — which half of the exchange this is.
    pub side: &'static str,
    pub content_type: String,
    pub bytes: String,
}

/// What one request's capture yielded: the span's attributes, and the bodies that
/// did not fit in them.
pub struct Drained {
    pub attributes: Vec<(String, String)>,
    pub bodies: Vec<StoredBody>,
}

/// One request's captured exchange.
#[derive(Default)]
struct Capture {
    config: HttpCaptureConfig,
    request_headers: Vec<(String, String)>,
    request_cookies: Vec<(String, String)>,
    request_query: Vec<(String, String)>,
    request_body: Option<(String, usize, String)>,
    response_headers: Vec<(String, String)>,
    response_body: Option<(String, usize, String)>,
    /// (phase name, nanoseconds from request start at which it BEGAN). `u128` to match
    /// the monotonic clock the rest of the extension measures in.
    phases: Vec<(String, u128)>,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

/// Whether this request's HTTP stack is being captured.
pub fn is_active() -> bool {
    CAPTURE.with(|c| c.borrow().is_some())
}

/// The configuration in force for this request, or a disabled one outside a request.
/// `observer` asks per outbound call, which is why this is a cheap clone rather than
/// a borrow held across the capture.
pub fn current_config() -> HttpCaptureConfig {
    CAPTURE.with(|c| {
        c.borrow()
            .as_ref()
            .map(|capture| capture.config.clone())
            .unwrap_or_default()
    })
}

pub fn reset() {
    CAPTURE.with(|c| *c.borrow_mut() = None);
}

/// Begin capture, reading everything about the request that is already in memory.
///
/// Read at START rather than at drain because `$_SERVER` and `php://input` are only
/// reliably intact before the application runs: a framework is free to rewrite
/// superglobals, and `php://input` can be consumed exactly once by whoever reads it
/// first on some SAPIs.
pub fn on_request_start(config: HttpCaptureConfig) {
    if !config.enabled {
        reset();
        return;
    }

    let server = server_vars();
    let mut capture = Capture {
        request_headers: request_headers(&server),
        request_cookies: parse_pairs(lookup(&server, "HTTP_COOKIE"), ';'),
        request_query: parse_pairs(lookup(&server, "QUERY_STRING"), '&'),
        ..Default::default()
    };

    if config.capture_bodies {
        let content_type = lookup(&server, "CONTENT_TYPE").to_owned();
        // A multipart body is file uploads: megabytes of binary that would be
        // truncated into meaninglessness anyway, and the one shape most likely to
        // carry a document nobody meant to ship to a telemetry store.
        if !content_type.contains("multipart/form-data") {
            if let Some(body) = read_input_stream() {
                // `size` is what the service saw; the copy kept is bounded. The two
                // differing is exactly what `.truncated` reports.
                let size = body.len();
                capture.request_body =
                    Some((truncate(&body, config.max_body_total_bytes), size, content_type));
            }
        }
    }

    capture.config = config;
    CAPTURE.with(|c| *c.borrow_mut() = Some(capture));
}

/// The response as a bridge saw it, which beats anything the output buffer holds.
pub fn set_response(body: String, content_type: String, headers: Vec<(String, String)>) {
    CAPTURE.with(|c| {
        if let Some(capture) = c.borrow_mut().as_mut() {
            if !headers.is_empty() {
                capture.response_headers = headers;
            }
            if capture.config.capture_bodies && !body.is_empty() {
                let size = body.len();
                let kept = truncate(&body, capture.config.max_body_total_bytes);
                capture.response_body = Some((kept, size, content_type));
            }
        }
    });
}

/// Mark the instant a named phase BEGAN, `at_ns` nanoseconds into the request.
pub fn mark_phase(name: &str, at_ns: u128) {
    CAPTURE.with(|c| {
        if let Some(capture) = c.borrow_mut().as_mut() {
            if capture.phases.len() < 32 && !name.is_empty() {
                capture.phases.push((name.to_owned(), at_ns));
            }
        }
    });
}

/// Take everything captured and encode it as span attributes, ending the capture.
///
/// `request_duration_ns` closes the final phase: a bridge marks where a phase starts
/// and never has to close one, so the last mark runs to the end of the request.
///
/// Bodies too long for the span come back beside the attributes rather than being
/// written here: this module never touches the filesystem, and the caller is the
/// one that knows the span's identity.
pub fn drain(request_duration_ns: u128) -> Drained {
    let Some(mut capture) = CAPTURE.with(|c| c.borrow_mut().take()) else {
        return Drained {
            attributes: Vec::new(),
            bodies: Vec::new(),
        };
    };
    let config = capture.config.clone();
    let mut attributes: Vec<(String, String)> = Vec::new();
    let mut bodies: Vec<StoredBody> = Vec::new();

    if let Some(json) = encode_map_masked(&capture.request_headers, &config) {
        attributes.push((REQUEST_HEADERS.to_owned(), json));
    }
    if let Some(json) = encode_map_masked(&capture.request_cookies, &config) {
        attributes.push((REQUEST_COOKIES.to_owned(), json));
    }
    if let Some(json) = encode_map_masked(&capture.request_query, &config) {
        attributes.push((REQUEST_QUERY.to_owned(), json));
    }
    if let Some((body, size, content_type)) = capture.request_body.take() {
        let (encoded, stored) =
            body_attributes(REQUEST_BODY, "request", &body, size, &content_type, &config);
        attributes.extend(encoded);
        bodies.extend(stored);
    }

    // The bridge supplies response headers because it calls request-end before the
    // framework flushes, when `headers_list()` is still empty. Only ask PHP when it
    // did not.
    if capture.response_headers.is_empty() {
        capture.response_headers = response_headers();
    }
    if let Some(json) = encode_map_masked(&capture.response_headers, &config) {
        attributes.push((RESPONSE_HEADERS.to_owned(), json));
    }

    if capture.response_body.is_none() && config.capture_bodies && config.response_buffer {
        if let Some(body) = call_string("ob_get_contents", &[]) {
            if !body.is_empty() {
                let size = body.len();
                let content_type = capture
                    .response_headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default();
                capture.response_body = Some((body, size, content_type));
            }
        }
    }
    if let Some((body, size, content_type)) = capture.response_body.take() {
        let (encoded, stored) =
            body_attributes(RESPONSE_BODY, "response", &body, size, &content_type, &config);
        attributes.extend(encoded);
        bodies.extend(stored);
    }

    if let Some(timeline) = phase_json(&capture.phases, request_duration_ns) {
        attributes.push((TIMELINE.to_owned(), timeline));
    }

    Drained { attributes, bodies }
}

/// `$_SERVER`, as a plain map.
///
/// Copied out rather than borrowed: the superglobal is behind a global lock, and
/// holding it across the rest of request start would deadlock anything else that
/// reads globals.
pub fn server_vars() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let globals = ext_php_rs::zend::ProcessGlobals::get();
    if let Some(table) = globals.http_server_vars() {
        for (key, value) in table.iter() {
            let key = match key {
                ext_php_rs::types::ArrayKey::String(key) => key,
                ext_php_rs::types::ArrayKey::Str(key) => key.to_owned(),
                _ => continue,
            };
            if let Some(value) = value.str() {
                map.insert(key, value.to_owned());
            }
        }
    }
    map
}

/// One `$_SERVER` entry, or the empty string. Empty rather than `Option` because
/// every caller treats "absent" and "present but blank" the same way.
pub fn lookup<'a>(server: &'a HashMap<String, String>, key: &str) -> &'a str {
    server.get(key).map(String::as_str).unwrap_or("")
}

/// The inbound headers, recovered from their `HTTP_*` spellings.
fn request_headers(server: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = server
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix("HTTP_")
                .map(|name| (header_case(name), value.clone()))
        })
        .collect();
    // CONTENT_TYPE and CONTENT_LENGTH are real request headers that CGI passes
    // WITHOUT the HTTP_ prefix; dropping them would lose the two headers most often
    // needed to read the body below.
    for (cgi, header) in [
        ("CONTENT_TYPE", "Content-Type"),
        ("CONTENT_LENGTH", "Content-Length"),
    ] {
        let value = lookup(server, cgi);
        if !value.is_empty() {
            headers.push((header.to_owned(), value.to_owned()));
        }
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    headers
}

/// `X_FORWARDED_FOR` -> `X-Forwarded-For`.
fn header_case(raw: &str) -> String {
    raw.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// A cookie jar or query string as pairs. Values are URL-decoded so a reader sees
/// what the application saw, not its wire encoding.
fn parse_pairs(raw: &str, separator: char) -> Vec<(String, String)> {
    raw.split(separator)
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let name = url_decode(name.trim());
            if name.is_empty() {
                return None;
            }
            Some((name, url_decode(value)))
        })
        .collect()
}

fn url_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    // A percent-encoded body fragment is not required to be valid UTF-8; showing the
    // lossy form beats dropping the whole map because one parameter held a raw byte.
    String::from_utf8_lossy(&out).into_owned()
}

/// The response headers PHP is about to send, via `headers_list()`.
fn response_headers() -> Vec<(String, String)> {
    let Some(raw) = call_array("headers_list") else {
        return Vec::new();
    };
    raw.iter()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// Encode pairs as the JSON object the desktop parses, masking as configured.
/// `None` when there is nothing to say, so an empty map never becomes an attribute.
pub fn encode_header_map(
    pairs: Vec<(String, String)>,
    config: &HttpCaptureConfig,
) -> Option<String> {
    encode_map_masked(&pairs, config)
}

fn encode_map_masked(pairs: &[(String, String)], config: &HttpCaptureConfig) -> Option<String> {
    if pairs.is_empty() {
        return None;
    }
    let mut map = Map::new();
    let mut bytes = 0usize;
    let mut dropped = 0usize;
    for (name, value) in pairs {
        if map.len() >= MAX_MAP_ENTRIES || bytes >= MAX_MAP_BYTES {
            dropped += 1;
            continue;
        }
        let value = if config.redact && redact_json(name, config) {
            MASK.to_owned()
        } else {
            truncate(value, MAX_VALUE_BYTES)
        };
        bytes += name.len() + value.len();
        map.insert(name.clone(), Value::String(value));
    }
    if dropped > 0 {
        map.insert(
            DROPPED_KEY.to_owned(),
            Value::String(format!("{dropped} more not captured")),
        );
    }
    serde_json::to_string(&Value::Object(map)).ok()
}

/// Whether this key names something that must be masked.
fn redact_json(name: &str, config: &HttpCaptureConfig) -> bool {
    let lowered = name.to_lowercase();
    config
        .redact_patterns
        .iter()
        .any(|pattern| lowered.contains(pattern.as_str()))
}

/// The redaction pattern list, resolved once per process.
///
/// Split out from [`HttpCaptureConfig`] because redaction is not an HTTP concern: the
/// deterministic profiler's Tier 3 argument capture needs the same verdict about a
/// PARAMETER name, and threading a whole capture config through the observer's hot path
/// to ask one question would couple two unrelated features. The patterns themselves stay
/// where they are — one list, additive over `CHRONOS_PHP_REDACT_PATTERNS`, so an
/// operator naming one more sensitive word covers both signals at once.
fn redaction_patterns() -> &'static Vec<String> {
    static PATTERNS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        let mut patterns: Vec<String> = DEFAULT_REDACT_PATTERNS
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect();
        // Additive, not a replacement: an operator naming one more sensitive header
        // means "also this", and reading it as "only this" would silently unmask
        // Authorization on every service that set it.
        patterns.extend(
            crate::settings::get("CHRONOS_PHP_REDACT_PATTERNS")
                .unwrap_or_default()
                .split(',')
                .map(|pattern| pattern.trim().to_lowercase())
                .filter(|pattern| !pattern.is_empty()),
        );
        patterns
    })
}

/// Whether an identifier (a header name, a query key, a PHP parameter name) names
/// something that must be masked. Never call this to decide whether to CAPTURE — it
/// decides whether to MASK, and the two answers differ: "argument 2 was a redacted
/// string" is evidence, while a silently omitted argument reads as absent.
#[must_use]
pub fn redacts_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let lowered = name.to_lowercase();
    redaction_patterns()
        .iter()
        .any(|pattern| lowered.contains(pattern.as_str()))
}

/// The body preview plus the siblings that keep a bounded payload from reading as a
/// complete one, and the whole body when it did not fit.
///
/// `.stored` is the load-bearing addition: it is how a reader can tell "this is
/// all there was" from "there is more, and it is retrievable". A `.truncated`
/// without it still means what it always did — the rest is gone.
fn body_attributes(
    key: &str,
    side: &'static str,
    body: &str,
    size: usize,
    content_type: &str,
    config: &HttpCaptureConfig,
) -> (Vec<(String, String)>, Option<StoredBody>) {
    let captured = truncate(body, config.max_body_bytes);
    let overflowed = captured.len() < body.len();
    let truncated = overflowed || size > body.len();
    let mut attributes = vec![
        (key.to_owned(), captured),
        (format!("{key}.size"), size.to_string()),
    ];
    if truncated {
        attributes.push((format!("{key}.truncated"), "true".to_owned()));
    }
    if !content_type.is_empty() {
        attributes.push((format!("{key}.content_type"), content_type.to_owned()));
    }
    if !overflowed || config.max_body_total_bytes <= config.max_body_bytes {
        return (attributes, None);
    }
    attributes.push((format!("{key}.stored"), "true".to_owned()));
    (
        attributes,
        Some(StoredBody {
            side,
            content_type: content_type.to_owned(),
            bytes: body.to_owned(),
        }),
    )
}

/// `body_attributes` for callers outside a request capture (outbound curl).
///
/// Attributes only, never a stored body: the whole-body path is keyed by the span
/// that carries the preview, and an outbound call's body belongs to a client span
/// this function's caller has not created yet. Previewing it is the same; storing
/// it would need an identity that does not exist here.
pub fn encode_body(
    key: &str,
    body: &str,
    size: usize,
    content_type: &str,
    config: &HttpCaptureConfig,
) -> Vec<(String, String)> {
    body_attributes(key, "response", body, size, content_type, config).0
}

/// Truncate to at most `limit` bytes without splitting a UTF-8 character.
fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// The marked phases as the JSON array the Timeline tab draws.
///
/// Marks name the phase that BEGINS, so each runs until the next one starts and the
/// last runs to the end of the request. Everything before the first mark is
/// `bootstrap`: it is real time the request spent, and leaving it as a gap would read
/// as missing data rather than as the answer.
fn phase_json(phases: &[(String, u128)], request_duration_ns: u128) -> Option<String> {
    if phases.is_empty() {
        return None;
    }
    let mut marks: Vec<(String, u128)> = Vec::with_capacity(phases.len() + 1);
    if phases[0].1 > 0 {
        marks.push(("bootstrap".to_owned(), 0));
    }
    marks.extend(phases.iter().cloned());

    let mut encoded: Vec<Value> = Vec::new();
    for (index, (name, start)) in marks.iter().enumerate() {
        let end = marks
            .get(index + 1)
            .map(|(_, next)| *next)
            .unwrap_or(request_duration_ns);
        if end <= *start {
            continue;
        }
        encoded.push(serde_json::json!({
            "name": name,
            "startNs": start.to_string(),
            "endNs": end.to_string(),
        }));
    }
    if encoded.is_empty() {
        return None;
    }
    serde_json::to_string(&Value::Array(encoded)).ok()
}

/// The same encoding for the curl timings the observer derives, which arrive as
/// explicit (start, end) pairs rather than as marks.
pub fn encode_phases(phases: &[(String, u128, u128)]) -> String {
    let encoded: Vec<Value> = phases
        .iter()
        .map(|(name, start, end)| {
            serde_json::json!({
                "name": name,
                "startNs": start.to_string(),
                "endNs": end.to_string(),
            })
        })
        .collect();
    serde_json::to_string(&Value::Array(encoded)).unwrap_or_else(|_| "[]".to_owned())
}

/// `php://input`, read through PHP itself so the stream wrapper — and any
/// SAPI-specific rewind behaviour — works exactly as it does for the application.
fn read_input_stream() -> Option<String> {
    let body = call_string("file_get_contents", &["php://input"])?;
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn call_string(name: &str, args: &[&str]) -> Option<String> {
    let function = ext_php_rs::zend::Function::try_from_function(name)?;
    let params: Vec<&dyn ext_php_rs::convert::IntoZvalDyn> = args
        .iter()
        .map(|arg| arg as &dyn ext_php_rs::convert::IntoZvalDyn)
        .collect();
    function.try_call(params).ok()?.string()
}

fn call_array(name: &str) -> Option<Vec<String>> {
    let function = ext_php_rs::zend::Function::try_from_function(name)?;
    let result = function.try_call(vec![]).ok()?;
    let table = result.array()?;
    Some(
        table
            .iter()
            .filter_map(|(_, value)| value.str().map(str::to_owned))
            .collect(),
    )
}

#[allow(dead_code)]
fn call_void(name: &str) {
    if let Some(function) = ext_php_rs::zend::Function::try_from_function(name) {
        let _ = function.try_call(vec![]);
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    crate::settings::get(name)
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
