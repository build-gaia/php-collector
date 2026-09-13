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
    /// The inbound `tracestate` header, VERBATIM. W3C Trace Context requires a
    /// participant to forward tracestate it does not understand — the vendor
    /// entries belong to other tracers sharing the trace, and dropping them
    /// severs their correlation. Never parsed here: the collector adds no entry
    /// of its own, so pass-through is the whole contract. Bounded at capture
    /// (see `lib.rs`), because a header is caller-controlled input.
    pub tracestate: Option<String>,
    /// The inbound W3C `baggage` header, VERBATIM, for the same forward-as-is
    /// reason as `tracestate` above.
    pub baggage: Option<String>,
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
            tracestate: None,
            baggage: None,
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
            // Stamped by `start_request` after parsing: propagation headers ride
            // NEXT TO the trace identity, they are not derived from it.
            tracestate: None,
            baggage: None,
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

/// The organisation/project/application identity envelope stamped onto every native span.
///
/// All four identity fields (organisation, project/team, application, spool directory)
/// resolve through the unified `settings` layer: process env > `chronos.*` INI >
/// `.chronos` file. That precedence IS the security property, not an implementation
/// detail:
///
/// - The `.chronos` file ships alongside the application's own code. Anyone who can
///   edit it can already run code in this process, so trusting its contents grants no
///   capability an attacker did not already have — a `.chronos` file is no more
///   dangerous than the composer package it rides in.
/// - Env and INI, by contrast, are set by whoever controls the platform UNDER the
///   application — the container's env, the FPM pool config, php.ini — and a value
///   fixed at that layer can never be overridden by a file the application ships. A
///   platform operator who pins identity in env or php.ini keeps that guarantee
///   absolutely: nothing an application deploys can shadow it.
///
/// On top of the ordering, file-sourced identity values are VALIDATED before they are
/// trusted (`is_identifier_shaped` / `is_safe_spool_path` below, via
/// `settings::get_validated` / `settings::first_validated`): organisation, project/team
/// and application must look like an identifier, and the spool directory must be an
/// absolute path with no `..` segment. Env and INI values are never validated — an
/// operator does not need protecting from their own platform configuration — only the
/// file tier, which nobody with platform authority necessarily reviewed. A file value
/// that fails validation is treated as ABSENT (never half-applied), so a mis-shaped
/// `.chronos` line falls back to "no value from the file" rather than becoming a
/// mangled tenant id or an unintended write target.
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

    /// Resolve identity through the settings layer (env > INI > `.chronos` file — see
    /// the struct docblock for why that order is the whole security model), validating
    /// whatever the `.chronos` file supplies. Returns None when a required identity is
    /// missing (or the only value offered for it was rejected), so the caller can stay
    /// inert rather than emit anonymous or mistenanted spans.
    ///
    /// Deliberately NOT called at MINIT: `settings::startup_flag` exists precisely
    /// because `.chronos` cannot be read there (the SAPI's cwd, not the application's,
    /// is current at module startup — see that function's own docblock). `resolve()`
    /// runs per-request, after RINIT, where `sources()` walking up from the request's
    /// document root / script path is valid. This is not something to "fix" — a flag
    /// resolved at MINIT and an identity resolved per-request are answering different
    /// questions at different times, and only one of them has a request to answer from.
    pub fn resolve() -> Option<Self> {
        let organisation_id =
            crate::settings::get_validated("CHRONOS_PHP_ORGANISATION", is_identifier_shaped)?;
        // A team IS a project; `team_id` is the spelling the product uses now, and the
        // one a service writes into its own `.chronos` to declare who owns it. `project`
        // stays readable — deployments are already carrying it — but loses to `team_id`
        // so adding the new name to a file that still has the old one is unambiguous.
        let project_id = crate::settings::first_validated(
            &["CHRONOS_PHP_TEAM_ID", "CHRONOS_PHP_PROJECT"],
            is_identifier_shaped,
        )?;
        let application_id =
            crate::settings::get_validated("CHRONOS_PHP_APPLICATION", is_identifier_shaped)?;
        let spool_directory =
            crate::settings::get_validated("CHRONOS_PHP_SPOOL_DIRECTORY", is_safe_spool_path)?;
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

/// Whether a `.chronos`-file-sourced organisation/project/application id looks like an
/// identifier, rather than a copy-paste accident (a stray `;`, a whole INI line pasted
/// into the wrong slot, an empty string). The estate's own ids are `org_<uuid>` or
/// kebab-case names, so the allowed alphabet is deliberately generous — `[A-Za-z0-9._-]`,
/// 1 to 128 bytes — because this exists to catch garbage, not to enforce the estate's
/// naming convention on every future id shape.
fn is_identifier_shaped(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether a `.chronos`-file-sourced spool directory is safe to use as a filesystem
/// write target: an absolute path (so it never resolves against whatever cwd the
/// worker happens to have) with no `..` segment (so it can never walk the write outside
/// the intended spool tree, once `tenant_spool_directory` joins a tenant subdirectory
/// onto it).
fn is_safe_spool_path(value: &str) -> bool {
    value.starts_with('/') && !value.split('/').any(|segment| segment == "..")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_shape_accepts_the_estate_s_real_ids() {
        assert!(is_identifier_shaped(
            "org_a2a69137-6d90-49cc-90b9-5d3e49f1ef96"
        ));
        assert!(is_identifier_shaped("qls-shipping"));
        assert!(is_identifier_shaped("project_qls"));
        assert!(is_identifier_shaped("a"));
    }

    #[test]
    fn identifier_shape_rejects_garbage() {
        assert!(!is_identifier_shaped(""));
        assert!(!is_identifier_shaped("bad;value"));
        assert!(!is_identifier_shaped("has space"));
        assert!(!is_identifier_shaped("quote\"d"));
        assert!(!is_identifier_shaped(&"x".repeat(129)));
        // Exactly at the boundary must still pass.
        assert!(is_identifier_shaped(&"x".repeat(128)));
    }

    #[test]
    fn spool_path_accepts_absolute_paths_without_traversal() {
        assert!(is_safe_spool_path("/var/lib/chronos/php"));
        assert!(is_safe_spool_path("/"));
    }

    #[test]
    fn spool_path_rejects_relative_and_traversal() {
        assert!(!is_safe_spool_path("relative/path"));
        assert!(!is_safe_spool_path(""));
        assert!(!is_safe_spool_path("/var/lib/../../etc"));
        assert!(!is_safe_spool_path("/var/lib/.."));
    }
}
