//! Full collector configuration, resolved once at RINIT from INI/env.
//! Mirrors the PHP `EnvironmentLocalCollectorConfiguration` so all gating decisions
//! happen in Rust before any PHP code runs.

use crate::context::CollectorEnvelope;
use crate::deterministic::DeterministicConfig;
use crate::http_capture::HttpCaptureConfig;
use crate::rate::{self, SampleRate};
use crate::settings;

#[derive(Clone, Debug)]
pub struct CollectorConfig {
    pub enabled: bool,
    pub envelope: Option<CollectorEnvelope>,
    pub apm_enabled: bool,
    /// What fraction of locally-rooted requests get a trace. Written as a
    /// fraction (`1` is everything, `0.1` a tenth); see [`crate::rate`] for the
    /// resolution it is quantised to and the basis-point migration.
    pub apm_sample_rate: SampleRate,
    pub logs_enabled: bool,
    pub profiler_enabled: bool,
    /// Stack-walk FREQUENCY in hertz (99 = ~1 sample per 10 ms of CPU). This is
    /// how densely a profiled request is sampled, NOT how many requests are
    /// profiled — the two were one knob for long enough that the names still
    /// look alike, so read `profile_request_rate` (or `profile_job_rate`) for the second.
    pub profile_sample_rate_hz: u32,
    /// What fraction of WEB requests get profiled. Zero — the default — means
    /// the profiler only ever runs on a request that explicitly asked for it
    /// via the directive below.
    ///
    /// Separate from `apm_sample_rate` because the two costs are nothing alike:
    /// a trace is a handful of spans, a profile is a timer plus a stack walk
    /// every 10 ms. Before this existed, profiling rode the APM verdict
    /// outright, so the only way to profile 1% of traffic was to throw away 99%
    /// of your traces with it.
    pub profile_request_rate: SampleRate,
    /// What fraction of BACKGROUND JOBS get profiled. Defaults to a tenth,
    /// unlike the web rate, and the asymmetry is deliberate.
    ///
    /// A job is the opposite of a web request in every way that decides this
    /// number. It is low volume, so a tenth of them is a handful rather than a
    /// flood. It is long-running, so the per-request profiling overhead is
    /// amortised over something that was already slow. And nobody is watching
    /// it — a slow endpoint gets reported by a user within the hour, while a
    /// slow nightly job is discovered by reading a graph weeks later, which is
    /// exactly the situation a profile that was already captured rescues.
    ///
    /// This only bites where jobs are ALREADY traced: native CLI needs
    /// `cli_enabled`, and a framework worker has to open a job through the SDK
    /// bridge. A service that never instrumented its workers profiles nothing.
    pub profile_job_rate: SampleRate,
    /// Shared secret that arms the forced-profile directive
    /// (`X-Chronos-Profile: <token>` / `chronos_profile=<token>`). EMPTY
    /// disables the directive entirely, which is the default: a header that
    /// makes the server do materially more work is an abuse surface, and an
    /// unauthenticated one on a public endpoint lets anyone put every request
    /// on the expensive path.
    pub profile_token: String,
    pub dst_enabled: bool,
    pub rich_telemetry: bool,
    pub log_sink: String,
    pub metrics_sink: String,
    /// Full HTTP stack capture (headers, cookies, query, bodies, phase timeline).
    pub http_capture: HttpCaptureConfig,
    /// Deterministic (counted) per-function aggregates — ADR 0029.
    ///
    /// Not gated by `profiler_enabled` and deliberately not by the sample verdict:
    /// this is the COUNTED sibling of the statistical profiler, its buffer is
    /// O(distinct functions) rather than O(calls), and Tier 1 is therefore always on
    /// with a kill switch rather than sampled. A service that has the statistical
    /// profiler switched off still gets exact call counts.
    pub deterministic: DeterministicConfig,
}

