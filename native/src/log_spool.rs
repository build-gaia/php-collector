//! Structured log capture and spool write. Schema `chronos.logs.log-batch.v1`.
//!
//! Logs are buffered request-locally and flushed at RSHUTDOWN as a single `.log` spool file,
//! matching the batch format the engine-agent ships to `/v1/logs`. This avoids per-log-line
//! file I/O and keeps the spool directory bounded.

use crate::context::{hex_bytes, CollectorEnvelope};
use crate::spool_common;
use serde_json::{json, Value};
use std::cell::RefCell;

const SCHEMA: &str = "chronos.tracing.log-batch.v1";
const MAX_LOGS_PER_REQUEST: usize = 512;
const MAX_BODY_LENGTH: usize = 1024;
const MAX_ATTRIBUTES: usize = 16;
const MAX_KEY_LENGTH: usize = 128;
const MAX_VALUE_LENGTH: usize = 512;

#[derive(Clone, Debug)]
pub struct LogRecord {
    pub severity_text: String,
    pub severity_number: i32,
    pub body: String,
    pub trace_id: String,
    pub span_id: String,
    pub observed_at: String,
    pub attributes: Vec<(String, String)>,
}

thread_local! {
    static REQUEST_LOGS: RefCell<Vec<LogRecord>> = const { RefCell::new(Vec::new()) };
}

pub fn reset() {
    REQUEST_LOGS.with(|logs| logs.borrow_mut().clear());
}

pub fn capture(record: LogRecord) {
    REQUEST_LOGS.with(|logs| {
        let mut logs = logs.borrow_mut();
        if logs.len() < MAX_LOGS_PER_REQUEST {
            logs.push(record);
        }
    });
}

pub fn drain() -> Vec<LogRecord> {
    REQUEST_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
}

pub fn flush(envelope: &CollectorEnvelope, records: &[LogRecord]) -> std::io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }

    let logs_json: Vec<Value> = records
        .iter()
        .map(|record| {
            let body = cap(&record.body, MAX_BODY_LENGTH);
            let mut attrs = serde_json::Map::new();
            for (i, (key, value)) in record.attributes.iter().enumerate() {
                if i >= MAX_ATTRIBUTES {
                    break;
                }
                attrs.insert(
                    cap(key, MAX_KEY_LENGTH),
                    Value::String(cap(value, MAX_VALUE_LENGTH)),
                );
            }

            json!({
                "organisation": { "organisationId": envelope.organisation_id },
                "application": { "applicationId": envelope.application_id },
                "traceId": record.trace_id,
                "spanId": record.span_id,
                "observedAt": record.observed_at,
                "severity": record.severity_text,
                "severityNumber": record.severity_number,
                "body": body,
                "attributes": attrs,
            })
        })
        .collect();

    let body = json!({
        "schema": SCHEMA,
        "processing": { "messageId": hex_bytes(16), "batchId": hex_bytes(16) },
        "logs": logs_json,
        "logCount": records.len().to_string(),
    });

    let serialised = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    spool_common::write_atomic(&envelope.tenant_spool_directory(), &serialised, "log")
}

fn cap(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_owned()
    } else {
        value[..max].to_owned()
    }
}
