//! DST (Deterministic Simulation Testing) recording and spool write.
//! Schema `chronos.dst.recording.v1`.
//!
//! Records non-deterministic effects (time, RNG, I/O results, thrown exceptions) observed
//! during a request. On flush, writes the recording as one or more `.dst` spool files
//! (`spool_common::write_chunked`, byte-bounded — see that module) that the engine-agent
//! ships to `/v1/dst`. The recording is the foundation for replay: mutating captured values
//! allows re-evaluating execution paths without re-executing the request.
//!
//! DELIBERATELY UNBOUNDED: unlike every other spool type, a recording's event stream has no
//! per-request count ceiling. A capped recording can silently stop short of the event that
//! explains the failure it was recording — the whole point of chunked byte-bounded writes is
//! that "the recording is too big for one file" is no longer a reason to drop data, only a
//! reason to split it.

use crate::context::{hex_bytes, CollectorEnvelope};
use crate::spool_common;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;

const SCHEMA: &str = "chronos.dst.recording.v1";
const MAX_PAYLOAD_VALUE_LENGTH: usize = 4096;

#[derive(Clone, Debug)]
pub struct DstEvent {
    pub sequence: u64,
    pub timestamp: String,
    pub kind: DstEventKind,
    pub payload: Vec<(String, String)>,
    pub payload_digest: String,
    pub redaction: String,
}

#[derive(Clone, Debug)]
pub enum DstEventKind {
    Time,
    Random,
    DatabaseQuery,
    DatabaseResult,
    CacheRead,
    CacheWrite,
    HttpRequest,
    HttpResponse,
    FileRead,
    FileWrite,
    EnvRead,
    /// A `Throwable` observed at the moment it was thrown (before any catch block runs).
    /// Payload carries the identity a replay needs to reproduce the failure: `class`,
    /// and whichever of `message`/`code`/`file`/`line` the throwable actually set
    /// (a userland class that skips `parent::__construct()` can leave them unset).
    /// Recorded regardless of whether the exception is ultimately caught — a caught
    /// internal exception still shaped the execution path a replay must reproduce.
    Exception,
    /// ADR 0021 Phase 2: a first-party call-path visit. Payload: `name`, `depth`.
    /// Observational for path-diff; not an effect lookup during replay.
    Call,
    Custom(String),
}

impl DstEventKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Time => "time",
            Self::Random => "random",
            Self::DatabaseQuery => "database_query",
            Self::DatabaseResult => "database_result",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::HttpRequest => "http_request",
            Self::HttpResponse => "http_response",
            Self::FileRead => "file_read",
            Self::FileWrite => "file_write",
            Self::EnvRead => "env_read",
            Self::Exception => "exception",
            Self::Call => "call",
            Self::Custom(s) => s.as_str(),
        }
    }
}

thread_local! {
    static RECORDING: RefCell<Vec<DstEvent>> = const { RefCell::new(Vec::new()) };
    static SEQUENCE: RefCell<u64> = const { RefCell::new(0) };
    static ACTIVE: RefCell<bool> = const { RefCell::new(false) };
}

pub fn activate() {
    ACTIVE.with(|a| *a.borrow_mut() = true);
    SEQUENCE.with(|s| *s.borrow_mut() = 0);
    RECORDING.with(|r| r.borrow_mut().clear());
    crate::call_path::arm();
}

pub fn deactivate() {
    ACTIVE.with(|a| *a.borrow_mut() = false);
}

pub fn reset() {
    deactivate();
    SEQUENCE.with(|s| *s.borrow_mut() = 0);
    RECORDING.with(|r| r.borrow_mut().clear());
    crate::call_path::reset();
}

pub fn is_active() -> bool {
    ACTIVE.with(|a| *a.borrow())
}

