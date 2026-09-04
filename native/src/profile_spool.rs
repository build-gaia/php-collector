//! Profile-sample-batch serialisation + atomic spool write, the profiling analogue of `spool.rs`.
//!
//! Envelope (schema `chronos.profiling.sample-batch.v1`) — a JSON mirror of the proto
//! `chronos.profiling.v1.ProfileSampleBatch` that the engine `/v1/profiles` route accepts.

use crate::context::{hex_bytes, CollectorEnvelope};
use crate::sampler::Sample;
use crate::spool_common;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const SCHEMA: &str = "chronos.profiling.sample-batch.v1";

/// Serialise buffered samples into the profile-sample-batch envelope JSON.
///
/// `labels` are the request's profile tags (route, action, …), stamped identically onto
/// every sample in the batch — they describe the request the samples came from, so they
/// are resolved once at flush rather than carried per sample. Maps onto the proto's
/// `map<string, string> labels = 12`.
///
/// The series id is suffixed per sample type. A series is the handle the engine scopes
/// a heatmap or flamegraph by, and it groups by `(series_id, sample_type, unit)` — so
/// mixing four types into one series id would produce four series sharing a name and
/// leave the reader unable to tell which is which.
#[must_use]
pub fn serialise_batch(
    envelope: &CollectorEnvelope,
    samples: &[Sample],
    first_sequence: u64,
    labels: &BTreeMap<String, String>,
) -> String {
    let body = json!({
        "schema": SCHEMA,
        "processing": { "messageId": hex_bytes(16), "batchId": hex_bytes(16) },
        "sampleCount": samples.len().to_string(),
        "samples": Value::Array(samples_json(envelope, samples, first_sequence, labels)),
    });
    serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string())
}

/// Every sample as its wire object. Separated from the envelope because a flush may
/// need to spread these across several documents, and the split has to happen on
/// finished items rather than on a serialised blob.
fn samples_json(
    envelope: &CollectorEnvelope,
    samples: &[Sample],
    first_sequence: u64,
    labels: &BTreeMap<String, String>,
) -> Vec<Value> {
    let identity = spool_common::identity_envelope(envelope);
    let series = std::env::var("CHRONOS_PHP_PROFILE_SERIES_ID").unwrap_or_else(|_| "php".into());
    let labels_json = labels_json(labels);

    let samples_json: Vec<Value> = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let mut object = identity.clone();
            object.insert(
                "profileSeriesId".into(),
                Value::String(series_id_for(&series, sample)),
            );
            object.insert(
                "sequence".into(),
                Value::String(first_sequence.saturating_add(index as u64).to_string()),
            );
            object.insert(
                "sampledAt".into(),
                Value::String(unix_nanos_to_utc(sample.sampled_at_unix_nanos)),
            );
            object.insert(
                "sampleType".into(),
                Value::String(sample.kind.proto_name().into()),
            );
            object.insert(
                "periodNanoseconds".into(),
                Value::String(sample.period_nanoseconds.to_string()),
            );
            object.insert(
                "value".into(),
                Value::String(sample.value_nanoseconds.to_string()),
            );
            object.insert("unit".into(), Value::String(sample.kind.unit().into()));
            object.insert("stack".into(), Value::Array(frames_json(sample)));
            object.insert("correlation".into(), correlation_json(sample));
            if !labels_json.is_empty() {
                object.insert("labels".into(), Value::Object(labels_json.clone()));
            }
            Value::Object(object)
        })
        .collect();

    samples_json
}

/// The process-wide sample sequence.
///
/// Monotonic per PROCESS rather than per request: the engine orders a series by it,
/// and restarting the count at every request would make a worker's samples sort as
/// though they had all arrived at once.
static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Serialise and spool one request's samples, chunked to the configured budget.
pub fn flush(
    envelope: &CollectorEnvelope,
    samples: &[Sample],
    labels: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    flush_with_budget(envelope, samples, labels, spool_common::max_body_bytes())
}

