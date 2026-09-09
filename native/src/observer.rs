//! Zend fcall observer with native traceparent injection.
//!
//! MINIT registers `chronos_observer_factory` with `zend_observer_fcall_register`; Zend
//! calls the factory once per observed function and, on each invocation, the returned
//! `begin`/`end` handlers capture spans.
//!
//! SPAN POLICY — the observer sees every call but is deliberately selective about what
//! becomes a span (exhaustive per-call data is the PROFILER's job; framework call trees
//! belong to the continuous profiler, not the trace waterfall):
//!   * Userland-defined functions emit spans ONLY when explicitly allowlisted via the
//!     application's instrumentation manifest (`chronos_trace_function` FFI, fed by
//!     `Chronos\trace_method`). Those spans carry `chronos.instrumented=manifest` and
//!     bypass the min-duration filter. CHRONOS_PHP_SPAN_ALL_USERLAND=1 restores the old
//!     span-every-userland-call behaviour as an escape hatch.
//!     NOTE: the Zend observer caches the factory verdict per function, and a fresh
//!     worker's factory can run before the manifest registers anything — so the factory
//!     attaches handlers to ALL userland functions and the BEGIN handler decides per
//!     call, downgrading unlisted calls to a no-span ObserveOnly frame (which still
//!     pairs with the end handler so the frame stack never desyncs).
//!   * Known I/O internals emit spans WITH payload detail: Redis/Memcached (cache.key),
//!     PDO/mysqli (db.statement), curl_exec (http.url) — span.kind=client.
//!   * Non-deterministic builtins (time/rand/getenv) and curl_setopt are observed for
//!     DST recording / header tracking but never emit spans.
//!   * Leaf spans shorter than CHRONOS_PHP_SPAN_MIN_DURATION_US (default 100µs) are
//!     dropped unless they carry attributes, errored, or kept a child.
//!
//! For outbound curl calls the begin handler injects a W3C `traceparent` header carrying
//! the child span's ID so downstream Chronos-instrumented services link their traces to
//! the caller. Header injection is merge-safe: `curl_setopt($ch, CURLOPT_HTTPHEADER, ...)`
//! calls are observed and the per-handle header list tracked, so injection appends to the
//! application's own headers instead of clobbering them.
//!
//! The begin handler also consumes pending profiler ticks (see `sampler`).

use crate::context::{hex_bytes, TraceContext};
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub struct NativeSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub started_at: String,
    pub ended_at: String,
    pub status: String,
    pub duration_nanoseconds: u128,
    /// Extra span attributes beyond the always-present duration (http.*, error.*, …).
    pub attributes: Vec<(String, String)>,
}

thread_local! {
    static REQUEST_SPANS: RefCell<Vec<NativeSpan>> = const { RefCell::new(Vec::new()) };
    static REQUEST_CONTEXT: RefCell<Option<TraceContext>> = const { RefCell::new(None) };
    static SPAN_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub struct CallFrame {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    /// The canonical function identity, INTERNED. An `Rc<str>` rather than a `String`
    /// because this used to be the second of two heap allocations every observed call
    /// paid before anything was decided (see `deterministic::NameInterner`); cloning the
    /// interned handle is a refcount bump and the allocation is gone from the hot path.
    name: std::rc::Rc<str>,
    started_at: String,
    /// The SPAN clock. Zero on an `ObserveOnly` frame, deliberately and still: this
    /// field feeds `on_end`'s duration and min-duration logic, and an `ObserveOnly`
    /// frame must never become a span. The deterministic clock is `timing` below —
    /// two fields, two purposes, no possibility of confusing them.
    start_hrtime: u128,
    /// If this call is a network function, the traceparent we injected.
    injected_traceparent: Option<String>,
    /// Whether this frame may become a span at all (ObserveOnly frames never do).
    emit_span: bool,
    /// Whether this frame was pushed onto the parenting stack.
    on_stack: bool,
    /// A kept child forces this span to be kept so the tree stays connected.
    kept_child: bool,
    /// Payload detail captured at call begin (cache.key, db.statement, http.url, …).
    attributes: Vec<(String, String)>,
    /// A known I/O call, so its measured duration also becomes an I/O profile sample.
    io: bool,
    /// The DETERMINISTIC clock and this frame's child-time accumulator (ADR 0029).
    /// Carried on the observer's own frame stack so the aggregate never maintains a
    /// parallel stack of its own — which is what would desync it on the requests where
    /// the `ObserveOnly` push is skipped.
    timing: crate::deterministic::FrameTiming,
}

impl CallFrame {
    /// A minimal no-span frame that exists only to pair with the end trampoline's pop
    /// (and to carry the name for DST result recording). Skips span-id generation,
    /// wall-clock formatting, and parent lookup — this runs for every unlisted
    /// userland call, so it must stay cheap.
    fn observe_only(name: std::rc::Rc<str>) -> Self {
        CallFrame {
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: None,
            name,
            started_at: String::new(),
            start_hrtime: 0,
            injected_traceparent: None,
            emit_span: false,
            on_stack: false,
            kept_child: false,
            attributes: Vec::new(),
            io: false,
            // Stamped by `push_frame`, which is the only thing allowed to create a
            // counted frame. See its doc comment for why it cannot be stamped here.
            timing: crate::deterministic::FrameTiming::default(),
        }
    }
}

/// What the observer does with an observed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpanPolicy {
    /// A userland application function: span with no payload capture. Only reachable
    /// via the CHRONOS_PHP_SPAN_ALL_USERLAND escape hatch.
    UserSpan,
    /// A function/method the application's instrumentation manifest allowlisted
    /// (`chronos_trace_function`): span tagged `chronos.instrumented=manifest`,
    /// exempt from the min-duration filter.
    ManifestSpan,
    /// A known I/O call: span + payload attributes + span.kind=client.
    IoSpan,
    /// Observed for side channels (DST recording, curl header tracking, begin/end
    /// pairing for unlisted userland calls) — no span.
    ObserveOnly,
}

/// Per-PROCESS allowlist of userland functions the instrumentation manifest traced.
/// Grows monotonically over the worker's life (manifests only ever register), so a
/// per-process set is correct: registrations from request 1 stay valid for request N.
fn traced_functions() -> &'static std::sync::RwLock<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::RwLock::new(std::collections::HashSet::new()))
}

/// Called over the `chronos_trace_function` FFI. Names use the observer's qualified
/// format: `Class::method` or a plain `function_name`.
pub fn trace_function(name: &str) {
    let name = name.trim().trim_start_matches('\\');
    if name.is_empty() {
        return;
    }
    if let Ok(mut set) = traced_functions().write() {
        set.insert(name.to_owned());
    }
    // Invalidate every cached policy verdict. `observe_policy` is otherwise pure in the
    // inputs the interner already holds, and the manifest is its ONE mutable input —
    // so bumping a generation here is what makes caching the verdict exactly equivalent
    // to recomputing it, rather than a behaviour change that only shows up on a worker
    // whose manifest registered after its first call to the function.
    MANIFEST_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Bumped by [`trace_function`]. Monotonic per process, and only ever advanced during
/// manifest registration — so in steady state every cached verdict is a hit.
static MANIFEST_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(feature = "zend-observer")]
fn manifest_generation() -> u64 {
    MANIFEST_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// PHP identifiers are case-insensitive, so fall back to a linear case-insensitive
/// scan when the exact-case lookup misses (the set is tiny — a handful of entries).
fn is_traced(name: &str) -> bool {
    traced_functions()
        .read()
        .map(|set| {
            !set.is_empty()
                && (set.contains(name) || set.iter().any(|t| t.eq_ignore_ascii_case(name)))
        })
        .unwrap_or(false)
}

/// Escape hatch restoring the pre-allowlist behaviour of spanning every userland call.
fn span_all_userland() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| crate::settings::flag("CHRONOS_PHP_SPAN_ALL_USERLAND", false))
}

