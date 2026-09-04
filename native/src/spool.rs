//! Span-batch serialisation + atomic spool write, byte-for-byte compatible with the userland
//! `LocalSpanRecorder::write()` so Engine ingests native and userland traces through one code path.
//!
//! Envelope (schema `chronos.tracing.span-batch.v1`):
//!   { schema, processing:{ messageId, batchId }, spans:[ ... ], spanCount:"<n>" }
//! where messageId/batchId are 16 random bytes as hex, spanCount is a STRING, each span's
//! `attributes` is a JSON OBJECT, `chronos.duration_nanoseconds` is a STRING, `parentSpanId` is ""
//! when absent, and timestamps are UTC `Y-m-d\TH:i:s.u\Z`. Persisted content-addressed: the file name
//! is sha256(body); written to `<spool>/<id>.trace.tmp` at mode 0600 then renamed to `<id>.trace`.

use crate::context::{hex_bytes, CollectorEnvelope};
use crate::observer::NativeSpan;
use crate::spool_common;
use serde_json::{json, Map, Value};

/// Serialise the finished request spans into the span-batch envelope JSON.
pub fn serialise_batch(
    envelope: &CollectorEnvelope,
    spans: &[NativeSpan],
    service_name: &str,
) -> String {
    let identity = spool_common::identity_envelope(envelope);

    let spans_json: Vec<Value> = spans
        .iter()
        .enumerate()
        .map(|(index, span)| {
            let mut attributes = Map::new();
            attributes.insert(
                "chronos.duration_nanoseconds".into(),
                Value::String(span.duration_nanoseconds.to_string()),
            );
            if index == 0 {
                if let Some(version) = &envelope.app_version {
                    attributes.insert("service.version".into(), Value::String(version.clone()));
                }
                // Runtime, framework and revision identity, on the ROOT span only:
                // they describe the process that served the request, so repeating
                // them on every child would multiply a constant by the span count.
                stamp(
                    &mut attributes,
                    "app.language",
                    Some(&envelope.app_language),
                );
                stamp(
                    &mut attributes,
                    "app.language.version",
                    envelope.app_language_version.as_ref(),
                );
                stamp(
                    &mut attributes,
                    "app.framework",
                    envelope.app_framework.as_ref(),
                );
                stamp(
                    &mut attributes,
                    "app.framework.version",
                    envelope.app_framework_version.as_ref(),
                );
                // Spelled identically to the profiler's labels (`sampler::set_label`)
                // so one vocabulary spans both signals: compare two commits on a flame
                // graph, then find the same two on a trace.
                let revision = crate::vcs::revision(&envelope.spool_directory);
                stamp(&mut attributes, "app.commit", Some(&revision.commit));
                stamp(&mut attributes, "app.branch", Some(&revision.branch));
            }
            for (key, value) in &span.attributes {
                attributes.insert(key.clone(), Value::String(value.clone()));
            }

            let mut object = identity.clone();
            object.insert("traceId".into(), Value::String(span.trace_id.clone()));
            object.insert("spanId".into(), Value::String(span.span_id.clone()));
            object.insert(
                "parentSpanId".into(),
                Value::String(span.parent_span_id.clone().unwrap_or_default()),
            );
            object.insert("name".into(), Value::String(span.name.clone()));
            object.insert("startedAt".into(), Value::String(span.started_at.clone()));
            object.insert("endedAt".into(), Value::String(span.ended_at.clone()));
            object.insert("status".into(), Value::String(span.status.clone()));
            // Lift HTTP identity to the top-level JsonSpan fields the engine indexes,
            // mirroring the userland SpanRecord::projection() shape.
            if let Some((_, method)) = span.attributes.iter().find(|(k, _)| k == "http.method") {
                object.insert("httpMethod".into(), Value::String(method.clone()));
            }
            if let Some((_, route)) = span.attributes.iter().find(|(k, _)| k == "http.route") {
                object.insert("httpRoute".into(), Value::String(route.clone()));
            }
            if let Some((_, code)) = span
                .attributes
                .iter()
                .find(|(k, _)| k == "http.status_code")
            {
                if let Ok(code) = code.parse::<i64>() {
                    object.insert("httpStatusCode".into(), Value::Number(code.into()));
                }
            }
            object.insert("attributes".into(), Value::Object(attributes));
            object.insert("serviceName".into(), Value::String(service_name.to_owned()));
            Value::Object(object)
        })
        .collect();

    let body = json!({
        "schema": "chronos.tracing.span-batch.v1",
        "processing": { "messageId": hex_bytes(16), "batchId": hex_bytes(16) },
        "spans": spans_json,
        "spanCount": spans.len().to_string(),
    });

    serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string())
}

/// Insert an identity attribute, skipping the ones this deployment cannot answer.
/// An absent framework is a fact about the application, not a value to invent — and
/// an empty string on the span would read as "known to be blank".
fn stamp(attributes: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        if !value.is_empty() {
            attributes.insert(key.into(), Value::String(value.clone()));
        }
    }
}

/// Content-addressed atomic write to the `.trace` spool.
pub fn write_atomic(spool_directory: &str, body: &str) -> std::io::Result<()> {
    spool_common::write_atomic(spool_directory, body, "trace")
}