/// `flush` with the per-document byte budget supplied rather than read from the
/// environment: mutating `CHRONOS_PHP_SPOOL_MAX_BYTES` from a test races every other
/// test that reads it, so the budget is a parameter and the environment is consulted
/// exactly once, at the edge.
pub fn flush_with_budget(
    envelope: &CollectorEnvelope,
    samples: &[Sample],
    labels: &BTreeMap<String, String>,
    max_body_bytes: usize,
) -> std::io::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let first_sequence =
        SEQUENCE.fetch_add(samples.len() as u64, std::sync::atomic::Ordering::Relaxed);
    let samples_json = samples_json(envelope, samples, first_sequence, labels);

    let mut header = Map::new();
    header.insert("schema".into(), Value::String(SCHEMA.to_owned()));
    header.insert(
        "processing".into(),
        json!({ "messageId": hex_bytes(16), "batchId": hex_bytes(16) }),
    );

    spool_common::write_chunked(
        &envelope.tenant_spool_directory(),
        "profile",
        &header,
        "samples",
        Some("sampleCount"),
        &samples_json,
        max_body_bytes,
    )
}

/// `"php"` + the lowercase type, e.g. `php-cpu`, `php-off_cpu`.
fn series_id_for(base: &str, sample: &Sample) -> String {
    let suffix = sample
        .kind
        .proto_name()
        .trim_start_matches("PROFILE_SAMPLE_TYPE_")
        .to_ascii_lowercase();
    format!("{base}-{suffix}")
}

fn labels_json(labels: &BTreeMap<String, String>) -> Map<String, Value> {
    labels
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect()
}

fn frames_json(sample: &Sample) -> Vec<Value> {
    sample
        .stack
        .iter()
        .map(|frame| {
            json!({
                "module": frame.module,
                "function": frame.function,
                "file": frame.file,
                "line": frame.line,
            })
        })
        .collect()
}

fn correlation_json(sample: &Sample) -> Value {
    json!({
        "traceId": sample.trace_id,
        "spanId": sample.span_id,
        "sessionId": sample.session_id.clone().unwrap_or_default(),
    })
}

/// Content-addressed atomic write to the `.profile` spool.
pub fn write_atomic(spool_directory: &str, body: &str) -> std::io::Result<()> {
    spool_common::write_atomic(spool_directory, body, "profile")
}