thread_local! {
    /// Native I/O span suppression, set per request by the userland SDK when its
    /// own richer instrumentation is active (Doctrine listener, DB::listen, cache
    /// hooks). The userland span carries db.host/db.name/bound params that the
    /// .so cannot see, so keeping both layers would double-count the same query
    /// and split the service map into a "host not reported" ghost node.
    static SUPPRESS_SQL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SUPPRESS_CACHE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Called over the `chronos_suppress_native` FFI. Unknown kinds are ignored.
pub fn suppress_native(kind: &str) {
    match kind {
        "sql" => SUPPRESS_SQL.with(|flag| flag.set(true)),
        "cache" => SUPPRESS_CACHE.with(|flag| flag.set(true)),
        _ => {}
    }
}

fn native_io_suppressed(name: &str) -> bool {
    if sql_io_function(name) {
        return SUPPRESS_SQL.with(std::cell::Cell::get);
    }
    if cache_io_method(name) {
        return SUPPRESS_CACHE.with(std::cell::Cell::get);
    }
    false
}

pub fn set_request_context(context: TraceContext) {
    REQUEST_CONTEXT.with(|ctx| *ctx.borrow_mut() = Some(context));
    SPAN_STACK.with(|stack| stack.borrow_mut().clear());
    REQUEST_SPANS.with(|spans| spans.borrow_mut().clear());
    SUPPRESS_SQL.with(|flag| flag.set(false));
    SUPPRESS_CACHE.with(|flag| flag.set(false));
    #[cfg(feature = "zend-observer")]
    CURL_HEADERS.with(|h| h.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    PREPARED_STATEMENTS.with(|m| m.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    LAST_THROW.with(|t| *t.borrow_mut() = None);
}

/// Update the propagation headers on the observer's own copy of the request
/// context. Exists because `enrich_request` (lib.rs) can learn a tracestate /
/// baggage the native start could not see, AFTER `set_request_context` already
/// cloned the context in here — and re-calling `set_request_context` would clear
/// every span recorded during framework bootstrap. Touches ONLY the propagation
/// fields; the trace identity stays whatever the request started with.
pub fn set_propagation(tracestate: Option<String>, baggage: Option<String>) {
    REQUEST_CONTEXT.with(|ctx| {
        if let Some(context) = ctx.borrow_mut().as_mut() {
            context.tracestate = tracestate;
            context.baggage = baggage;
        }
    });
}

pub fn clear_request_context() {
    REQUEST_CONTEXT.with(|ctx| *ctx.borrow_mut() = None);
    SPAN_STACK.with(|stack| stack.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    CURL_HEADERS.with(|h| h.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    PREPARED_STATEMENTS.with(|m| m.borrow_mut().clear());
}

fn on_begin(
    context: &TraceContext,
    function_name: &std::rc::Rc<str>,
    policy: SpanPolicy,
) -> CallFrame {
    let span_id = hex_bytes(8);
    let emit_span = policy != SpanPolicy::ObserveOnly;
    let parent_span_id = SPAN_STACK
        .with(|stack| {
            let stack = stack.borrow();
            stack.last().cloned()
        })
        .or_else(|| Some(context.span_id.clone()));

    if emit_span {
        SPAN_STACK.with(|stack| stack.borrow_mut().push(span_id.clone()));
    }

    let traceparent = if is_network_function(function_name) {
        Some(format!(
            "00-{}-{}-{}",
            context.trace_id,
            span_id,
            if context.sampled { "01" } else { "00" }
        ))
    } else {
        None
    };

    CallFrame {
        trace_id: context.trace_id.clone(),
        span_id,
        parent_span_id,
        name: function_name.clone(),
        started_at: now_utc(),
        start_hrtime: monotonic_nanos(),
        injected_traceparent: traceparent,
        emit_span,
        on_stack: emit_span,
        kept_child: false,
        attributes: Vec::new(),
        io: policy == SpanPolicy::IoSpan,
        // Stamped by `push_frame`. See its doc comment.
        timing: crate::deterministic::FrameTiming::default(),
    }
}

/// Minimum leaf-span duration in nanoseconds (CHRONOS_PHP_SPAN_MIN_DURATION_US,
/// default 100µs). Spans with payload attributes, errors, or kept children are
/// always kept regardless of duration.
fn min_span_duration_nanos() -> u128 {
    static MIN_NANOS: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *MIN_NANOS.get_or_init(|| {
        crate::settings::get("CHRONOS_PHP_SPAN_MIN_DURATION_US")
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(100)
            * 1_000
    })
}

/// Finish a frame; returns true when a span was emitted (so the caller can mark
/// the parent frame's `kept_child`).
fn on_end(frame: CallFrame, threw: bool) -> bool {
    if frame.on_stack {
        SPAN_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.last().map(|s| s == &frame.span_id).unwrap_or(false) {
                stack.pop();
            }
        });
    }
    if !frame.emit_span {
        return false;
    }

    let duration = monotonic_nanos().saturating_sub(frame.start_hrtime);
    // I/O profile samples are independent of whether the SPAN survives the
    // min-duration filter: a wait long enough to profile is recorded either way, and
    // the sampler applies its own (higher) threshold.
    if frame.io {
        crate::sampler::record_io_wait(duration, &frame.name);
    }
    let keep = threw
        || frame.kept_child
        || !frame.attributes.is_empty()
        || duration >= min_span_duration_nanos();
    if !keep {
        return false;
    }

    let span = NativeSpan {
        trace_id: frame.trace_id,
        span_id: frame.span_id,
        parent_span_id: frame.parent_span_id,
        // The one place the interned identity is copied into an owned String: a span
        // that actually survives the keep rule. Rare by construction, unlike the
        // per-call allocation this replaced.
        name: frame.name.to_string(),
        started_at: frame.started_at,
        ended_at: now_utc(),
        status: if threw { "error".into() } else { "ok".into() },
        duration_nanoseconds: duration,
        attributes: frame.attributes,
    };
    push_span(span)
}

fn push_span(span: NativeSpan) -> bool {
    REQUEST_SPANS.with(|spans| {
        let mut spans = spans.borrow_mut();
        if spans.len() < MAX_SPANS_PER_REQUEST {
            spans.push(span);
            true
        } else {
            false
        }
    })
}

/// Append a userland-recorded span (SpanManager / Doctrine listeners bridged over the
/// `chronos_record_span` FFI) into the same batch the observer spans flush from.
#[allow(clippy::too_many_arguments)]
pub fn record_userland_span(
    trace_id: String,
    span_id: String,
    parent_span_id: String,
    name: String,
    started_at: String,
    ended_at: String,
    status: String,
    attributes: Vec<(String, String)>,
) {
    let parent = if parent_span_id.is_empty() {
        REQUEST_CONTEXT.with(|ctx| ctx.borrow().as_ref().map(|c| c.span_id.clone()))
    } else {
        Some(parent_span_id)
    };
    let trace_id = if trace_id.is_empty() {
        REQUEST_CONTEXT
            .with(|ctx| ctx.borrow().as_ref().map(|c| c.trace_id.clone()))
            .unwrap_or_default()
    } else {
        trace_id
    };
    push_span(NativeSpan {
        trace_id,
        span_id,
        parent_span_id: parent,
        name,
        started_at,
        ended_at,
        status,
        duration_nanoseconds: 0,
        attributes,
    });
}

pub fn drain() -> Vec<NativeSpan> {
    REQUEST_SPANS.with(|spans| std::mem::take(&mut *spans.borrow_mut()))
}

/// The request root span. Always emitted at request end so every observer/userland span
/// has an in-batch ancestor and the request itself carries the HTTP identity attributes.
#[allow(clippy::too_many_arguments)]
pub fn root_http_span(
    context: &TraceContext,
    name: &str,
    started_at: String,
    http_status_code: i64,
    request_start_ns: u128,
    request_end_ns: u128,
    status: &str,
    attributes: Vec<(String, String)>,
) -> NativeSpan {
    NativeSpan {
        trace_id: context.trace_id.clone(),
        span_id: context.span_id.clone(),
        parent_span_id: context.parent_span_id.clone(),
        name: if name.is_empty() {
            "request".to_owned()
        } else {
            name.to_owned()
        },
        started_at,
        ended_at: now_utc(),
        status: if !status.is_empty() {
            status.to_owned()
        } else if http_status_code == 0 || (100..500).contains(&http_status_code) {
            "ok".into()
        } else {
            "error".into()
        },
        duration_nanoseconds: request_end_ns.saturating_sub(request_start_ns),
        attributes,
    }
}

/// Functions that make outbound network calls and need traceparent injection.
const NETWORK_FUNCTIONS: &[&str] = &["curl_exec", "curl_multi_exec", "file_get_contents"];

fn is_network_function(name: &str) -> bool {
    NETWORK_FUNCTIONS.contains(&name)
}

/// Stream functions that are network calls only SOMETIMES. `file_get_contents` is the
/// one that matters: the same builtin fetches an HTTP URL and reads a local template,
/// and the overwhelmingly common case is the local read. Spanning both put dozens of
/// 0 ms `file_get_contents` rows into every trace, which is noise that buries the
/// request's real work — so the target decides, per call, not the function name.
const DUAL_PURPOSE_STREAM_FUNCTIONS: &[&str] = &["file_get_contents"];

fn is_dual_purpose_stream_function(name: &str) -> bool {
    DUAL_PURPOSE_STREAM_FUNCTIONS.contains(&name)
}

/// Stream wrappers that cross the network. Deliberately an allow-list: `php://`,
/// `data://`, `compress.*://`, a bare relative path and everything else unnamed here
/// are local, and a wrapper nobody listed should read as local rather than quietly
/// producing a client span for a file read.
const REMOTE_STREAM_SCHEMES: &[&str] = &[
    "http://", "https://", "ftp://", "ftps://", "sftp://", "ssh2.",
];

/// Whether a stream target actually goes over the network. Scheme comparison is
/// case-insensitive because PHP's wrapper lookup is.
fn is_remote_stream_target(target: &str) -> bool {
    let target = target.trim_start();
    REMOTE_STREAM_SCHEMES.iter().any(|scheme| {
        target.len() >= scheme.len() && target[..scheme.len()].eq_ignore_ascii_case(scheme)
    })
}

/// SQL calls that ARE the I/O. Method-scoped, not class-scoped: PDO/PDOStatement
/// expose dozens of cursor/metadata methods (setAttribute, fetch, bindParam,
/// closeCursor, …) that are per-row bookkeeping, not I/O — spanning them buried
/// real queries under hundreds of 0ms rows.
const SQL_IO_FUNCTIONS: &[&str] = &[
    "PDO::query",
    "PDO::exec",
    "PDO::prepare",
    "PDOStatement::execute",
    "mysqli::query",
    "mysqli::real_query",
    "mysqli::execute_query",
    "mysqli_query",
    // Prepared-statement execution IS the round trip — `mysqli::prepare` /
    // `SQLite3::prepare` only compile the statement. The SQL text is not an
    // argument at execute time, so it is captured at prepare time into
    // PREPARED_STATEMENTS (keyed by the statement object's handle, same
    // per-handle-map pattern as CURL_HEADERS) and stamped onto this span.
    "mysqli_stmt::execute",
    "SQLite3::query",
    "SQLite3::exec",
    "SQLite3::querySingle",
    "SQLite3Stmt::execute",
];

/// The prepare calls whose RESULT is a statement object worth remembering.
/// `PDO::prepare` is deliberately absent: it already emits its own `IoSpan`
/// carrying `db.statement`, and userland PDO instrumentation (Doctrine,
/// DB::listen) owns the richer capture there. Procedural `mysqli_prepare`
/// belongs here too (same returned-statement shape, SQL at arg 1 after the
/// link) — see `prepare_sql_arg_index`.
const SQL_PREPARE_METHODS: &[&str] = &["mysqli::prepare", "SQLite3::prepare", "mysqli_prepare"];

/// The prepare calls that mutate the statement object they are CALLED ON
/// (`$db->stmt_init()` + `$stmt->prepare($sql)`, `new mysqli_stmt($link, $sql)`)
/// rather than returning a fresh one. Observed for the same side channel as
/// SQL_PREPARE_METHODS, with one extra duty the retval-keyed path never has:
/// EVICTION. The Zend engine recycles object handles within a request, so a
/// statement created through one of these calls can inherit the handle id of a
/// freed statement whose SQL is still banked — and a later `::execute` would
/// then be stamped with the OLD query's text. Every observed call here therefore
/// either re-banks the handle with its real SQL or evicts it (failed prepare,
/// unreadable argument): a wrong query on a span is worse than no query.
const SQL_THIS_PREPARE_METHODS: &[&str] = &["mysqli_stmt::prepare", "mysqli_stmt::__construct"];

fn sql_prepare_method(name: &str) -> bool {
    SQL_PREPARE_METHODS.contains(&name)
}

fn sql_this_prepare_method(name: &str) -> bool {
    SQL_THIS_PREPARE_METHODS.contains(&name)
}

/// Which argument carries the SQL text for a given prepare call. The method
/// forms take it first; the procedural forms and the constructor take the
/// connection/link first and the SQL second.
fn prepare_sql_arg_index(name: &str) -> usize {
    match name {
        "mysqli_prepare" | "mysqli_stmt::__construct" => 1,
        _ => 0,
    }
}

/// Cache client classes whose DATA methods are I/O spans. Lifecycle methods
/// (construct/connect/auth/…) are excluded from observation entirely.
const CACHE_CLASS_PREFIXES: &[&str] = &["Redis::", "RedisCluster::", "Memcached::"];

const LIFECYCLE_METHODS: &[&str] = &[
    "__construct",
    "__destruct",
    "connect",
    "pconnect",
    "open",
    "auth",
    "select",
    "close",
    "setOption",
    "getOption",
    "addServer",
    "addServers",
    "quit",
];

fn sql_io_function(name: &str) -> bool {
    SQL_IO_FUNCTIONS.contains(&name)
}

fn cache_io_method(name: &str) -> bool {
    CACHE_CLASS_PREFIXES.iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|method| !LIFECYCLE_METHODS.contains(&method))
    })
}

/// Non-deterministic builtins whose results a DST recording captures so a replay can
/// substitute them. Names are lowercase (internal functions report lowercase).
fn dst_event_kind_for(name: &str) -> Option<crate::dst_spool::DstEventKind> {
    use crate::dst_spool::DstEventKind as K;
    Some(match name {
        "time" | "microtime" | "hrtime" | "date" | "mktime" | "gmdate" => K::Time,
        "rand" | "mt_rand" | "random_int" | "random_bytes" | "uniqid" | "mt_srand" => K::Random,
        "getenv" => K::EnvRead,
        _ => return None,
    })
}

/// Per-request span collection ceiling. This bounds MEMORY, not the wire: the
/// flush splits spans into byte-bounded spool files (see lib.rs), so a deep
/// request never hits ingest's 1 MiB per-batch body limit. Effectively
/// unbounded for real requests — a runaway-loop backstop, not a budget:
/// 32k spans × ~1KB worst-case buffered ≈ 32 MB transient per request.
const MAX_SPANS_PER_REQUEST: usize = 32_768;

/// Userland namespaces that are infrastructure, not application logic. Their
/// calls never become spans (the profiler still sees them).
const SKIP_PREFIXES: &[&str] = &[
    "Composer\\Autoload\\",
    "Chronos\\Collector\\",
    "sfAutoload::",
    "sfCoreAutoload::",
    "sfSimpleAutoload::",
    "Illuminate\\Container\\",
    "Illuminate\\Support\\",
    "Illuminate\\Events\\",
    "Illuminate\\Pipeline\\Pipeline::carry",
    "Illuminate\\Foundation\\Application::isDeferredService",
    "Illuminate\\Foundation\\Application::bound",
];

/// Path fragments whose code is not this application's. A userland span defined in a
/// file matching any of them is dropped: dependency internals are the profiler's
/// territory, where they are attributed without flooding the trace waterfall.
///
/// Defaults cover the PHP and JS dependency trees; override per service with
/// `CHRONOS_PHP_EXCLUDE_PATHS` (comma-separated fragments, matched as substrings of the
/// defining file path). An explicitly EMPTY value means "exclude nothing" — a service
/// that really wants to trace inside its dependencies can say so.
const DEFAULT_EXCLUDED_PATHS: &[&str] = &["/vendor/", "/node_modules/", "/cache/", "/.git/"];

fn excluded_paths() -> &'static Vec<String> {
    static PATHS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    PATHS.get_or_init(|| match crate::settings::get("CHRONOS_PHP_EXCLUDE_PATHS") {
        Some(list) => parse_excluded_paths(&list),
        None => DEFAULT_EXCLUDED_PATHS
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    })
}