impl CollectorConfig {
    pub fn resolve() -> Self {
        let enabled = flag("CHRONOS_PHP_ENABLED", false);
        if !enabled {
            return Self::disabled();
        }
        // One resolution for every rate in the process: mixing them would make
        // two rates written identically mean different things.
        let denominator = rate::clamp_denominator(env_u32(
            "CHRONOS_PHP_SAMPLE_RATE_DENOMINATOR",
            rate::DEFAULT_DENOMINATOR,
        ));

        Self {
            enabled: true,
            envelope: CollectorEnvelope::resolve(),
            apm_enabled: flag("CHRONOS_PHP_APM_ENABLED", false),
            apm_sample_rate: resolve_rate("CHRONOS_PHP_APM_SAMPLE_RATE", 1.0, denominator),
            logs_enabled: flag("CHRONOS_PHP_LOGS_ENABLED", false),
            profiler_enabled: flag("CHRONOS_PHP_PROFILER_ENABLED", false),
            profile_sample_rate_hz: env_u32("CHRONOS_PHP_PROFILE_SAMPLE_RATE", 99),
            // Off by default. A service that wants continuous coverage sets a
            // rate; everyone else pays nothing until someone asks for a profile.
            profile_request_rate: resolve_rate(
                "CHRONOS_PHP_PROFILE_REQUEST_RATE",
                0.0,
                denominator,
            ),
            profile_job_rate: resolve_rate("CHRONOS_PHP_PROFILE_JOB_RATE", 0.1, denominator),
            profile_token: env_string("CHRONOS_PHP_PROFILE_TOKEN", ""),
            // Process-wide DST is refused in production: continuous effect capture would
            // inflate latency and is a sharper privacy surface than APM. Production arms
            // DST only via X-Chronos-DST / chronos_dst (see lib.rs directive_records).
            dst_enabled: flag("CHRONOS_PHP_DST_ENABLED", false)
                && allow_process_wide_dst(settings::get("CHRONOS_PHP_ENV").as_deref()),
            rich_telemetry: flag("CHRONOS_PHP_LOCAL_RICH_TELEMETRY", false),
            log_sink: env_string("CHRONOS_PHP_LOG_SINK", ""),
            metrics_sink: env_string("CHRONOS_PHP_METRICS_SINK", ""),
            // On by default, redacted and capped: a trace that cannot show the
            // request that produced it sends the reader back to the application
            // logs, which is the gap this whole capture exists to close.
            http_capture: HttpCaptureConfig::resolve(),
            // Resolved even when the statistical profiler is off: the two signals share
            // a name and nothing else. See the field's doc comment.
            deterministic: DeterministicConfig::resolve(),
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            envelope: None,
            apm_enabled: false,
            apm_sample_rate: SampleRate::off(rate::DEFAULT_DENOMINATOR),
            logs_enabled: false,
            profiler_enabled: false,
            profile_sample_rate_hz: 0,
            profile_request_rate: SampleRate::off(rate::DEFAULT_DENOMINATOR),
            profile_job_rate: SampleRate::off(rate::DEFAULT_DENOMINATOR),
            profile_token: String::new(),
            dst_enabled: false,
            rich_telemetry: false,
            log_sink: String::new(),
            metrics_sink: String::new(),
            http_capture: HttpCaptureConfig::default(),
            deterministic: DeterministicConfig::off(),
        }
    }
}

/// Process-wide `CHRONOS_PHP_DST_ENABLED` is for lab/CLI break-glass only.
/// When `CHRONOS_PHP_ENV` is production/prod, ignore it so APM/profiler latency
/// stays on their sample gates and DST stays header/cookie session-gated.
pub(crate) fn allow_process_wide_dst(env: Option<&str>) -> bool {
    match env.map(str::trim).map(|value| value.to_ascii_lowercase()) {
        Some(value) if value == "production" || value == "prod" => false,
        _ => true,
    }
}

fn flag(name: &str, default: bool) -> bool {
    settings::flag(name, default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    settings::u32_value(name, default)
}

/// Read one written rate and quantise it. `default_fraction` is what the setting
/// means when nobody wrote it at all.
fn resolve_rate(name: &str, default_fraction: f64, denominator: u32) -> SampleRate {
    rate::resolve(settings::f64_value(name, default_fraction), denominator)
}

fn env_string(name: &str, default: &str) -> String {
    settings::string(name, default)
}

#[cfg(test)]
mod tests {
    use super::allow_process_wide_dst;

    #[test]
    fn production_refuses_process_wide_dst() {
        assert!(!allow_process_wide_dst(Some("production")));
        assert!(!allow_process_wide_dst(Some("PROD")));
        assert!(!allow_process_wide_dst(Some(" production ")));
    }

    #[test]
    fn non_production_allows_process_wide_dst() {
        assert!(allow_process_wide_dst(None));
        assert!(allow_process_wide_dst(Some("")));
        assert!(allow_process_wide_dst(Some("local")));
        assert!(allow_process_wide_dst(Some("staging")));
    }
}