pub fn record(kind: DstEventKind, payload: Vec<(String, String)>) {
    if !is_active() {
        return;
    }
    RECORDING.with(|recording| {
        let mut recording = recording.borrow_mut();
        let seq = SEQUENCE.with(|s| {
            let mut s = s.borrow_mut();
            *s += 1;
            *s
        });

        let digest = payload_digest(&payload);

        let redacted_payload: Vec<(String, String)> = payload
            .into_iter()
            .map(|(k, v)| {
                let capped = cap_utf8(v, MAX_PAYLOAD_VALUE_LENGTH);
                (k, capped)
            })
            .collect();

        recording.push(DstEvent {
            sequence: seq,
            timestamp: now_utc(),
            kind,
            payload: redacted_payload,
            payload_digest: digest,
            redaction: "capped".to_owned(),
        });
    });
}

pub fn drain() -> Vec<DstEvent> {
    RECORDING.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

pub fn flush(
    envelope: &CollectorEnvelope,
    events: &[DstEvent],
    trace_id: &str,
    session_id: Option<&str>,
) -> std::io::Result<()> {
    flush_with_budget(envelope, events, trace_id, session_id, spool_common::max_body_bytes())
}

/// `flush` with the per-document byte budget supplied rather than read from the environment.
/// See `profile_spool::flush_with_budget` for why the budget is a parameter: mutating
/// `CHRONOS_PHP_SPOOL_MAX_BYTES` from a test races every other test that reads it.
pub fn flush_with_budget(
    envelope: &CollectorEnvelope,
    events: &[DstEvent],
    trace_id: &str,
    session_id: Option<&str>,
    max_body_bytes: usize,
) -> std::io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    let events_json: Vec<Value> = events
        .iter()
        .map(|event| {
            let mut payload_map = serde_json::Map::new();
            for (k, v) in &event.payload {
                payload_map.insert(k.clone(), Value::String(v.clone()));
            }
            json!({
                "sequence": event.sequence,
                "timestamp": event.timestamp,
                "kind": event.kind.as_str(),
                "payload": payload_map,
                "payloadDigest": event.payload_digest,
                "redaction": event.redaction,
            })
        })
        .collect();

    // `recordingId` is generated ONCE here, before chunking, and repeated identically on
    // every chunk (it lives on the shared header, not per-event) — a recording split across
    // several `.dst` files must still carry one id a consumer can group them back by. That
    // is a distinct concept from `chunk.groupId` (spool_common's file-reassembly id): this
    // is the recording's own identity, independent of how many files it happened to need.
    let mut header = Map::new();
    header.insert("schema".into(), Value::String(SCHEMA.to_owned()));
    header.insert(
        "processing".into(),
        json!({ "messageId": hex_bytes(16), "batchId": hex_bytes(16) }),
    );
    header.insert(
        "organisation".into(),
        json!({ "organisationId": envelope.organisation_id }),
    );
    header.insert(
        "application".into(),
        json!({
            "project": {
                "organisation": { "organisationId": envelope.organisation_id },
                "projectId": envelope.project_id,
            },
            "applicationId": envelope.application_id,
        }),
    );
    header.insert("recordingId".into(), Value::String(hex_bytes(16)));
    header.insert("traceId".into(), Value::String(trace_id.to_owned()));
    header.insert(
        "sessionId".into(),
        Value::String(session_id.unwrap_or("").to_owned()),
    );
    header.insert("recordedAt".into(), Value::String(now_utc()));
    header.insert("eventCount".into(), Value::String(events.len().to_string()));

    spool_common::write_chunked(
        &envelope.spool_directory,
        "dst",
        &header,
        "events",
        Some("eventCount"),
        &events_json,
        max_body_bytes,
    )
}