/// Parse the env list. Blank entries are dropped rather than becoming a fragment that
/// matches every path — one stray comma should not silence a service's whole trace.
fn parse_excluded_paths(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a defining file belongs to code the service does not own. An UNKNOWN file
/// (internal function, eval'd code, closure with no op array) is never excluded — the
/// rule drops what it can positively identify as a dependency, and a missing path is
/// not evidence.
fn is_excluded_path(file: &str, excluded: &[String]) -> bool {
    if file.is_empty() {
        return false;
    }
    excluded
        .iter()
        .any(|fragment| file.contains(fragment.as_str()))
}

/// Decide what to do with an observed call. `is_internal` is true for engine
/// builtins (strpos, curl_*, Redis::get, …), false for userland-defined code.
///
/// CACHING CAVEAT: the Zend engine caches the observer factory's verdict per function,
/// and on a fresh worker the factory can run before the instrumentation manifest has
/// registered anything. This function therefore never returns `None` for a plain
/// userland function — the factory attaches handlers, and the BEGIN handler re-runs
/// this per call so a later `chronos_trace_function` registration takes effect. An
/// unlisted userland call gets `ObserveOnly`: a paired begin/end frame, no span.
fn observe_policy(name: &str, is_internal: bool) -> Option<SpanPolicy> {
    if name.is_empty() || name.starts_with("chronos_") {
        return None;
    }
    if name == "curl_setopt" || name == "curl_setopt_array" {
        return Some(SpanPolicy::ObserveOnly);
    }
    // Observed for the side channel only, like curl_setopt above: the prepare
    // call is where the SQL text is last visible, so its end handler banks the
    // text against the statement object (returned, or `$this` for the mutating
    // forms) for the later `::execute` span. No span of its own — the wire
    // round trip that matters is the execute.
    if sql_prepare_method(name) || sql_this_prepare_method(name) {
        return Some(SpanPolicy::ObserveOnly);
    }
    if dst_event_kind_for(name).is_some() {
        return Some(SpanPolicy::ObserveOnly);
    }
    if is_network_function(name) || sql_io_function(name) || cache_io_method(name) {
        return Some(SpanPolicy::IoSpan);
    }
    if is_internal {
        // Engine builtins (strpos, substr, file_exists, …) are profiler
        // territory — never spans.
        return None;
    }
    // Closures/{main} can never be named by a manifest — safe to cache a hard no.
    if name == "{main}" || name.starts_with("{closure") || name.contains("\\{closure") {
        return None;
    }
    if is_traced(name) {
        return Some(SpanPolicy::ManifestSpan);
    }
    if !span_all_userland() {
        // Framework call trees are the profiler's job — no span, but keep the
        // frame paired so a per-process allowlist registration can kick in later.
        return Some(SpanPolicy::ObserveOnly);
    }
    if SKIP_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return None;
    }
    // Class autoloaders are infrastructure whatever the framework calls them —
    // their cost is compile time, which the profiler attributes correctly.
    if name.ends_with("::autoload") || name.ends_with("::loadClass") {
        return None;
    }
    Some(SpanPolicy::UserSpan)
}