fn unix_nanos_to_utc(unix_nanos: u128) -> String {
    let seconds = i64::try_from(unix_nanos / 1_000_000_000).unwrap_or(0);
    let micros = u32::try_from((unix_nanos % 1_000_000_000) / 1_000).unwrap_or(0);
    chrono::DateTime::from_timestamp(seconds, micros * 1_000)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::{Sample, SampleFrame, SampleKind};

    fn envelope() -> CollectorEnvelope {
        CollectorEnvelope {
            organisation_id: "org-local".into(),
            project_id: "proj".into(),
            application_id: "deepwell".into(),
            app_version: None,
            spool_directory: "/tmp".into(),
            app_language: "php".into(),
            app_language_version: None,
            app_framework: None,
            app_framework_version: None,
        }
    }

    fn sample_of(kind: SampleKind) -> Sample {
        Sample {
            kind,
            sampled_at_unix_nanos: 1_723_550_400_123_456_000,
            period_nanoseconds: 1_000_000_000 / 99,
            value_nanoseconds: 1_000_000_000 / 99,
            stack: std::rc::Rc::new(vec![
                SampleFrame {
                    module: "php".into(),
                    function: "App\\Controller::run".into(),
                    file: "/srv/app/src/Controller.php".into(),
                    line: 12,
                },
                SampleFrame {
                    module: "php".into(),
                    function: "Doctrine::query".into(),
                    file: "/srv/vendor/doctrine/Query.php".into(),
                    line: 88,
                },
            ]),
            trace_id: "a".repeat(32),
            span_id: "b".repeat(16),
            session_id: Some("018f-session".into()),
        }
    }

    fn sample() -> Sample {
        sample_of(SampleKind::Cpu)
    }

    fn no_labels() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn envelope_mirrors_the_profile_sample_batch_proto_shape() {
        let body = serialise_batch(&envelope(), &[sample()], 7, &no_labels());
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["sampleCount"], "1");
        let first = &value["samples"][0];
        assert_eq!(first["sequence"], "7");
        assert_eq!(first["sampleType"], "PROFILE_SAMPLE_TYPE_CPU");
        assert_eq!(first["unit"], "nanoseconds");
        assert_eq!(first["stack"][0]["function"], "App\\Controller::run");
        assert_eq!(first["stack"][1]["function"], "Doctrine::query");
        assert_eq!(first["correlation"]["traceId"], "a".repeat(32));
        assert_eq!(first["correlation"]["sessionId"], "018f-session");
    }

    #[test]
    fn each_sample_carries_its_own_type() {
        let samples = [
            sample_of(SampleKind::Cpu),
            sample_of(SampleKind::Wall),
            sample_of(SampleKind::OffCpu),
            sample_of(SampleKind::Io),
        ];
        let body = serialise_batch(&envelope(), &samples, 1, &no_labels());
        let value: Value = serde_json::from_str(&body).unwrap();
        let types: Vec<&str> = (0..4)
            .map(|index| value["samples"][index]["sampleType"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                "PROFILE_SAMPLE_TYPE_CPU",
                "PROFILE_SAMPLE_TYPE_WALL",
                "PROFILE_SAMPLE_TYPE_OFF_CPU",
                "PROFILE_SAMPLE_TYPE_IO",
            ]
        );
    }

    #[test]
    fn the_series_id_is_suffixed_per_type_so_series_stay_distinguishable() {
        std::env::set_var("CHRONOS_PHP_PROFILE_SERIES_ID", "php");
        let samples = [sample_of(SampleKind::Cpu), sample_of(SampleKind::OffCpu)];
        let body = serialise_batch(&envelope(), &samples, 1, &no_labels());
        std::env::remove_var("CHRONOS_PHP_PROFILE_SERIES_ID");
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["samples"][0]["profileSeriesId"], "php-cpu");
        assert_eq!(value["samples"][1]["profileSeriesId"], "php-off_cpu");
    }

    #[test]
    fn labels_are_stamped_onto_every_sample() {
        let samples = [sample_of(SampleKind::Cpu), sample_of(SampleKind::Wall)];
        let tags = labels(&[("route", "/orders/{id}"), ("action", "orderActions::show")]);
        let body = serialise_batch(&envelope(), &samples, 1, &tags);
        let value: Value = serde_json::from_str(&body).unwrap();
        for index in 0..2 {
            assert_eq!(value["samples"][index]["labels"]["route"], "/orders/{id}");
            assert_eq!(
                value["samples"][index]["labels"]["action"],
                "orderActions::show"
            );
        }
    }

    #[test]
    fn the_labels_key_is_omitted_entirely_when_there_are_no_tags() {
        // An empty map would serialise as `{}`, which reads as "tagged with nothing"
        // rather than "untagged"; absent is the honest encoding.
        let body = serialise_batch(&envelope(), &[sample()], 1, &no_labels());
        let value: Value = serde_json::from_str(&body).unwrap();
        assert!(value["samples"][0].get("labels").is_none());
    }

    #[test]
    fn scalar_wide_integers_are_serialised_as_strings() {
        let body = serialise_batch(&envelope(), &[sample()], 1, &no_labels());
        let value: Value = serde_json::from_str(&body).unwrap();
        let first = &value["samples"][0];
        assert!(first["value"].is_string());
        assert!(first["periodNanoseconds"].is_string());
        assert!(first["sequence"].is_string());
        assert!(value["sampleCount"].is_string());
    }

    #[test]
    fn timestamp_renders_utc_microseconds() {
        let body = serialise_batch(&envelope(), &[sample()], 1, &no_labels());
        let value: Value = serde_json::from_str(&body).unwrap();
        let sampled_at = value["samples"][0]["sampledAt"].as_str().unwrap();
        assert!(sampled_at.starts_with("2024-08-13T"));
        assert!(sampled_at.ends_with("Z"));
    }
}