/// Truncate to at most `limit` bytes WITHOUT splitting a UTF-8 character.
///
/// Slicing a `String` at a fixed byte offset panics when that offset lands inside a multi-byte
/// sequence, and this crate is built `panic = "abort"` — so a recorded payload value longer than
/// the cap whose boundary byte falls mid-character would abort the PHP worker mid-request.
/// Recorded values are application-supplied (exception messages, serialized objects, SQL), so
/// multi-byte content at an arbitrary offset is entirely ordinary, not a corner case. The
/// trailing partial character is dropped rather than kept.
fn cap_utf8(value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn payload_digest(payload: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    for (k, v) in payload {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_spool(name: &str) -> String {
        let root = std::env::temp_dir().join(format!("chronos-dst-spool-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        root.to_string_lossy().into_owned()
    }

    fn envelope(spool_directory: &str) -> CollectorEnvelope {
        CollectorEnvelope {
            organisation_id: "org-local".into(),
            project_id: "proj".into(),
            application_id: "deepwell".into(),
            app_version: None,
            app_language: "php".into(),
            app_language_version: None,
            app_framework: None,
            app_framework_version: None,
            spool_directory: spool_directory.to_owned(),
        }
    }

    fn read_chunks(spool_directory: &str) -> Vec<Value> {
        let mut chunks: Vec<(u64, Value)> = std::fs::read_dir(spool_directory)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "dst"))
                    .map(|entry| {
                        let body = std::fs::read_to_string(entry.path()).expect("read chunk");
                        let value: Value = serde_json::from_str(&body).expect("parse chunk");
                        let index = value["chunk"]["index"].as_u64().unwrap_or(0);
                        (index, value)
                    })
                    .collect()
            })
            .unwrap_or_default();
        chunks.sort_by_key(|(index, _)| *index);
        chunks.into_iter().map(|(_, value)| value).collect()
    }

    #[test]
    fn exception_is_the_event_kind_a_throw_time_capture_records_under() {
        assert_eq!(DstEventKind::Exception.as_str(), "exception");
    }

    #[test]
    fn recording_is_a_no_op_when_not_active() {
        reset();
        record(DstEventKind::Time, vec![("function".into(), "time".into())]);
        assert!(drain().is_empty());
    }

    #[test]
    fn activate_clears_prior_state_and_restarts_sequence_at_one() {
        activate();
        record(DstEventKind::Random, vec![]);
        activate();
        record(DstEventKind::Time, vec![]);
        let events = drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);
    }

    #[test]
    fn an_exception_event_carries_its_identity_payload() {
        activate();
        record(
            DstEventKind::Exception,
            vec![
                ("class".into(), "RuntimeException".into()),
                ("message".into(), "boom".into()),
                ("code".into(), "0".into()),
                ("file".into(), "/srv/app/src/Controller.php".into()),
                ("line".into(), "42".into()),
            ],
        );
        let events = drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind.as_str(), "exception");
        let class = events[0]
            .payload
            .iter()
            .find(|(k, _)| k == "class")
            .map(|(_, v)| v.as_str());
        assert_eq!(class, Some("RuntimeException"));
    }

    #[test]
    fn a_recording_is_no_longer_capped_at_four_thousand_ninety_six_events() {
        // Regression: MAX_EVENTS used to silently drop everything past 4096. DST is now
        // deliberately unbounded — dropping the event that explains a failure defeats the
        // point of recording it.
        activate();
        for _ in 0..5000 {
            record(DstEventKind::Time, vec![("function".into(), "time".into())]);
        }
        assert_eq!(drain().len(), 5000);
    }

    #[test]
    fn a_payload_value_over_the_length_cap_is_truncated_not_dropped() {
        activate();
        let long_value = "x".repeat(MAX_PAYLOAD_VALUE_LENGTH + 500);
        record(DstEventKind::Custom("probe".into()), vec![("result".into(), long_value)]);
        let events = drain();
        assert_eq!(events[0].payload[0].1.len(), MAX_PAYLOAD_VALUE_LENGTH);
    }

    #[test]
    fn flushing_no_events_writes_no_file() {
        let dir = temp_spool("empty");
        flush(&envelope(&dir), &[], "trace-1", None).expect("flush");
        assert!(!std::path::Path::new(&dir).exists());
    }

    #[test]
    fn a_small_recording_flushes_to_one_file_with_the_full_header() {
        activate();
        record(DstEventKind::Time, vec![("function".into(), "time".into())]);
        record(DstEventKind::EnvRead, vec![("function".into(), "getenv".into())]);
        let events = drain();
        deactivate();

        let dir = temp_spool("single-chunk");
        flush_with_budget(&envelope(&dir), &events, "trace-1", Some("session-1"), spool_common::DEFAULT_MAX_BODY_BYTES).expect("flush");
        let chunks = read_chunks(&dir);
        assert_eq!(chunks.len(), 1);
        let doc = &chunks[0];
        assert_eq!(doc["schema"], SCHEMA);
        assert_eq!(doc["traceId"], "trace-1");
        assert_eq!(doc["sessionId"], "session-1");
        assert_eq!(doc["eventCount"], "2");
        assert_eq!(doc["events"].as_array().unwrap().len(), 2);
        assert_eq!(doc["chunk"]["index"], 0);
        assert_eq!(doc["chunk"]["count"], 1);
    }

    #[test]
    fn session_id_is_an_empty_string_rather_than_absent_when_there_is_no_session() {
        activate();
        record(DstEventKind::Time, vec![]);
        let events = drain();
        deactivate();

        let dir = temp_spool("no-session");
        flush_with_budget(&envelope(&dir), &events, "trace-1", None, spool_common::DEFAULT_MAX_BODY_BYTES).expect("flush");
        let chunks = read_chunks(&dir);
        assert_eq!(chunks[0]["sessionId"], "");
    }

    #[test]
    fn an_oversized_recording_splits_into_chunks_sharing_one_recording_id() {
        // Regression for the "don't bound the size of the recorded profile" instruction:
        // an unbounded recording must still ship, split across files that stay under the
        // byte budget, rather than either being capped or exceeding the ingest ceiling.
        activate();
        for i in 0..200 {
            record(
                DstEventKind::DatabaseQuery,
                vec![
                    ("function".into(), "PDO::query".into()),
                    ("statement".into(), format!("SELECT * FROM orders WHERE id = {i}")),
                ],
            );
        }
        let events = drain();
        deactivate();

        let dir = temp_spool("split");
        // A small explicit budget forces the split without touching process-global env, and the
        // same value bounds the assertion below — the budget the writer was given is the only
        // budget the files can be checked against.
        let budget = 2048;
        flush_with_budget(&envelope(&dir), &events, "trace-1", None, budget).expect("flush");

        let chunks = read_chunks(&dir);
        assert!(chunks.len() > 1, "expected more than one chunk file, got {}", chunks.len());

        let recording_id = chunks[0]["recordingId"].as_str().unwrap().to_owned();
        let group_id = chunks[0]["chunk"]["groupId"].as_str().unwrap().to_owned();
        let total = chunks.len() as u64;
        let mut reassembled = 0usize;
        for (index, chunk) in chunks.iter().enumerate() {
            // recordingId is the recording's own identity: identical on every chunk,
            // distinct from chunk.groupId (spool_common's file-reassembly id).
            assert_eq!(chunk["recordingId"], recording_id);
            assert_eq!(chunk["chunk"]["groupId"], group_id);
            assert_eq!(chunk["chunk"]["index"], index as u64);
            assert_eq!(chunk["chunk"]["count"], total);
            // eventCount states what THIS file holds — engine-ingest validates it against the
            // array it actually received — while the recording's true total stays readable from
            // the chunk descriptor.
            let held = chunk["events"].as_array().unwrap().len();
            assert_eq!(chunk["eventCount"], held.to_string());
            assert_eq!(chunk["chunk"]["totalItems"], 200);
            reassembled += chunk["events"].as_array().unwrap().len();
        }
        assert_eq!(reassembled, 200);

        for path in std::fs::read_dir(&dir).unwrap() {
            let path = path.unwrap().path();
            let len = std::fs::metadata(&path).unwrap().len() as usize;
            assert!(len <= budget, "{path:?} is {len} bytes, over the {budget}-byte budget");
        }
    }
}