/// Whether MINIT actually registered the observer. False when the collector was
/// explicitly switched off at module startup, and false in a build without the
/// `zend-observer` feature.
static OBSERVER_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether this process has a live Zend observer. The heartbeat reports it, because
/// a process that later resolves `enabled` to true while this is false will produce
/// no observer spans and nothing else would say why.
pub fn installed() -> bool {
    OBSERVER_INSTALLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Register the fcall observer, unless the collector is explicitly off.
///
/// Registration is MINIT-only — `zend_observer_fcall_register` cannot be called
/// later — and it is not free: an installed observer costs roughly 30ns on EVERY
/// userland call, whether or not a request is being collected, because the factory
/// and the paired begin/end handlers run regardless. That is the price of the
/// feature when the collector is on, and pure waste when it is off, which is the
/// case for an image that bakes the .so in and disables it.
///
/// Only an EXPLICIT off skips it (`CHRONOS_PHP_ENABLED` / `chronos.enabled` in the
/// process environment or php.ini). An absent setting installs as before, because a
/// value this early cannot see per-request configuration — see `settings::startup_flag`.
/// The consequence is worth stating plainly: switching the collector off at module
/// startup and back on per-request leaves the process without an observer for its
/// whole life. `heartbeat()` says so out loud when that happens.
pub fn install_observer() {
    if !crate::settings::startup_flag("CHRONOS_PHP_ENABLED", true) {
        return;
    }
    #[cfg(feature = "zend-observer")]
    unsafe {
        zend_observer_fcall_register(Some(chronos_observer_factory));
        // Chain rather than clobber: another extension (or a future chronos component)
        // may already have set this single global slot, and there is no registration
        // list to append to — only one function pointer.
        let previous = ext_php_rs::ffi::zend_throw_exception_hook;
        let _ = PREVIOUS_THROW_HOOK.set(previous);
        ext_php_rs::ffi::zend_throw_exception_hook = Some(chronos_throw_trampoline);
        OBSERVER_INSTALLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(not(feature = "zend-observer"))]
    {}
}

#[cfg(feature = "zend-observer")]
static PREVIOUS_THROW_HOOK: std::sync::OnceLock<
    Option<unsafe extern "C" fn(ex: *mut ext_php_rs::ffi::zend_object)>,
> = std::sync::OnceLock::new();

/// The most recent throw of the request, bounded like the root span's own error caps
/// (`lib.rs` caps error.type at 256 and error.message at 2048 — same numbers here, so
/// what shutdown stamps can never exceed what a bridge could have stamped).
#[derive(Clone, Debug)]
pub struct LastThrow {
    pub class: String,
    pub message: String,
    pub file: String,
    pub line: String,
}

#[cfg(feature = "zend-observer")]
thread_local! {
    /// Set by the throw hook on EVERY throw while a request is open, overwritten by
    /// later throws, cleared at request start. At RSHUTDOWN this is the only witness
    /// left of an exception that escaped a frameworkless script — the object itself
    /// is gone by then. See `lib.rs::uncaught_exception_at_shutdown` for how it is
    /// matched against `error_get_last()` before anything is stamped.
    static LAST_THROW: RefCell<Option<LastThrow>> = const { RefCell::new(None) };
}

/// The request's most recent recorded throw, if any. `None` without the observer
/// feature — there is no throw hook to have seen one.
pub fn last_throw() -> Option<LastThrow> {
    #[cfg(feature = "zend-observer")]
    {
        LAST_THROW.with(|t| t.borrow().clone())
    }
    #[cfg(not(feature = "zend-observer"))]
    {
        None
    }
}

/// Truncate on a char boundary — same shape as `lib.rs::cap`, local so the throw
/// hook does not reach across modules for a three-line helper.
#[cfg(feature = "zend-observer")]
fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Native throw-time exception capture (debug-extension direction P5, gated on this native
/// `.so` existing — see the phasing doc). `zend_throw_exception_hook` fires once per throw,
/// before any catch block runs, with the raw `Throwable` object — the only point at which
/// class/message/file/line are cheaply available without walking a stack. This is DST-only
/// (a replay recording), distinct from and upstream of the userland `ExceptionCaptureRecorder`
/// (P1) that builds the `chronos.debugging.v1` snapshot for the Errors/Snapshot feature.
///
/// Recorded regardless of whether the exception is ultimately caught: a caught internal
/// exception still shaped the execution path a replay must reproduce.
#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_throw_trampoline(exception: *mut ext_php_rs::ffi::zend_object) {
    // The identity is read at most once per throw, and only when someone will use
    // it: the DST recording, or the request-open LAST_THROW slot that lets a
    // frameworkless fatal still stamp error.* onto its root span at RSHUTDOWN.
    let request_open = REQUEST_CONTEXT.with(|ctx| ctx.borrow().is_some());
    if request_open || crate::dst_spool::is_active() {
        if let Some(identity) = zend_helpers::exception_identity(exception) {
            if request_open {
                LAST_THROW.with(|slot| {
                    *slot.borrow_mut() = Some(LastThrow {
                        class: bounded(&identity.class, 256),
                        message: bounded(identity.message.as_deref().unwrap_or(""), 2048),
                        file: bounded(identity.file.as_deref().unwrap_or(""), 1024),
                        line: bounded(identity.line.as_deref().unwrap_or(""), 16),
                    });
                });
            }
            if crate::dst_spool::is_active() {
                let mut payload = vec![("class".to_owned(), identity.class)];
                if let Some(message) = identity.message {
                    payload.push(("message".to_owned(), message));
                }
                if let Some(code) = identity.code {
                    payload.push(("code".to_owned(), code));
                }
                if let Some(file) = identity.file {
                    payload.push(("file".to_owned(), file));
                }
                if let Some(line) = identity.line {
                    payload.push(("line".to_owned(), line));
                }
                crate::dst_spool::record(crate::dst_spool::DstEventKind::Exception, payload);
            }
        }
    }
    if let Some(Some(previous)) = PREVIOUS_THROW_HOOK.get() {
        previous(exception);
    }
}

#[cfg(feature = "zend-observer")]
type ZendObserverFcallBegin =
    unsafe extern "C" fn(execute_data: *mut ext_php_rs::ffi::zend_execute_data);

#[cfg(feature = "zend-observer")]
type ZendObserverFcallEnd = unsafe extern "C" fn(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
);

#[cfg(feature = "zend-observer")]
#[repr(C)]
struct ZendObserverFcallHandlers {
    begin: Option<ZendObserverFcallBegin>,
    end: Option<ZendObserverFcallEnd>,
}

#[cfg(feature = "zend-observer")]
type ZendObserverFcallInit = unsafe extern "C" fn(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> ZendObserverFcallHandlers;

#[cfg(feature = "zend-observer")]
unsafe extern "C" {
    fn zend_observer_fcall_register(callback: Option<ZendObserverFcallInit>);
}

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_observer_factory(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> ZendObserverFcallHandlers {
    const NONE: ZendObserverFcallHandlers = ZendObserverFcallHandlers {
        begin: None,
        end: None,
    };
    if execute_data.is_null() {
        return NONE;
    }
    let name = zend_helpers::function_name(execute_data).unwrap_or_default();
    let is_internal = zend_helpers::is_internal_function(execute_data);
    if observe_policy(&name, is_internal).is_none() {
        return NONE;
    }
    ZendObserverFcallHandlers {
        begin: Some(chronos_begin_trampoline),
        end: Some(chronos_end_trampoline),
    }
}

/// ADR 0021 Phase 2: retain a first-party call visit when DST is armed.
///
/// ADR 0029 Tier 3 deepens the same event rather than adding a parallel one:
/// `arguments` (already allowlisted, typed, redacted and capped by
/// `capture_tier_three_arguments`) ride the existing `call` payload as
/// `arg<N>.name` / `arg<N>.type` / `arg<N>.value` keys. A path-diff that can say WHICH
/// argument the two runs disagreed on is a materially deeper execution graph than one
/// that can only say the same function was reached, and it costs no new event kind, no
/// new spool schema and no second budget.
#[cfg(feature = "zend-observer")]
fn record_call_path_enter(
    name: &str,
    is_internal: bool,
    defining_file: Option<&str>,
    arguments: &[crate::deterministic::CapturedArgument],
) {
    if is_internal || !crate::dst_spool::is_active() {
        return;
    }
    let first_party = crate::call_path::is_first_party(defining_file, excluded_paths());
    let caps = crate::call_path::caps();
    if let Some(depth) = crate::call_path::on_enter(&caps, first_party) {
        let mut payload = vec![
            ("name".to_owned(), name.to_owned()),
            ("depth".to_owned(), depth.to_string()),
        ];
        for argument in arguments {
            let prefix = format!("arg{}", argument.position);
            if !argument.name.is_empty() {
                payload.push((format!("{prefix}.name"), argument.name.clone()));
            }
            // The TYPE is always recorded, including for a composite whose value was
            // refused: "argument 3 was an array" is evidence about the path and leaks
            // nothing.
            payload.push((
                format!("{prefix}.type"),
                argument.argument_type.proto_name().to_owned(),
            ));
            if !argument.value.is_empty() {
                payload.push((format!("{prefix}.value"), argument.value.clone()));
            }
            if argument.redacted {
                payload.push((format!("{prefix}.redacted"), "true".to_owned()));
            }
            if argument.truncated {
                payload.push((format!("{prefix}.truncated"), "true".to_owned()));
            }
        }
        crate::dst_spool::record(crate::dst_spool::DstEventKind::Call, payload);
    }
}

/// One call's function identity, resolved through the per-process interner.
///
/// This is where the two heap allocations every observed call used to pay before
/// anything was decided are removed: `zend_helpers::function_name`'s
/// `format!("{class}::{method}")` and `CallFrame::observe_only`'s owned copy now happen
/// once per FUNCTION per PROCESS. A hit costs one map probe and three refcount bumps.
///
/// Returns `None` only when the runtime has no function to describe at all, which is
/// also the case in which the old code produced an empty name and `observe_policy`
/// refused it.
#[cfg(feature = "zend-observer")]
unsafe fn interned_function(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> Option<crate::deterministic::InternedFunction> {
    let handle = zend_helpers::function_ptr(execute_data);
    if handle == 0 {
        // No stable key to intern against. Fall back to interning by NAME — a string
        // comparison per call rather than per function, which is the price of not
        // having a key, but never a wrong identity and never a missing row.
        return Some(crate::deterministic::intern_named(
            zend_helpers::function_facts(execute_data)?,
        ));
    }
    Some(crate::deterministic::intern(handle, || {
        zend_helpers::function_facts(execute_data).unwrap_or_default()
    }))
}

/// The observer's span verdict for an interned function, cached per function.
///
/// WHY THIS IS SAFE TO CACHE, given the warning on `observe_policy` itself. That
/// warning is about the ZEND factory's cache, which decides whether handlers are
/// attached AT ALL and cannot be revisited. This cache is ours, it is keyed by
/// [`crate::deterministic::FunctionId`], and it is invalidated by
/// [`MANIFEST_GENERATION`] — the only mutable input `observe_policy` has. Every other
/// input (the name, the internal flag, `span_all_userland`'s `OnceLock`) is fixed for
/// the life of the process, so a generation-matched hit is byte-identical to a
/// recomputation.
///
/// A `Vec` indexed by id rather than a second map: ids are dense and monotonic, so this
/// is an array index, and it keeps the deterministic module free of any knowledge of
/// `SpanPolicy`.
#[cfg(feature = "zend-observer")]
fn cached_policy(
    id: crate::deterministic::FunctionId,
    name: &str,
    is_internal: bool,
) -> Option<SpanPolicy> {
    let generation = manifest_generation();
    let index = id as usize;
    let hit = POLICY_CACHE.with(|cache| {
        cache
            .borrow()
            .get(index)
            .copied()
            .flatten()
            .filter(|entry| entry.generation == generation)
            .map(|entry| entry.policy)
    });
    if let Some(policy) = hit {
        return policy;
    }
    let policy = observe_policy(name, is_internal);
    POLICY_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() <= index {
            cache.resize(index + 1, None);
        }
        cache[index] = Some(CachedPolicy { generation, policy });
    });
    policy
}

#[cfg(feature = "zend-observer")]
#[derive(Clone, Copy)]
struct CachedPolicy {
    generation: u64,
    policy: Option<SpanPolicy>,
}

/// Tier 3: bounded, redacted, scalar-only arguments for a manifest-allowlisted call.
///
/// FOUR independent gates, all of which must pass, and none of which this function is
/// allowed to skip: the request must have armed Tier 3 (a forced profile or an armed
/// DST recording), the function must be userland, and it must be named by the
/// instrumentation manifest. `CHRONOS_PHP_PROFILE_ARGS` is the fourth and is checked
/// inside `arguments_active`.
///
/// Returns the captured arguments so the caller can ALSO hang them on the DST
/// call-path event — the same evidence, once, in both places a reader looks.
#[cfg(feature = "zend-observer")]
unsafe fn capture_tier_three_arguments(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    interned: &crate::deterministic::InternedFunction,
) -> Vec<crate::deterministic::CapturedArgument> {
    if interned.internal || !crate::deterministic::arguments_active() {
        return Vec::new();
    }
    // The manifest allowlist, re-checked here rather than trusted from the policy:
    // `ManifestSpan` is not the only policy an allowlisted function can end up with
    // (an allowlisted I/O method keeps `IoSpan`), and Tier 3 follows the ALLOWLIST, not
    // the span decision.
    if !is_traced(&interned.name) {
        return Vec::new();
    }
    let config = crate::deterministic::config();
    let (arguments, dropped) = zend_helpers::capture_scalar_arguments(
        execute_data,
        config.max_arguments,
        config.max_argument_bytes,
    );
    if arguments.is_empty() && dropped == 0 {
        return Vec::new();
    }
    if crate::deterministic::record_arguments(interned.id, arguments.clone(), dropped) {
        arguments
    } else {
        // Refused by a budget. The refusal is already counted in `truncation`; handing
        // the arguments to the DST graph anyway would route around the byte cap.
        Vec::new()
    }
}

/// Push a prepared frame onto the observer's stack, stamping the deterministic clock in
/// the same breath.
///
/// THE ONE PLACE a counted frame comes into existence, and that is the point:
/// `deterministic::on_enter` increments a call count and a recursion depth that only the
/// matching pop can unwind, so an enter that is not immediately followed by a push is a
/// frame the end trampoline can never balance.
///
/// It also has to be LAST rather than merely paired. `inject_curl_traceparent` calls back
/// into PHP — `curl_setopt`, which is itself an observed function with its own
/// trampolines — so a start stamped before the frame was assembled would charge our own
/// header injection to the observed call, and would hand the injected call's duration to
/// this frame's parent while this frame's inclusive time covered it too. Double counted,
/// plausible-looking, and invisible.
#[cfg(feature = "zend-observer")]
fn push_frame(mut frame: CallFrame, function: crate::deterministic::FunctionId) {
    frame.timing = crate::deterministic::on_enter(function, monotonic_nanos_u64());
    CALL_FRAMES.with(|frames| frames.borrow_mut().push(frame));
}

#[cfg(feature = "zend-observer")]
fn record_call_path_leave(is_internal: bool) {
    if is_internal || !crate::dst_spool::is_active() {
        return;
    }
    crate::call_path::on_leave();
}

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_begin_trampoline(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) {
    if execute_data.is_null() {
        return;
    }

    // Consume any profiler ticks the SIGPROF handler queued since the last
    // function-call boundary. This is the safe walk point: we are at a Zend
    // instruction boundary, never inside a signal handler. MUST stay first: ticks are
    // drained at the instruction boundary, not deferred behind our own bookkeeping.
    crate::sampler::consume_pending_ticks();

    // ONE dereference of `(*execute_data).func`, cached per function: name, defining
    // file, line and the internal flag all come off the same struct, and the old shape
    // re-walked the pointer once per helper once per call.
    let Some(interned) = interned_function(execute_data) else {
        return;
    };
    let name = interned.name.clone();
    let is_internal = interned.internal;
    let Some(mut policy) = cached_policy(interned.id, &name, is_internal) else {
        return;
    };

    // Userland SQL/cache instrumentation is active for this request: its spans
    // are richer (host/db/bound params), so the native ones stand down. The
    // frame still runs as ObserveOnly to keep begin/end pairing intact.
    if policy == SpanPolicy::IoSpan && native_io_suppressed(&name) {
        policy = SpanPolicy::ObserveOnly;
    }

    // A dual-purpose stream call is a client span only when its target is actually
    // remote. Read once here: the verdict decides the policy, and a remote target is
    // also the span's `http.url`, so re-reading the argument later would be waste.
    let mut stream_url = None;
    if policy == SpanPolicy::IoSpan && is_dual_purpose_stream_function(&name) {
        let target = zend_helpers::arg_scalar_string(execute_data, 0, 2048);
        match target {
            Some(target) if is_remote_stream_target(&target) => stream_url = Some(target),
            // A local file read. The profiler still accounts for its time; the trace
            // does not need a row per template.
            _ => policy = SpanPolicy::ObserveOnly,
        }
    }

    // Where the observed userland function is DEFINED — the file that owns the code,
    // not the file that called it. Used both to drop dependency internals and, when a
    // span survives, to tell the reader which source file it came from.
    //
    // Read off the INTERNED origin rather than re-resolved per call: a function's
    // defining file is fixed for the life of the process, so this was a third heap
    // allocation on every observed call for a string that never changes.
    let defining_file: Option<&str> = if is_internal { None } else { interned.file() };
    // Manifest spans are explicit intent (`chronos_trace_function`) and are NEVER
    // dropped by the path rule: someone asked for that function by name, and a
    // dependency they chose to instrument is theirs to see.
    if policy == SpanPolicy::UserSpan
        && defining_file.is_some_and(|file| is_excluded_path(file, excluded_paths()))
    {
        policy = SpanPolicy::ObserveOnly;
    }

    // Track curl header configuration so traceparent injection is merge-safe.
    if &*name == "curl_setopt" {
        track_curl_setopt(execute_data);
    } else if &*name == "curl_setopt_array" {
        track_curl_setopt_array(execute_data);
    }

    // ObserveOnly is the bulk path (every unlisted userland call lands here): push a
    // minimal frame purely to keep begin/end pairing intact — no span ids, no
    // timestamps, no context clone. The end trampoline pops it and emits nothing.
    if policy == SpanPolicy::ObserveOnly {
        let has_context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().is_some());
        // Pair begin/end whenever we have a request context OR DST is armed: call-path
        // depth must stay balanced with leave in the end trampoline.
        //
        // The deterministic aggregate rides STRICTLY inside this guard, and that is
        // load-bearing. The end trampoline pops unconditionally, so counting an enter
        // the guard refused to push would leave a frame the pop can never balance —
        // and the aggregate would drift a little further wrong on every request. The
        // buffer therefore never owns a stack; it is handed back the timing this frame
        // carries. `has_context` is true for every request the collector actually
        // started, sampled or not, which is exactly Tier 1's always-on scope.
        if has_context || crate::dst_spool::is_active() {
            let arguments = capture_tier_three_arguments(execute_data, &interned);
            record_call_path_enter(&name, is_internal, defining_file, &arguments);
            push_frame(CallFrame::observe_only(name), interned.id);
        }
        return;
    }

    REQUEST_CONTEXT.with(|ctx| {
        let context = ctx.borrow().as_ref().cloned();
        if let Some(context) = context {
            let mut frame = on_begin(&context, &name, policy);

            if policy == SpanPolicy::IoSpan {
                capture_io_detail(execute_data, &mut frame);
                if let Some(ref url) = stream_url {
                    frame
                        .attributes
                        .push((crate::http_capture::URL_FULL.into(), url.clone()));
                    frame.attributes.push(("http.url".into(), url.clone()));
                }
            }
            // The source file behind a userland span. Internal functions have none,
            // and this is the same identity the profiler already reports per frame —
            // never an argument or a captured value.
            if let Some(file) = defining_file {
                frame
                    .attributes
                    .push(("code.filepath".into(), file.to_owned()));
            }
            if policy == SpanPolicy::ManifestSpan {
                // Explicit user intent: the attribute both marks provenance for the UI
                // and exempts the span from the min-duration filter (the keep rule
                // always retains spans that carry attributes).
                frame
                    .attributes
                    .push(("chronos.instrumented".into(), "manifest".into()));
            }

            if let Some(ref traceparent) = frame.injected_traceparent {
                if &*name == "curl_exec" {
                    inject_curl_traceparent(execute_data, traceparent);
                }
                // curl_multi_exec / file_get_contents: covered by the pending-
                // traceparent userland seam (Guzzle middleware, Http facade).
            }

            let arguments = capture_tier_three_arguments(execute_data, &interned);
            record_call_path_enter(&name, is_internal, defining_file, &arguments);
            // Same enter-and-push as the ObserveOnly branch above, through the same
            // helper, so Tier 1 covers ALL observed calls rather than only the unlisted
            // ones — and so neither branch can drift from the other.
            push_frame(frame, interned.id);
        }
    });
}

/// Attach payload detail to an I/O span from the observed call's arguments.
/// Values are bounded; bound parameters stay in the userland hooks (Doctrine /
/// DB::listen) where redaction policy applies.
#[cfg(feature = "zend-observer")]
unsafe fn capture_io_detail(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    frame: &mut CallFrame,
) {
    let name: &str = &frame.name;
    frame.attributes.push(("span.kind".into(), "client".into()));
    // Arg 0 is a cache KEY only on data operations — on lifecycle methods
    // (__construct, connect, auth, …) it is a host or credential, not a key.
    let lifecycle_method = name.rsplit("::").next().is_some_and(|method| {
        matches!(
            method,
            "__construct"
                | "__destruct"
                | "connect"
                | "pconnect"
                | "open"
                | "auth"
                | "select"
                | "close"
                | "setOption"
                | "getOption"
                | "addServer"
                | "addServers"
                | "quit"
        )
    });
    if name.starts_with("Redis::") || name.starts_with("RedisCluster::") {
        frame.attributes.push(("db.system".into(), "redis".into()));
        if !lifecycle_method {
            if let Some(key) = zend_helpers::arg_scalar_string(execute_data, 0, 256) {
                frame.attributes.push(("cache.key".into(), key));
            }
        }
    } else if name.starts_with("Memcached::") {
        frame
            .attributes
            .push(("db.system".into(), "memcached".into()));
        if !lifecycle_method {
            if let Some(key) = zend_helpers::arg_scalar_string(execute_data, 0, 256) {
                frame.attributes.push(("cache.key".into(), key));
            }
        }
    } else if name == "PDO::query"
        || name == "PDO::exec"
        || name == "PDO::prepare"
        || name == "mysqli::query"
        || name == "SQLite3::query"
        || name == "SQLite3::exec"
    {
        if let Some(statement) = zend_helpers::arg_scalar_string(execute_data, 0, 4096) {
            // Legacy + current semconv spelling, same value — see the constants'
            // comment in `http_capture.rs` for why both are kept.
            frame
                .attributes
                .push((crate::http_capture::DB_QUERY_TEXT.into(), statement.clone()));
            frame.attributes.push(("db.statement".into(), statement));
        }
    } else if name == "mysqli_stmt::execute" || name == "SQLite3Stmt::execute" {
        // The SQL text is NOT an argument here — it was banked at prepare time
        // against this statement object's handle. A map miss (procedural prepare,
        // cap overflow, statement from before this request) still emits the span,
        // just without the text: an execute round trip with an unknown statement
        // is better evidence than no row at all.
        if let Some(handle) = zend_helpers::this_object_handle(execute_data) {
            let statement = PREPARED_STATEMENTS.with(|map| map.borrow().get(&handle).cloned());
            if let Some(statement) = statement {
                frame
                    .attributes
                    .push((crate::http_capture::DB_QUERY_TEXT.into(), statement.clone()));
                frame.attributes.push(("db.statement".into(), statement));
            }
        }
    } else if name == "curl_exec" {
        if let Some(url) = zend_helpers::curl_effective_url(execute_data) {
            frame
                .attributes
                .push((crate::http_capture::URL_FULL.into(), url.clone()));
            frame.attributes.push(("http.url".into(), url));
        }
    }
}

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_end_trampoline(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
) {
    // A function call observed while an exception is propagating (or that itself
    // threw) unwinds with EG(exception) set — mark the span errored.
    let threw = zend_helpers::exception_pending();

    CALL_FRAMES.with(|frames| {
        let frame = frames.borrow_mut().pop();
        if let Some(mut frame) = frame {
            // The DETERMINISTIC clock is read FIRST, before any of the bookkeeping
            // below: curl result capture calls back into PHP (`curl_getinfo`) and DST
            // recording formats strings, and charging our own instrumentation's cost to
            // the observed function would be the one bias a counted profiler must not
            // have.
            let leave_nanos = monotonic_nanos_u64();

            // Bank the deterministic aggregate immediately, BEFORE the curl/DST
            // bookkeeping below and before `on_end` consumes the frame.
            //
            // Ordering, not taste: `capture_curl_result` calls back into PHP
            // (`curl_getinfo`), and doing our own accounting first means no
            // re-entrant observed call can interleave with it. The caller is read off
            // the frame BELOW this one on the observer's OWN stack — the same stack the
            // pop came from — so a Tier 2 edge can never name a caller the frame stack
            // does not actually have.
            let timing = frame.timing;
            let caller = frames.borrow().last().map(|parent| parent.timing.function);
            let duration = crate::deterministic::on_leave(&timing, caller, leave_nanos);
            // Exclusive time is `duration - child_nanoseconds`, so every frame hands its
            // inclusive duration up to its parent as it leaves. Done even when the
            // aggregate refused to count THIS frame (the function cap), because the
            // parent's exclusive time is still wrong without it.
            if let Some(parent) = frames.borrow_mut().last_mut() {
                parent.timing.child_nanoseconds =
                    parent.timing.child_nanoseconds.saturating_add(duration);
            }
            // Everything an outbound HTTP call knows is only knowable now: curl fills
            // its timing and response info in during the transfer, and the body is the
            // return value we are holding.
            if &*frame.name == "curl_exec" {
                capture_curl_result(execute_data, retval, &mut frame);
            }
            // Bank a successful prepare's SQL against the statement object — the one
            // it RETURNED (mysqli::prepare and friends) or the one it was CALLED ON
            // (mysqli_stmt::prepare / new mysqli_stmt) — for the later `::execute`
            // span (the text is not an argument there). At END rather than begin
            // because only the end handler holds the return value — and a failed
            // prepare (`false`) must bank nothing / must EVICT a recycled handle.
            if sql_prepare_method(&frame.name) {
                bank_prepared_statement(execute_data, retval, prepare_sql_arg_index(&frame.name));
            } else if sql_this_prepare_method(&frame.name) {
                rebank_prepared_statement_for_this(execute_data, retval, &frame.name);
            }
            // DST: record the observed result of known non-deterministic builtins.
            if crate::dst_spool::is_active() {
                if let Some(kind) = dst_event_kind_for(&frame.name) {
                    let payload = if matches!(kind, crate::dst_spool::DstEventKind::EnvRead) {
                        // Protocol env channel selects on variable name and answers with value
                        // (conformance / Effect::environment), not function+result.
                        let name_arg = zend_helpers::arg_scalar_string(execute_data, 0, 4096)
                            .unwrap_or_default();
                        let value = zend_helpers::scalar_to_string(retval).unwrap_or_default();
                        vec![("name".to_owned(), name_arg), ("value".to_owned(), value)]
                    } else {
                        let value = zend_helpers::scalar_to_string(retval).unwrap_or_default();
                        vec![
                            ("function".to_owned(), frame.name.to_string()),
                            ("result".to_owned(), value),
                        ]
                    };
                    crate::dst_spool::record(kind, payload);
                }
                let is_internal = zend_helpers::is_internal_function(execute_data);
                record_call_path_leave(is_internal);
            }
            if on_end(frame, threw) {
                // A kept child pins its ancestor so the waterfall stays connected.
                if let Some(parent) = frames.borrow_mut().last_mut() {
                    parent.kept_child = true;
                }
            }
        }
    });
}

#[cfg(feature = "zend-observer")]
thread_local! {
    static CALL_FRAMES: RefCell<Vec<CallFrame>> = const { RefCell::new(Vec::new()) };
    /// Cached `observe_policy` verdicts, indexed by interned function id. Per PROCESS
    /// in spirit (PHP-FPM is one request per worker), and deliberately NOT reset
    /// between requests: the verdict depends only on the function and the manifest
    /// generation, both of which outlive the request. See `cached_policy`.
    static POLICY_CACHE: RefCell<Vec<Option<CachedPolicy>>> = const { RefCell::new(Vec::new()) };
    /// Per-curl-handle custom header lists observed via curl_setopt, keyed by the
    /// CurlHandle object handle id. Injection merges with these instead of clobbering.
    static CURL_HEADERS: RefCell<std::collections::HashMap<u32, Vec<String>>> =
        RefCell::new(std::collections::HashMap::new());
    /// SQL text banked at prepare time (`mysqli::prepare` / `SQLite3::prepare` /
    /// `mysqli_prepare` keyed by the RETURNED statement's handle;
    /// `mysqli_stmt::prepare` / `new mysqli_stmt` keyed by `$this` — the query is
    /// not an argument of the later `::execute` call, so this map is the only
    /// bridge between the two). Same per-handle-map pattern as CURL_HEADERS,
    /// cleared with it per request, and EVICTED per handle whenever an observed
    /// prepare path re-creates a statement without readable SQL — the engine
    /// recycles object handles, and a recycled id keeping a freed statement's text
    /// would stamp the wrong query onto a span. A miss (evicted entry, statement
    /// prepared before this request) degrades to an execute span WITHOUT the text
    /// — never to a dropped span.
    static PREPARED_STATEMENTS: RefCell<std::collections::HashMap<u32, String>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Ceiling on remembered prepared statements per request. CURL_HEADERS gets away
/// without one because an application holds a handful of curl handles; an ORM in a
/// loop can prepare per row, and each entry here retains up to 4 KiB of SQL text.
/// Past the cap new prepares fall back to the graceful text-less path above.
#[cfg(feature = "zend-observer")]
const MAX_PREPARED_STATEMENTS: usize = 512;

/// Remember a prepare call's SQL, keyed by the statement object it returned —
/// see PREPARED_STATEMENTS for the map's contract and the miss behaviour.
///
/// The cap refuses NEW handles only: re-preparing into an already-known handle id
/// always lands, because the engine recycles object handles and a recycled id must
/// never keep the PREVIOUS statement's text — a wrong query on a span is worse
/// than no query.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn bank_prepared_statement(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    sql_arg_index: usize,
) {
    let Some(handle) = zend_helpers::retval_object_handle(retval) else {
        return;
    };
    // Same 4096-byte cap as the direct-query capture in `capture_io_detail`, so a
    // prepared query is bounded exactly like an immediate one.
    let Some(statement) = zend_helpers::arg_scalar_string(execute_data, sql_arg_index, 4096)
    else {
        // A brand-new statement object whose SQL could not be read must not
        // inherit whatever a freed statement left banked under this handle id.
        PREPARED_STATEMENTS.with(|map| {
            map.borrow_mut().remove(&handle);
        });
        return;
    };
    PREPARED_STATEMENTS.with(|map| {
        let mut map = map.borrow_mut();
        if map.len() >= MAX_PREPARED_STATEMENTS && !map.contains_key(&handle) {
            return;
        }
        map.insert(handle, statement);
    });
}

/// The `$this`-keyed counterpart of `bank_prepared_statement`, for the prepare
/// calls that mutate an existing statement object (`mysqli_stmt::prepare`,
/// `mysqli_stmt::__construct`) instead of returning a new one.
///
/// Eviction is the point, not an edge case: these are exactly the creation paths
/// the retval-keyed banking never sees, so a recycled object handle could
/// otherwise keep a freed statement's SQL and stamp the wrong query onto this
/// statement's `::execute` span. A failed `prepare()` (returns `false`), a
/// constructor called without a query, or an unreadable argument all EVICT the
/// handle; only a successful prepare with readable SQL re-banks it. The cap
/// refuses NEW handles only, same rule as above — an evicted-or-replaced known
/// handle always lands.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn rebank_prepared_statement_for_this(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    name: &str,
) {
    let Some(handle) = zend_helpers::this_object_handle(execute_data) else {
        return;
    };
    // `mysqli_stmt::prepare` answers a bool; banking on `false` would record SQL
    // the server never compiled. A constructor has no meaningful return value —
    // reaching the end handler at all means it did not throw.
    let succeeded =
        name != "mysqli_stmt::prepare" || zend_helpers::retval_is_true(retval);
    let statement = if succeeded {
        zend_helpers::arg_scalar_string(execute_data, prepare_sql_arg_index(name), 4096)
    } else {
        None
    };
    PREPARED_STATEMENTS.with(|map| {
        let mut map = map.borrow_mut();
        match statement {
            Some(sql) => {
                if map.len() >= MAX_PREPARED_STATEMENTS && !map.contains_key(&handle) {
                    return;
                }
                map.insert(handle, sql);
            }
            None => {
                map.remove(&handle);
            }
        }
    });
}

