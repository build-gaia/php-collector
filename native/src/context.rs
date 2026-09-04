//! W3C trace context parsing and the org/project/app envelope the native collector stamps onto every
//! span, sourced from INI/env exactly like the userland `EnvironmentLocalCollectorConfiguration` +
//! `TraceContext`. Kept dependency-light and non-panicking: a malformed traceparent yields a fresh
//! root context, never an error, mirroring userland fail-open behaviour.

use rand::RngCore;

/// The parsed inbound distributed-trace context, byte-compatible with userland `TraceContext`.
#[derive(Clone, Debug)]
pub struct TraceContext {
    pub trace_id: String, // 32 lowercase hex
    pub span_id: String,  // 16 lowercase hex (freshly minted server span)
    pub parent_span_id: Option<String>,
    pub sampled: bool,
    pub session_id: Option<String>,
}

impl TraceContext {
    /// Parse a `traceparent` header plus optional `x-chronos-session-id`. Absent/invalid input
    /// produces a new root context (new trace id, sampled = true) — the userland contract.
    pub fn from_header(traceparent: Option<&str>, session: Option<&str>) -> Self {
        let session_id = session
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        if let Some(raw) = traceparent.map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(parsed) = Self::parse_traceparent(raw, session_id.clone()) {
                return parsed;
            }
        }
        Self {
            trace_id: hex_bytes(16),
            span_id: hex_bytes(8),
            parent_span_id: None,
            sampled: true,
            session_id,
        }
    }

    fn parse_traceparent(raw: &str, session_id: Option<String>) -> Option<Self> {
        // 00-<32 hex>-<16 hex>-<2 hex>
        let parts: Vec<&str> = raw.split('-').collect();
        if parts.len() != 4 || parts[0] != "00" {
            return None;
        }
        let (trace, parent, flags) = (parts[1], parts[2], parts[3]);
        if trace.len() != 32
            || parent.len() != 16
            || flags.len() != 2
            || !is_hex(trace)
            || !is_hex(parent)
            || !is_hex(flags)
            || trace.bytes().all(|b| b == b'0')
            || parent.bytes().all(|b| b == b'0')
        {
            return None;
        }
        let sampled = u8::from_str_radix(flags, 16).ok()? & 1 == 1;
        Some(Self {
            trace_id: trace.to_ascii_lowercase(),
            span_id: hex_bytes(8),
            parent_span_id: Some(parent.to_ascii_lowercase()),
            sampled,
            session_id,
        })
    }

    /// Outbound `traceparent` for downstream propagation.
    pub fn header(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id,
            self.span_id,
            if self.sampled { "01" } else { "00" }
        )
    }
}

/// The organisation/project/application identity envelope stamped onto every native span, read from
/// INI (`chronos.*`) with an env fallback, matching userland `CHRONOS_PHP_*`.
#[derive(Clone, Debug)]
pub struct CollectorEnvelope {
    pub organisation_id: String,
    pub project_id: String,
    pub application_id: String,
    pub app_version: Option<String>,
    pub spool_directory: String,
    /// The runtime and framework serving the request (ADR 0024 §4). `app_language` is
    /// a constant here — this extension only ever runs inside PHP — while the three
    /// versions are filled in by `chronos_set_app_metadata`, because only userland can
    /// cheaply read `PHP_VERSION` and a framework's own version constant.
    pub app_language: String,
    pub app_language_version: Option<String>,
    pub app_framework: Option<String>,
    pub app_framework_version: Option<String>,
}

impl CollectorEnvelope {
    /// The tenant-scoped spool directory: `<spool_directory>/<organisation_id>`.
    ///
    /// One host (or one shared spool volume) can carry several tenants; the
    /// per-tenant subdirectory is what lets the draining collector — and a human
    /// with `ls` — attribute files to a tenant without opening them. The
    /// organisation id is sanitised to a path-safe alphabet so a mis-set env
    /// var can never become a traversal, and an id that sanitises to nothing
    /// falls back to the flat root (the pre-tenant layout, still drained).
    pub fn tenant_spool_directory(&self) -> String {
        let tenant: String = self
            .organisation_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let tenant = tenant.trim_matches('.').to_owned();
        if tenant.is_empty() {
            return self.spool_directory.clone();
        }
        format!("{}/{}", self.spool_directory.trim_end_matches('/'), tenant)
    }

    /// Resolve from INI first (an ext should prefer php.ini), then process env. Returns None when a
    /// required identity is missing, so the caller can stay inert rather than emit anonymous spans.
    pub fn resolve() -> Option<Self> {
        let organisation_id = setting("chronos.organisation", "CHRONOS_PHP_ORGANISATION")?;
        // A team IS a project; `team_id` is the spelling the product uses now, and the
        // one a service writes into its own `.chronos` to declare who owns it. `project`
        // stays readable — deployments are already carrying it — but loses to `team_id`
        // so adding the new name to a file that still has the old one is unambiguous.
        let project_id = crate::settings::first(&["CHRONOS_PHP_TEAM_ID", "CHRONOS_PHP_PROJECT"])
            .filter(|v| !v.is_empty())?;
        let application_id = setting("chronos.application", "CHRONOS_PHP_APPLICATION")?;
        let spool_directory = setting("chronos.spool_directory", "CHRONOS_PHP_SPOOL_DIRECTORY")?;
        let app_version = setting("chronos.app_version", "CHRONOS_APP_VERSION");
        Some(Self {
            organisation_id,
            project_id,
            application_id,
            app_version,
            spool_directory,
            app_language: "php".to_owned(),
            app_language_version: setting(
                "chronos.app_language_version",
                "CHRONOS_PHP_APP_LANGUAGE_VERSION",
            ),
            app_framework: setting("chronos.app_framework", "CHRONOS_PHP_APP_FRAMEWORK"),
            app_framework_version: setting(
                "chronos.app_framework_version",
                "CHRONOS_PHP_APP_FRAMEWORK_VERSION",
            ),
        })
    }
}

/// One setting, through the unified resolution layer (process env > `chronos.*` INI >
/// `.chronos` file). The INI name is passed for readability at the call site; the
/// lookup itself derives it, so both spellings stay in one place — `settings`.
fn setting(_ini_name: &str, env_name: &str) -> Option<String> {
    crate::settings::get(env_name).filter(|v| !v.is_empty())
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `n` random bytes rendered as lowercase hex (16 bytes -> 32-char trace id, 8 -> 16-char span id).
pub fn hex_bytes(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}
