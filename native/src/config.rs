//! Full collector configuration, resolved once at RINIT from INI/env.
//! Mirrors the PHP `EnvironmentLocalCollectorConfiguration` so all gating decisions
//! happen in Rust before any PHP code runs.

use crate::context::CollectorEnvelope;
use crate::http_capture::HttpCaptureConfig;
use crate::settings;

#[derive(Clone, Debug)]
pub struct CollectorConfig {
    pub enabled: bool,
    pub envelope: Option<CollectorEnvelope>,
    pub apm_enabled: bool,
    pub apm_sample_rate_bps: u32,
    pub logs_enabled: bool,
    pub profiler_enabled: bool,
    pub profile_sample_rate_hz: u32,
    pub dst_enabled: bool,
    pub runtime_metrics_enabled: bool,
    pub rich_telemetry: bool,
    pub log_sink: String,
    pub metrics_sink: String,
    /// Full HTTP stack capture (headers, cookies, query, bodies, phase timeline).
    pub http_capture: HttpCaptureConfig,
}

impl CollectorConfig {
    pub fn resolve() -> Self {
        let enabled = flag("CHRONOS_PHP_ENABLED", false);
        if !enabled {
            return Self::disabled();
        }

        Self {
            enabled: true,
            envelope: CollectorEnvelope::resolve(),
            apm_enabled: flag("CHRONOS_PHP_APM_ENABLED", false),
            apm_sample_rate_bps: env_u32("CHRONOS_PHP_APM_SAMPLE_RATE", 10000),
            logs_enabled: flag("CHRONOS_PHP_LOGS_ENABLED", false),
            profiler_enabled: flag("CHRONOS_PHP_PROFILER_ENABLED", false),
            profile_sample_rate_hz: env_u32("CHRONOS_PHP_PROFILE_SAMPLE_RATE", 99),
            // Process-wide DST is refused in production: continuous effect capture would
            // inflate latency and is a sharper privacy surface than APM. Production arms
            // DST only via X-Chronos-DST / chronos_dst (see lib.rs directive_records).
            dst_enabled: flag("CHRONOS_PHP_DST_ENABLED", false)
                && allow_process_wide_dst(settings::get("CHRONOS_PHP_ENV").as_deref()),
            runtime_metrics_enabled: flag("CHRONOS_PHP_RUNTIME_METRICS_ENABLED", false),
            rich_telemetry: flag("CHRONOS_PHP_LOCAL_RICH_TELEMETRY", false),
            log_sink: env_string("CHRONOS_PHP_LOG_SINK", ""),
            metrics_sink: env_string("CHRONOS_PHP_METRICS_SINK", ""),
            // On by default, redacted and capped: a trace that cannot show the
            // request that produced it sends the reader back to the application
            // logs, which is the gap this whole capture exists to close.
            http_capture: HttpCaptureConfig::resolve(),
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            envelope: None,
            apm_enabled: false,
            apm_sample_rate_bps: 0,
            logs_enabled: false,
            profiler_enabled: false,
            profile_sample_rate_hz: 0,
            dst_enabled: false,
            runtime_metrics_enabled: false,
            rich_telemetry: false,
            log_sink: String::new(),
            metrics_sink: String::new(),
            http_capture: HttpCaptureConfig::default(),
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