/// Response detail for a finished `curl_exec`: the connection phase timeline, the
/// status and content type, the peer address, the request headers the application
/// set, and the response body.
///
/// This is the client-side twin of the server-side `http_capture` module, and it
/// writes the SAME attribute keys — `http.request.headers`, `http.response.body`,
/// `http.timeline` — so the desktop's Request / Response / Timeline tabs render an
/// outbound call and an inbound request through one code path.
///
/// Redaction and caps are applied by `http_capture` so an `Authorization:` header on
/// an outbound API call is masked exactly like an inbound one.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are both
/// still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn capture_curl_result(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    frame: &mut CallFrame,
) {
    let config = crate::http_capture::current_config();
    if !config.enabled {
        return;
    }
    let info = zend_helpers::curl_info(execute_data);

    // Request headers the application configured on this handle (plus the traceparent
    // the observer merged in), as the same JSON object shape the server side emits.
    if let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) {
        let headers = CURL_HEADERS.with(|map| map.borrow().get(&handle).cloned());
        if let Some(headers) = headers {
            let pairs: Vec<(String, String)> = headers
                .iter()
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
                .collect();
            if let Some(json) = crate::http_capture::encode_header_map(pairs, &config) {
                frame
                    .attributes
                    .push((crate::http_capture::REQUEST_HEADERS.into(), json));
            }
        }
    }

    let content_type = info.get("content_type").cloned().unwrap_or_default();
    if let Some(code) = info.get("http_code").and_then(|c| c.parse::<i64>().ok()) {
        if code > 0 {
            // Legacy + current semconv spelling, same value (see `http_capture.rs`).
            frame
                .attributes
                .push(("http.status_code".into(), code.to_string()));
            frame.attributes.push((
                crate::http_capture::RESPONSE_STATUS_CODE.into(),
                code.to_string(),
            ));
        }
    }
    if !content_type.is_empty() {
        frame
            .attributes
            .push(("http.response.content_type".into(), content_type.clone()));
    }
    if let Some(ip) = info.get("primary_ip").filter(|ip| !ip.is_empty()) {
        frame.attributes.push(("server.address".into(), ip.clone()));
    }
    if let Some(port) = info.get("primary_port").filter(|p| p.as_str() != "0") {
        frame.attributes.push(("server.port".into(), port.clone()));
    }
    if let Some(method) = info.get("effective_method").filter(|m| !m.is_empty()) {
        frame
            .attributes
            .push(("http.method".into(), method.clone()));
        frame
            .attributes
            .push((crate::http_capture::REQUEST_METHOD.into(), method.clone()));
    }

    // The body, but only when CURLOPT_RETURNTRANSFER made curl_exec return one; a
    // handle writing straight to stdout returns `true`, and "true" is not a payload.
    if config.capture_bodies {
        if let Some(body) = zend_helpers::string_retval(retval) {
            let size = info
                .get("size_download")
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|size| *size > 0)
                .unwrap_or(body.len());
            frame.attributes.extend(crate::http_capture::encode_body(
                crate::http_capture::RESPONSE_BODY,
                &body,
                size,
                &content_type,
                &config,
            ));
        }
    }

    if let Some(timeline) = curl_timeline(&info) {
        frame
            .attributes
            .push((crate::http_capture::TIMELINE.into(), timeline));
    }
}

