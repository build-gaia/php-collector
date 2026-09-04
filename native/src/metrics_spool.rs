//! Runtime metrics capture and spool write. Schema `chronos.runtime.metrics.v1`.
//!
//! Captures process-level runtime metrics (memory, GC, timing) at RSHUTDOWN and writes a
//! single `.metrics` spool file per request. The engine-agent ships these to `/v1/metrics`.
//!
//! Unlike the PHP userland emitter, this reads metrics from `/proc/self/status` and the Rust
//! allocator rather than PHP's `memory_get_usage()`, avoiding the overhead of calling back
//! into the PHP runtime for data that's available natively.

use crate::context::CollectorEnvelope;
use crate::spool_common;
use serde_json::{json, Value};

const SCHEMA: &str = "chronos.runtime.metrics.v1";

pub struct RequestMetrics {
    pub trace_id: String,
    pub span_id: String,
    pub http_route: String,
    pub http_method: String,
    pub http_status_code: u16,
    pub request_start_ns: u128,
    pub request_end_ns: u128,
}

pub fn flush(
    envelope: &CollectorEnvelope,
    metrics: &RequestMetrics,
    app_version: Option<&str>,
) -> std::io::Result<()> {
    let duration_ms = (metrics
        .request_end_ns
        .saturating_sub(metrics.request_start_ns)) as f64
        / 1_000_000.0;

    let mut metric_map = serde_json::Map::new();
    metric_map.insert(
        "process.runtime.php.request.count".into(),
        Value::Number(1.into()),
    );
    metric_map.insert(
        "process.runtime.php.request.duration_ms".into(),
        json!(duration_ms),
    );

    // Memory from the Rust side — no PHP callback overhead.
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(value) = line.strip_prefix("VmRSS:") {
                    if let Ok(kb) = value.trim().trim_end_matches(" kB").trim().parse::<u64>() {
                        metric_map.insert(
                            "process.runtime.php.memory.rss_bytes".into(),
                            Value::Number((kb * 1024).into()),
                        );
                    }
                }
            }
        }
    }

    let mut service = serde_json::Map::new();
    service.insert(
        "organisation".into(),
        Value::String(envelope.organisation_id.clone()),
    );
    service.insert("project".into(), Value::String(envelope.project_id.clone()));
    service.insert(
        "application".into(),
        Value::String(envelope.application_id.clone()),
    );
    if let Some(version) = app_version {
        service.insert("version".into(), Value::String(version.to_owned()));
    }

    let mut attributes = serde_json::Map::new();
    if !metrics.http_route.is_empty() {
        attributes.insert(
            "http.route".into(),
            Value::String(metrics.http_route.clone()),
        );
    }
    if !metrics.http_method.is_empty() {
        attributes.insert(
            "http.method".into(),
            Value::String(metrics.http_method.clone()),
        );
    }
    if metrics.http_status_code > 0 {
        attributes.insert(
            "http.status_code".into(),
            Value::String(metrics.http_status_code.to_string()),
        );
    }

    let mut resource = serde_json::Map::new();
    resource.insert("process.pid".into(), json!(std::process::id()));
    resource.insert("process.runtime.name".into(), Value::String("php".into()));

    let body = json!({
        "schema": SCHEMA,
        "time": now_utc(),
        "trace_id": metrics.trace_id,
        "span_id": metrics.span_id,
        "service": service,
        "resource": resource,
        "attributes": attributes,
        "metrics": metric_map,
    });

    let serialised = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    spool_common::write_atomic(&envelope.tenant_spool_directory(), &serialised, "metrics")
}

fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}