/// curl's cumulative timing marks -> the phase array the Timeline tab draws.
///
/// Every `*_time` curl reports is measured from the START of the transfer and is
/// cumulative, so consecutive marks subtract into the phase between them. A mark of
/// zero means the phase did not happen (no TLS on plain HTTP, no DNS on a warm
/// connection or an IP literal), and a zero-length phase is dropped rather than
/// drawn as a hairline the reader would have to hover to dismiss.
#[cfg(feature = "zend-observer")]
fn curl_timeline(info: &std::collections::HashMap<String, String>) -> Option<String> {
    let seconds = |key: &str| -> Option<f64> {
        info.get(key)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
    };
    let total = seconds("total_time")?;
    let namelookup = seconds("namelookup_time").unwrap_or(0.0);
    let connect = seconds("connect_time").unwrap_or(namelookup);
    let appconnect = seconds("appconnect_time").unwrap_or(0.0);
    let pretransfer = seconds("pretransfer_time").unwrap_or(connect.max(appconnect));
    let starttransfer = seconds("starttransfer_time").unwrap_or(pretransfer);

    // (name, cumulative end mark). TLS collapses into the connect phase when curl
    // reports no appconnect mark, which is exactly what a plain-HTTP call looks like.
    let marks: Vec<(&str, f64)> = vec![
        ("dns", namelookup),
        ("connect", connect),
        (
            "tls",
            if appconnect > 0.0 {
                appconnect
            } else {
                connect
            },
        ),
        ("send", pretransfer),
        ("wait", starttransfer),
        ("download", total),
    ];

    let mut phases: Vec<(String, u128, u128)> = Vec::new();
    let mut cursor = 0.0f64;
    for (name, end) in marks {
        if end <= cursor {
            continue;
        }
        phases.push((name.to_owned(), to_nanos(cursor), to_nanos(end)));
        cursor = end;
    }
    if phases.is_empty() {
        return None;
    }
    Some(crate::http_capture::encode_phases(&phases))
}

#[cfg(feature = "zend-observer")]
fn to_nanos(seconds: f64) -> u128 {
    (seconds.max(0.0) * 1e9) as u128
}

/// CURLOPT_HTTPHEADER — stable across every curl/PHP version.
#[cfg(feature = "zend-observer")]
const CURLOPT_HTTPHEADER: i64 = 10023;

/// Record the header list an application sets on a curl handle:
/// `curl_setopt($ch, CURLOPT_HTTPHEADER, [...])`.
#[cfg(feature = "zend-observer")]
unsafe fn track_curl_setopt(execute_data: *mut ext_php_rs::ffi::zend_execute_data) {
    let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) else {
        return;
    };
    let Some(option) = zend_helpers::arg_long(execute_data, 1) else {
        return;
    };
    if option != CURLOPT_HTTPHEADER {
        return;
    }
    let headers = zend_helpers::arg_string_array(execute_data, 2).unwrap_or_default();
    CURL_HEADERS.with(|map| {
        map.borrow_mut().insert(handle, headers);
    });
}

/// Record headers set through `curl_setopt_array($ch, [CURLOPT_HTTPHEADER => [...]])`.
#[cfg(feature = "zend-observer")]
unsafe fn track_curl_setopt_array(execute_data: *mut ext_php_rs::ffi::zend_execute_data) {
    let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) else {
        return;
    };
    let Some(headers) =
        zend_helpers::arg_array_key_string_array(execute_data, 1, CURLOPT_HTTPHEADER)
    else {
        return;
    };
    CURL_HEADERS.with(|map| {
        map.borrow_mut().insert(handle, headers);
    });
}

/// Inject a traceparent header into the curl handle passed to `curl_exec($ch)`.
///
/// Merge-safe: the header list is the application's tracked CURLOPT_HTTPHEADER value
/// (if any) plus our `traceparent`, applied via a real `curl_setopt` call. The pending
/// traceparent is also published for the userland seam (`chronos_pending_traceparent`).
///
/// # Safety
/// Called from the Zend observer with a valid execute_data pointer.
#[cfg(feature = "zend-observer")]
unsafe fn inject_curl_traceparent(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    traceparent: &str,
) {
    // Publish for userland retrieval regardless of whether direct injection works.
    PENDING_TRACEPARENT.with(|tp| *tp.borrow_mut() = Some(traceparent.to_owned()));

    let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) else {
        return;
    };

    let mut headers = CURL_HEADERS
        .with(|map| map.borrow().get(&handle).cloned())
        .unwrap_or_default();
    // The request's inbound `tracestate`/`baggage` ride along VERBATIM. They were
    // captured next to the traceparent (see `lib.rs::start_request`) and never
    // parsed — pass-through is the whole contract.
    let (tracestate, baggage) = REQUEST_CONTEXT.with(|ctx| {
        ctx.borrow()
            .as_ref()
            .map(|c| (c.tracestate.clone(), c.baggage.clone()))
            .unwrap_or((None, None))
    });
    merge_propagation_headers(
        &mut headers,
        traceparent,
        tracestate.as_deref(),
        baggage.as_deref(),
    );

    zend_helpers::call_curl_setopt_httpheader(execute_data, &headers);

    // The merged list is now the handle's effective header set.
    CURL_HEADERS.with(|map| {
        map.borrow_mut().insert(handle, headers);
    });
}

/// Merge the trace-propagation headers into an application's own curl header list.
///
/// `traceparent` always lands. `tracestate` is forwarded whenever the inbound
/// request carried one, because W3C Trace Context REQUIRES a participant that
/// forwards `traceparent` to also forward `tracestate` it does not understand —
/// the vendor entries in it belong to OTHER tracers sharing this trace, and
/// dropping them severs their correlation. `baggage` follows the same
/// forward-as-is contract (its own W3C spec).
///
/// Dedupe mirrors the traceparent idempotence rule that was always here: each
/// header we are about to add first evicts any earlier spelling of itself (a
/// retried handle, or an application that forwarded the inbound headers by
/// hand). A header we have NO value for is left untouched — an application's
/// own `tracestate` is not ours to remove.
fn merge_propagation_headers(
    headers: &mut Vec<String>,
    traceparent: &str,
    tracestate: Option<&str>,
    baggage: Option<&str>,
) {
    // Idempotence: never stack multiple traceparent headers on retried handles.
    headers.retain(|h| !h.to_ascii_lowercase().starts_with("traceparent:"));
    headers.push(format!("traceparent: {traceparent}"));
    if let Some(tracestate) = tracestate {
        headers.retain(|h| !h.to_ascii_lowercase().starts_with("tracestate:"));
        headers.push(format!("tracestate: {tracestate}"));
    }
    if let Some(baggage) = baggage {
        headers.retain(|h| !h.to_ascii_lowercase().starts_with("baggage:"));
        headers.push(format!("baggage: {baggage}"));
    }
}

#[cfg(feature = "zend-observer")]
thread_local! {
    static PENDING_TRACEPARENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Called from the PHP-registered `chronos_pending_traceparent()` function
/// to retrieve the pending traceparent value.
pub fn take_pending_traceparent() -> Option<String> {
    #[cfg(feature = "zend-observer")]
    {
        PENDING_TRACEPARENT.with(|tp| tp.borrow_mut().take())
    }
    #[cfg(not(feature = "zend-observer"))]
    {
        None
    }
}

#[cfg(feature = "zend-observer")]
pub(crate) mod zend_helpers {
    use std::ffi::CStr;

    pub unsafe fn function_name(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<String> {
        let func = (*execute_data).func;
        if func.is_null() {
            return None;
        }
        let func_name = (*func).common.function_name;
        if func_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr((*func_name).val.as_ptr()).to_string_lossy();

        let scope = (*func).common.scope;
        if !scope.is_null() {
            let class_name = (*scope).name;
            if !class_name.is_null() {
                let cls = CStr::from_ptr((*class_name).val.as_ptr()).to_string_lossy();
                return Some(format!("{cls}::{name}"));
            }
        }
        Some(name.into_owned())
    }

    /// The runtime's own handle for the observed function, as an opaque `usize`.
    ///
    /// THE interning key. `zend_function` is allocated once per function and lives as
    /// long as the process (op arrays and internal function structs are engine-owned and
    /// cached), which is exactly the property an interner needs — and it is the same key
    /// the Zend observer factory itself caches its verdict on, so a hit here is a hit
    /// there. `0` means "no handle", never a valid function.
    ///
    /// Returned as a `usize` rather than a pointer so the interner can live in a module
    /// with no Zend types and stay unit-testable without PHP.
    pub unsafe fn function_ptr(execute_data: *mut ext_php_rs::ffi::zend_execute_data) -> usize {
        if execute_data.is_null() {
            return 0;
        }
        (*execute_data).func as usize
    }

    /// Everything the interner needs about a function, from ONE walk of its
    /// `zend_function`: canonical name, module, defining file, declaring line, and
    /// whether it is engine-internal.
    ///
    /// Called only on an interner MISS, which is what makes it affordable to be this
    /// thorough. Every pointer is null-checked before dereference — `panic = "abort"` is
    /// set for the release profile, so an unwrap here is a worker crash, not an error.
    pub unsafe fn function_facts(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<crate::deterministic::FunctionFacts> {
        if execute_data.is_null() {
            return None;
        }
        let func = (*execute_data).func;
        if func.is_null() {
            return None;
        }
        let name = function_name(execute_data)?;
        let internal = (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8;
        // `module` uses the same vocabulary the sampler's stack walk writes to
        // `SampleFrame::module`, so a counted row and a sampled frame describe their
        // origin identically.
        let module: std::rc::Rc<str> = if internal {
            std::rc::Rc::from("internal")
        } else {
            std::rc::Rc::from("php")
        };
        let (file, line) = if internal {
            (std::rc::Rc::from(""), 0)
        } else {
            let file: std::rc::Rc<str> = match defining_file(execute_data) {
                Some(file) => std::rc::Rc::from(file.as_str()),
                None => std::rc::Rc::from(""),
            };
            (file, (*func).op_array.line_start)
        };
        Some(crate::deterministic::FunctionFacts {
            name,
            origin: crate::deterministic::FunctionOrigin { module, file, line },
            internal,
        })
    }

    /// The compiled-file path of a userland function's op array — where the function is
    /// DEFINED. `None` for internal functions, and for user functions with no filename
    /// (eval'd code), which the path rules treat as unknown rather than as application
    /// code.
    pub unsafe fn defining_file(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<String> {
        let func = (*execute_data).func;
        if func.is_null() || (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8 {
            return None;
        }
        let filename = (*func).op_array.filename;
        if filename.is_null() {
            return None;
        }
        let file = CStr::from_ptr((*filename).val.as_ptr()).to_string_lossy();
        if file.is_empty() {
            None
        } else {
            Some(file.into_owned())
        }
    }

    /// True for engine-internal functions (builtins and extension methods),
    /// false for userland-defined PHP code.
    pub unsafe fn is_internal_function(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> bool {
        let func = (*execute_data).func;
        if func.is_null() {
            return true;
        }
        (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8
    }

    /// True when an exception is currently propagating in the executor.
    pub unsafe fn exception_pending() -> bool {
        !ext_php_rs::ffi::executor_globals.exception.is_null()
    }

    /// Identity of a thrown object, read at the moment `zend_throw_exception_hook` fires —
    /// class name plus whichever of `message`/`code`/`file`/`line` the `Throwable` actually
    /// set (a userland class that skips `parent::__construct()` can leave any of them unset).
    pub struct ExceptionIdentity {
        pub class: String,
        pub message: Option<String>,
        pub code: Option<String>,
        pub file: Option<String>,
        pub line: Option<String>,
    }

    /// Class name of a thrown object, read the same way as a call's own scope in
    /// `function_name` above: `zend_class_entry.name` is a `zend_string`.
    unsafe fn exception_class_name(exception: *mut ext_php_rs::ffi::zend_object) -> Option<String> {
        if exception.is_null() {
            return None;
        }
        let ce = (*exception).ce;
        if ce.is_null() {
            return None;
        }
        let class_name = (*ce).name;
        if class_name.is_null() {
            return None;
        }
        Some(
            CStr::from_ptr((*class_name).val.as_ptr())
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Read one of a `Throwable`'s own properties (`message`, `code`, `file`, `line`) at
    /// throw time via `zend_read_property`. `silent` suppresses the engine's "undefined
    /// property" notice — every `Throwable` declares these, but a userland class that
    /// overrides the constructor without calling `parent::__construct()` can leave them
    /// unset. Reuses `zval_to_owned_string` below, which already handles both the string
    /// (`message`, `file`) and long (`code`, `line`) property types.
    unsafe fn read_exception_property(
        exception: *mut ext_php_rs::ffi::zend_object,
        name: &str,
    ) -> Option<String> {
        let ce = (*exception).ce;
        if ce.is_null() {
            return None;
        }
        let name = std::ffi::CString::new(name).ok()?;
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = ext_php_rs::ffi::zend_read_property(
            ce,
            exception,
            name.as_ptr(),
            name.as_bytes().len(),
            true,
            rv.as_mut_ptr(),
        );
        if prop.is_null() {
            return None;
        }
        zval_to_owned_string(prop)
    }

    /// # Safety
    /// `exception` must be a valid pointer to a `Throwable`'s `zend_object`, as passed to
    /// `zend_throw_exception_hook`.
    pub unsafe fn exception_identity(
        exception: *mut ext_php_rs::ffi::zend_object,
    ) -> Option<ExceptionIdentity> {
        let class = exception_class_name(exception)?;
        Some(ExceptionIdentity {
            class,
            message: read_exception_property(exception, "message"),
            code: read_exception_property(exception, "code"),
            file: read_exception_property(exception, "file"),
            line: read_exception_property(exception, "line"),
        })
    }

    /// Pointer to the i-th (0-based) argument zval of the observed call.
    /// ZEND_CALL_FRAME_SLOT: zend_execute_data is zval-aligned, so advancing one
    /// zend_execute_data lands exactly on the first argument slot.
    unsafe fn arg_zval(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> *mut ext_php_rs::ffi::zval {
        let argc = (*execute_data).This.u2.num_args as usize;
        if index >= argc {
            return std::ptr::null_mut();
        }
        (execute_data.add(1) as *mut ext_php_rs::ffi::zval).add(index)
    }

    const IS_NULL: u8 = 1;
    const IS_LONG: u8 = 4;
    const IS_DOUBLE: u8 = 5;
    const IS_STRING: u8 = 6;
    const IS_ARRAY: u8 = 7;
    const IS_OBJECT: u8 = 8;
    const IS_RESOURCE: u8 = 9;
    const IS_TRUE: u8 = 3;
    const IS_FALSE: u8 = 2;

    unsafe fn zval_type(zv: *const ext_php_rs::ffi::zval) -> u8 {
        (*zv).u1.v.type_
    }

    pub unsafe fn arg_object_handle(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<u32> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_OBJECT {
            return None;
        }
        let obj = (*zv).value.obj;
        if obj.is_null() {
            return None;
        }
        Some((*obj).handle)
    }

    /// Handle id of the object a METHOD call is invoked on (`$this`) — how the
    /// prepared-statement map is keyed at `mysqli_stmt::execute` /
    /// `SQLite3Stmt::execute` time, matching the key banked off the prepare call's
    /// return value. `None` for a plain function call or a static call, which
    /// have no bound object.
    pub unsafe fn this_object_handle(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<u32> {
        if execute_data.is_null() {
            return None;
        }
        let this = std::ptr::addr_of!((*execute_data).This) as *const ext_php_rs::ffi::zval;
        if zval_type(this) != IS_OBJECT {
            return None;
        }
        let obj = (*this).value.obj;
        if obj.is_null() {
            return None;
        }
        Some((*obj).handle)
    }

    /// Whether a return value is the boolean `true` — how `mysqli_stmt::prepare`
    /// reports success. A null retval pointer (a call unwinding on an exception)
    /// reads as failure, which is the conservative answer for a banking decision.
    pub unsafe fn retval_is_true(retval: *mut ext_php_rs::ffi::zval) -> bool {
        !retval.is_null() && zval_type(retval) == IS_TRUE
    }

    /// Handle id of an object RETURN VALUE (`mysqli::prepare` returning a
    /// `mysqli_stmt`). `None` when the call returned anything else — a failed
    /// prepare returns `false`, and there is nothing to key against then.
    pub unsafe fn retval_object_handle(retval: *mut ext_php_rs::ffi::zval) -> Option<u32> {
        if retval.is_null() || zval_type(retval) != IS_OBJECT {
            return None;
        }
        let obj = (*retval).value.obj;
        if obj.is_null() {
            return None;
        }
        Some((*obj).handle)
    }

    pub unsafe fn arg_long(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<i64> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_LONG {
            return None;
        }
        Some((*zv).value.lval)
    }

    unsafe fn zval_to_owned_string(zv: *const ext_php_rs::ffi::zval) -> Option<String> {
        match zval_type(zv) {
            IS_STRING => {
                let s = (*zv).value.str_;
                if s.is_null() {
                    return None;
                }
                let len = (*s).len;
                let ptr = (*s).val.as_ptr() as *const u8;
                Some(String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned())
            }
            IS_LONG => Some((*zv).value.lval.to_string()),
            IS_DOUBLE => Some((*zv).value.dval.to_string()),
            IS_TRUE => Some("true".to_owned()),
            IS_FALSE => Some("false".to_owned()),
            _ => None,
        }
    }

    /// A scalar retval as a bounded string (for DST recordings). Non-scalars yield None.
    pub unsafe fn scalar_to_string(zv: *mut ext_php_rs::ffi::zval) -> Option<String> {
        if zv.is_null() {
            return None;
        }
        zval_to_owned_string(zv).map(|mut s| {
            s.truncate(512);
            s
        })
    }

    /// Read arg `index` as a bounded scalar string (payload detail capture).
    pub unsafe fn arg_scalar_string(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        max: usize,
    ) -> Option<String> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() {
            return None;
        }
        zval_to_owned_string(zv).map(|mut s| {
            if s.len() > max {
                let mut end = max;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
            }
            s
        })
    }

    /// Map a zval's type tag onto the Tier 3 argument vocabulary.
    ///
    /// Anything unrecognised reads as `Null`, which carries a type and no value — the
    /// conservative answer, and never a guess at content.
    unsafe fn argument_type_of(
        zv: *const ext_php_rs::ffi::zval,
    ) -> crate::deterministic::ArgumentType {
        use crate::deterministic::ArgumentType;
        match zval_type(zv) {
            IS_TRUE | IS_FALSE => ArgumentType::Bool,
            IS_LONG => ArgumentType::Int,
            IS_DOUBLE => ArgumentType::Float,
            IS_STRING => ArgumentType::Str,
            IS_ARRAY => ArgumentType::Array,
            IS_OBJECT => ArgumentType::Object,
            IS_RESOURCE => ArgumentType::Resource,
            IS_NULL => ArgumentType::Null,
            // IS_UNDEF, IS_REFERENCE and anything a future PHP adds. Never a guess at
            // content: an unrecognised tag reports a type and no value.
            _ => ArgumentType::Null,
        }
    }

    /// The declared parameter name at `index`, when the runtime exposes one.
    ///
    /// Only read for USER functions: an internal function's `arg_info` is a
    /// `zend_internal_arg_info` whose `name` is a plain `char*`, a different layout at
    /// the same offset, and reading one as the other would be a wild dereference.
    unsafe fn declared_argument_name(
        func: *mut ext_php_rs::ffi::zend_function,
        index: usize,
    ) -> String {
        if func.is_null() || (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8 {
            return String::new();
        }
        let arg_info = (*func).common.arg_info;
        if arg_info.is_null() || index >= (*func).common.num_args as usize {
            return String::new();
        }
        let info = arg_info.add(index);
        let name = (*info).name;
        if name.is_null() {
            return String::new();
        }
        CStr::from_ptr((*name).val.as_ptr())
            .to_string_lossy()
            .into_owned()
    }

    /// TIER 3: this call's arguments as bounded, typed, redacted records.
    ///
    /// SCALARS ONLY. An object, array or resource records its TYPE and never its value:
    /// "argument 3 was an array" is evidence and leaks nothing, while the array itself is
    /// unbounded application data that ADR 0017 refuses. The refusal lives in
    /// `deterministic::capture_argument`, before redaction, so no reordering of this
    /// function can serialise an object's contents.
    ///
    /// Redaction reuses the collector's existing pattern list, so an `$apiToken`
    /// parameter is masked by exactly the rule that masks an `Authorization` header.
    ///
    /// Returns `(captured, dropped_by_count)`. The caller is responsible for having
    /// checked the manifest allowlist and the Tier 3 arming gate — this function is a
    /// reader, not a policy.
    pub unsafe fn capture_scalar_arguments(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        max_arguments: usize,
        max_bytes: usize,
    ) -> (Vec<crate::deterministic::CapturedArgument>, u32) {
        if execute_data.is_null() {
            return (Vec::new(), 0);
        }
        let func = (*execute_data).func;
        let argc = (*execute_data).This.u2.num_args as usize;
        let taken = argc.min(max_arguments);
        let dropped = u32::try_from(argc.saturating_sub(taken)).unwrap_or(u32::MAX);
        let mut captured = Vec::with_capacity(taken);
        for index in 0..taken {
            let zv = arg_zval(execute_data, index);
            let (argument_type, raw) = if zv.is_null() {
                (crate::deterministic::ArgumentType::Null, None)
            } else {
                let argument_type = argument_type_of(zv);
                let raw = if argument_type.carries_value() {
                    zval_to_owned_string(zv)
                } else {
                    None
                };
                (argument_type, raw)
            };
            let name = declared_argument_name(func, index);
            let redact = crate::http_capture::redacts_identifier(&name);
            captured.push(crate::deterministic::capture_argument(
                u32::try_from(index).unwrap_or(u32::MAX),
                name,
                argument_type,
                raw,
                redact,
                max_bytes,
            ));
        }
        (captured, dropped)
    }

    /// Read arg `index` as a PHP list of strings.
    pub unsafe fn arg_string_array(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<Vec<String>> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_ARRAY {
            return None;
        }
        array_string_values((*zv).value.arr)
    }

    /// Read arg `index` as a PHP array, returning the string-list value at integer key `key`.
    pub unsafe fn arg_array_key_string_array(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        key: i64,
    ) -> Option<Vec<String>> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_ARRAY {
            return None;
        }
        let arr = (*zv).value.arr;
        if arr.is_null() {
            return None;
        }
        let ht = &*(arr as *const ext_php_rs::types::ZendHashTable);
        let value = ht.get_index(key)?;
        let vz = value as *const ext_php_rs::types::Zval as *const ext_php_rs::ffi::zval;
        if zval_type(vz) != IS_ARRAY {
            return None;
        }
        array_string_values((*vz).value.arr)
    }

    unsafe fn array_string_values(arr: *mut ext_php_rs::ffi::zend_array) -> Option<Vec<String>> {
        if arr.is_null() {
            return None;
        }
        let ht = &*(arr as *const ext_php_rs::types::ZendHashTable);
        let mut out = Vec::new();
        for (_key, value) in ht.iter() {
            let vz = value as *const ext_php_rs::types::Zval as *const ext_php_rs::ffi::zval;
            if let Some(s) = zval_to_owned_string(vz) {
                out.push(s);
            }
            if out.len() >= 64 {
                break;
            }
        }
        Some(out)
    }

    /// Call `curl_setopt($ch, CURLOPT_HTTPHEADER, $headers)` on the handle in arg 0
    /// of the observed `curl_exec` call.
    pub unsafe fn call_curl_setopt_httpheader(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        headers: &[String],
    ) {
        let ch = arg_zval(execute_data, 0);
        if ch.is_null() {
            return;
        }
        let Some(func) = ext_php_rs::zend::Function::try_from_function("curl_setopt") else {
            return;
        };
        let ch_ref = &*(ch as *const ext_php_rs::types::Zval);
        let headers_vec: Vec<String> = headers.to_vec();
        let _ = func.try_call(vec![ch_ref, &super::CURLOPT_HTTPHEADER, &headers_vec]);
    }

    /// The whole `curl_getinfo($ch)` associative array, flattened to strings.
    ///
    /// The one-argument form is used deliberately: asking for each CURLINFO_* constant
    /// individually would mean hard-coding a dozen numeric constants and paying a PHP
    /// call for each, and the constants' *values* have moved between curl releases in
    /// ways the array keys never have.
    pub unsafe fn curl_info(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> std::collections::HashMap<String, String> {
        use ext_php_rs::types::ArrayKey;
        let mut out = std::collections::HashMap::new();
        let ch = arg_zval(execute_data, 0);
        if ch.is_null() {
            return out;
        }
        let Some(func) = ext_php_rs::zend::Function::try_from_function("curl_getinfo") else {
            return out;
        };
        let ch_ref = &*(ch as *const ext_php_rs::types::Zval);
        let Ok(result) = func.try_call(vec![ch_ref]) else {
            return out;
        };
        let Some(table) = result.array() else {
            return out;
        };
        for (key, value) in table.iter() {
            let key = match key {
                ArrayKey::String(k) => k,
                ArrayKey::Str(k) => k.to_owned(),
                _ => continue,
            };
            // curl mixes strings, longs and doubles in one array; the timings are the
            // doubles, so a string-only read would silently drop the whole timeline.
            let value = if let Some(text) = value.str() {
                text.to_owned()
            } else if let Some(number) = value.long() {
                number.to_string()
            } else if let Some(number) = value.double() {
                format!("{number}")
            } else {
                continue;
            };
            out.insert(key, value);
        }
        out
    }

    /// A returned zval as a String, but ONLY when it really is one. `curl_exec`
    /// without CURLOPT_RETURNTRANSFER returns `true`, and coercing that to "1" would
    /// present a one-byte lie as the response body.
    pub unsafe fn string_retval(retval: *mut ext_php_rs::ffi::zval) -> Option<String> {
        if retval.is_null() {
            return None;
        }
        let zv = &*(retval as *const ext_php_rs::types::Zval);
        zv.str()
            .map(std::borrow::ToOwned::to_owned)
            .filter(|s| !s.is_empty())
    }

    /// The URL configured on the curl handle in arg 0 of the observed
    /// `curl_exec` call, via `curl_getinfo($ch, CURLINFO_EFFECTIVE_URL)`.
    pub unsafe fn curl_effective_url(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<String> {
        const CURLINFO_EFFECTIVE_URL: i64 = 0x0010_0000 + 1;
        let ch = arg_zval(execute_data, 0);
        if ch.is_null() {
            return None;
        }
        let func = ext_php_rs::zend::Function::try_from_function("curl_getinfo")?;
        let ch_ref = &*(ch as *const ext_php_rs::types::Zval);
        let result = func.try_call(vec![ch_ref, &CURLINFO_EFFECTIVE_URL]).ok()?;
        let url = result.string()?;
        if url.is_empty() {
            return None;
        }
        let mut url = url;
        url.truncate(512);
        Some(url)
    }
}

fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

fn monotonic_nanos() -> u128 {
    use std::time::Instant;
    thread_local! { static ORIGIN: Instant = Instant::now(); }
    ORIGIN.with(|origin| origin.elapsed().as_nanos())
}

/// The deterministic aggregate's clock, in `u64` nanoseconds.
///
/// THE SAME ORIGIN as [`monotonic_nanos`] above, and that matters: there are three
/// independent `monotonic_nanos` implementations in this crate (here, `sampler.rs`,
/// `lib.rs`), each with its own private origin, so values are NOT comparable across
/// modules. Frame durations are already computed against this one, so the counted
/// aggregate uses it too — a begin stamped by one clock and an end by another would
/// produce durations that are plausible, wrong, and impossible to spot.
///
/// Saturating rather than wrapping on the cast: `u64` nanoseconds is 584 years of
/// process uptime, so the clamp is unreachable, and a wrap would silently produce a
/// negative-looking duration where a clamp produces an obviously stuck one.
fn monotonic_nanos_u64() -> u64 {
    u64::try_from(monotonic_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dual-purpose stream calls ----------------------------------------

    #[test]
    fn a_remote_target_is_recognised_whatever_the_scheme_case() {
        for target in [
            "http://api.internal/orders",
            "HTTPS://api.internal/orders",
            "ftp://files.internal/report.csv",
            "sftp://files.internal/report.csv",
            "ssh2.sftp://files.internal/report.csv",
        ] {
            assert!(
                is_remote_stream_target(target),
                "{target} is a network fetch"
            );
        }
    }

    #[test]
    fn local_reads_are_never_mistaken_for_network_calls() {
        // These are the calls that were filling the waterfall with 0 ms rows. The
        // allow-list decides, so an unlisted wrapper reads as local.
        for target in [
            "/srv/app/config/routing.yml",
            "config/app.php",
            "php://input",
            "data://text/plain,hello",
            "compress.zlib:///srv/app/cache/x.gz",
            "",
        ] {
            assert!(!is_remote_stream_target(target), "{target} is a local read");
        }
    }

    #[test]
    fn only_the_ambiguous_stream_builtins_have_their_target_inspected() {
        assert!(is_dual_purpose_stream_function("file_get_contents"));
        // curl always crosses the network — nothing to disambiguate, and reading its
        // handle argument as a string would be meaningless.
        assert!(!is_dual_purpose_stream_function("curl_exec"));
        assert!(!is_dual_purpose_stream_function("PDO::query"));
    }

    // --- Excluded paths ----------------------------------------------------

    #[test]
    fn dependency_paths_are_excluded_and_application_paths_are_not() {
        let excluded = DEFAULT_EXCLUDED_PATHS
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>();
        assert!(is_excluded_path(
            "/srv/app/vendor/symfony/Kernel.php",
            &excluded
        ));
        assert!(is_excluded_path(
            "/srv/app/node_modules/x/index.php",
            &excluded
        ));
        assert!(!is_excluded_path(
            "/srv/app/src/Orders/Controller.php",
            &excluded
        ));
        // "vendor" as part of an application's own name is not the dependency tree;
        // the fragments carry their separators for exactly this reason.
        assert!(!is_excluded_path(
            "/srv/app/src/VendorPayouts.php",
            &excluded
        ));
    }

    #[test]
    fn an_unknown_defining_file_is_never_excluded() {
        // Absence of a path is not evidence that the code is a dependency, and
        // guessing would silently drop spans the reader asked for.
        let excluded = vec!["/vendor/".to_owned()];
        assert!(!is_excluded_path("", &excluded));
    }

    #[test]
    fn the_exclude_list_ignores_blank_entries() {
        // A stray comma would otherwise contribute an empty fragment, which
        // `contains` matches against every path — silencing the whole service.
        let parsed = parse_excluded_paths("/vendor/, ,/node_modules/,");
        assert_eq!(
            parsed,
            vec!["/vendor/".to_owned(), "/node_modules/".to_owned()]
        );
        assert!(!is_excluded_path("/srv/app/src/Controller.php", &parsed));
    }

    #[test]
    fn an_empty_exclude_list_excludes_nothing() {
        let parsed = parse_excluded_paths("");
        assert!(parsed.is_empty());
        assert!(!is_excluded_path(
            "/srv/app/vendor/symfony/Kernel.php",
            &parsed
        ));
    }

    // --- Propagation header merging -----------------------------------------

    #[test]
    fn all_three_propagation_headers_are_appended_to_the_applications_own() {
        let mut headers = vec!["Accept: application/json".to_owned()];
        merge_propagation_headers(
            &mut headers,
            "00-abc-def-01",
            Some("vendor=state"),
            Some("userId=1"),
        );
        assert_eq!(
            headers,
            vec![
                "Accept: application/json".to_owned(),
                "traceparent: 00-abc-def-01".to_owned(),
                "tracestate: vendor=state".to_owned(),
                "baggage: userId=1".to_owned(),
            ]
        );
    }

    #[test]
    fn a_retried_handle_never_stacks_duplicate_propagation_headers() {
        let mut headers = vec![
            "TraceParent: 00-old-old-01".to_owned(),
            "tracestate: stale=1".to_owned(),
            "Baggage: stale=1".to_owned(),
        ];
        merge_propagation_headers(&mut headers, "00-new-new-01", Some("fresh=1"), Some("k=v"));
        assert_eq!(
            headers,
            vec![
                "traceparent: 00-new-new-01".to_owned(),
                "tracestate: fresh=1".to_owned(),
                "baggage: k=v".to_owned(),
            ]
        );
    }

    #[test]
    fn an_applications_own_tracestate_survives_when_the_request_carried_none() {
        // We only dedupe a header we are about to re-add. An application that set
        // its own tracestate on a request with no inbound one keeps it.
        let mut headers = vec!["tracestate: mine=1".to_owned()];
        merge_propagation_headers(&mut headers, "00-abc-def-01", None, None);
        assert_eq!(
            headers,
            vec![
                "tracestate: mine=1".to_owned(),
                "traceparent: 00-abc-def-01".to_owned(),
            ]
        );
    }

    // --- Policy ------------------------------------------------------------

    #[test]
    fn engine_builtins_that_are_not_io_never_reach_a_span() {
        assert!(observe_policy("strpos", true).is_none());
        assert!(observe_policy("file_exists", true).is_none());
    }

    #[test]
    fn file_get_contents_still_attaches_handlers_so_its_target_can_be_judged() {
        // The factory's verdict is cached per function by Zend, so the policy here must
        // stay IoSpan; the per-call demotion to ObserveOnly happens in the begin
        // handler, which is the only place the argument exists.
        assert_eq!(
            observe_policy("file_get_contents", true),
            Some(SpanPolicy::IoSpan)
        );
    }
}
