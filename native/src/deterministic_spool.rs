//! Deterministic-profile-batch serialisation + atomic spool write — the counted sibling
//! of `profile_spool.rs`.
//!
//! Envelope (schema `chronos.profiling.deterministic-batch.v1`, extension `.dprofile`) —
//! a JSON mirror of the proto `chronos.profiling.v1.DeterministicProfileBatch` that the
//! engine's `POST /v1/profiles/deterministic` route accepts.
//!
//! WHY A NEW EXTENSION AND NOT REUSED `.profile`. The engine-agent routes purely by
//! file extension, and engine-ingest's profile decoder never reads the `schema` key —
//! it dispatches on which axum route received the POST. Reusing `.profile` would make
//! `schema` load-bearing across two services purely to disambiguate two unrelated body
//! shapes arriving on one route.
//!
//! WHERE THIS DIFFERS FROM THE SAMPLE BATCH, and why. A `.profile` sample is a POINT IN
//! TIME, so tenant identity, series and correlation are repeated per sample. Every row
//! in a `.dprofile` document describes the SAME window, so all of that sits on the batch
//! header instead and the rows carry only the per-function numbers. Chunking still
//! repeats the header verbatim per chunk, which is the property the per-item placement
//! existed to guarantee in the first place — each file stays independently ingestible.
//!
//! One chunkable item array by design: Tier 2 edges hang off their caller's row and
//! Tier 3 samples off their function's row, so the whole document has a single
//! splittable list and `spool_common::write_chunked` needs no second axis.

use crate::context::{hex_bytes, CollectorEnvelope, TraceContext};
use crate::deterministic::{ArgumentSample, DeterministicWindow, EdgeRow, FunctionRow};
use crate::spool_common;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The spool document's own schema string. DIFFERENT from the NATS republish header
/// (`chronos.profiling.v1.DeterministicProfileBatch`) for the same signal, exactly as
/// it already is for samples. Do not conflate the two.
pub const SCHEMA: &str = "chronos.profiling.deterministic-batch.v1";

/// The spool file extension the engine-agent routes on. `.dprofile` must appear in BOTH
/// the agent's `next_batch()` extension allowlist and its `publish_batch()` route
/// mapping: miss the allowlist and these files are invisible on disk and accumulate
/// until the volume fills; miss the mapping and they are posted to the sample route.
pub const EXTENSION: &str = "dprofile";

/// Always `"nanoseconds"` in v1. Emitted rather than implied so a future unit change is
/// a data change and not a silent reinterpretation of every historical row.
const UNIT: &str = "nanoseconds";

/// The window one document covers — one request, RINIT to RSHUTDOWN.
///
/// `wall_nanoseconds` is carried rather than derived from `from`/`to` so a share can be
/// computed without subtracting two RFC 3339 strings across a clock the reader does not
/// own. It comes off the monotonic clock; `from`/`to` come off the wall clock, and the
/// two are deliberately not assumed to agree.
#[derive(Clone, Debug)]
pub struct Window {
    pub from: String,
    pub to: String,
    pub wall_nanoseconds: u128,
}

/// Serialise one window into the deterministic-batch envelope JSON.
///
/// Exposed alongside [`flush`] for the same reason `profile_spool::serialise_batch` is:
/// the document shape is a contract, and a test that asserts on it must not have to
/// write a file to read it.
#[must_use]
pub fn serialise_batch(
    envelope: &CollectorEnvelope,
    window: &DeterministicWindow,
    interval: &Window,
    context: Option<&TraceContext>,
    labels: &BTreeMap<String, String>,
) -> String {
    let mut object = header(envelope, window, interval, context, labels);
    object.insert(
        "functionCount".into(),
        Value::String(window.functions.len().to_string()),
    );
    object.insert("functions".into(), Value::Array(functions_json(window)));
    serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_owned())
}

/// Serialise and spool one request's aggregate, chunked to the configured budget.
pub fn flush(
    envelope: &CollectorEnvelope,
    window: &DeterministicWindow,
    interval: &Window,
    context: Option<&TraceContext>,
    labels: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    flush_with_budget(
        envelope,
        window,
        interval,
        context,
        labels,
        spool_common::max_body_bytes(),
    )
}

/// [`flush`] with the per-document byte budget supplied rather than read from the
/// environment: mutating `CHRONOS_PHP_SPOOL_MAX_BYTES` from a test races every other
/// test that reads it, so the budget is a parameter and the environment is consulted
/// exactly once, at the edge.
pub fn flush_with_budget(
    envelope: &CollectorEnvelope,
    window: &DeterministicWindow,
    interval: &Window,
    context: Option<&TraceContext>,
    labels: &BTreeMap<String, String>,
    max_body_bytes: usize,
) -> std::io::Result<()> {
    // A window with no rows is not written at all. An empty `.dprofile` would claim
    // coverage of a request in which nothing was observed, which is a different fact
    // from "this request was not covered" and the reader cannot tell them apart.
    if window.functions.is_empty() {
        return Ok(());
    }
    let header = header(envelope, window, interval, context, labels);
    spool_common::write_chunked(
        &envelope.tenant_spool_directory(),
        EXTENSION,
        &header,
        "functions",
        Some("functionCount"),
        &functions_json(window),
        max_body_bytes,
    )
}

/// Everything that describes the WINDOW rather than a row. Repeated verbatim per chunk.
fn header(
    envelope: &CollectorEnvelope,
    window: &DeterministicWindow,
    interval: &Window,
    context: Option<&TraceContext>,
    labels: &BTreeMap<String, String>,
) -> Map<String, Value> {
    let mut object = spool_common::identity_envelope(envelope);
    object.insert("schema".into(), Value::String(SCHEMA.to_owned()));
    object.insert(
        "processing".into(),
        json!({ "messageId": hex_bytes(16), "batchId": hex_bytes(16) }),
    );
    object.insert("profileSeriesId".into(), Value::String(series_id()));
    object.insert("unit".into(), Value::String(UNIT.to_owned()));
    object.insert(
        "window".into(),
        json!({
            "from": interval.from,
            "to": interval.to,
            "wallNanoseconds": interval.wall_nanoseconds.to_string(),
        }),
    );
    // Canonical correlation only — trace/span/session ids, never captured application
    // values. Emitted even when empty so the key's absence never has to be interpreted.
    object.insert(
        "correlation".into(),
        json!({
            "traceId": context.map(|c| c.trace_id.as_str()).unwrap_or_default(),
            "spanId": context.map(|c| c.span_id.as_str()).unwrap_or_default(),
            "sessionId": context
                .and_then(|c| c.session_id.as_deref())
                .unwrap_or_default(),
        }),
    );
    // Omitted entirely when empty: `{}` reads as "tagged with nothing" rather than
    // "untagged", the same reason the sample batch omits it.
    if !labels.is_empty() {
        object.insert(
            "labels".into(),
            Value::Object(
                labels
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect(),
            ),
        );
    }
    // Declared coverage, never inferred. An absent tier means NOT COLLECTED and must
    // not render as zero, which is only possible if the document says which tiers ran.
    object.insert(
        "tiers".into(),
        json!({
            "aggregates": window.coverage.aggregates,
            "edges": window.coverage.edges,
            "arguments": window.coverage.arguments,
        }),
    );
    object.insert(
        "truncation".into(),
        json!({
            "functionsSeen": window.truncation.functions_seen.to_string(),
            "functionsKept": window.truncation.functions_kept.to_string(),
            "edgesSeen": window.truncation.edges_seen.to_string(),
            "edgesKept": window.truncation.edges_kept.to_string(),
            "argumentBytesDropped": window.truncation.argument_bytes_dropped.to_string(),
        }),
    );
    object
}

/// `"php"` + `-deterministic`. A series is the handle the engine scopes a read by, and
/// the counted and sampled signals must never share one: their values are not the same
/// kind of number and a merged series would leave the reader unable to tell which is
/// which.
fn series_id() -> String {
    format!(
        "{}-deterministic",
        crate::settings::string("CHRONOS_PHP_PROFILE_SERIES_ID", "php")
    )
}

fn functions_json(window: &DeterministicWindow) -> Vec<Value> {
    window.functions.iter().map(function_json).collect()
}

fn function_json(row: &FunctionRow) -> Value {
    let mut object = Map::new();
    // The canonical identity, verbatim. Byte-for-byte a `.profile` `stack[].function`
    // and byte-for-byte a segment of the desktop's flame path.
    object.insert("function".into(), Value::String(row.function.to_string()));
    if !row.module.is_empty() {
        object.insert("module".into(), Value::String(row.module.to_string()));
    }
    if !row.file.is_empty() {
        object.insert("file".into(), Value::String(row.file.to_string()));
    }
    // uint32 -> JSON number, unlike the uint64s beside it. Matches the sample batch,
    // where `line` is a number while `value` is a string.
    object.insert("line".into(), Value::from(row.line));
    object.insert(
        "callCount".into(),
        Value::String(row.call_count.to_string()),
    );
    object.insert(
        "inclusiveNanoseconds".into(),
        Value::String(row.inclusive_nanoseconds.to_string()),
    );
    object.insert(
        "exclusiveNanoseconds".into(),
        Value::String(row.exclusive_nanoseconds.to_string()),
    );
    object.insert(
        "maxRecursionDepth".into(),
        Value::from(row.max_recursion_depth),
    );
    // Omitted rather than empty: Tier 2 being off and a function calling nothing must
    // not be representable by the same document.
    if !row.callees.is_empty() {
        object.insert(
            "callees".into(),
            Value::Array(row.callees.iter().map(edge_json).collect()),
        );
    }
    if !row.argument_samples.is_empty() {
        object.insert(
            "argumentSamples".into(),
            Value::Array(
                row.argument_samples
                    .iter()
                    .map(argument_sample_json)
                    .collect(),
            ),
        );
    }
    Value::Object(object)
}

fn edge_json(edge: &EdgeRow) -> Value {
    json!({
        "callee": edge.callee.to_string(),
        "callCount": edge.call_count.to_string(),
        "inclusiveNanoseconds": edge.inclusive_nanoseconds.to_string(),
    })
}

fn argument_sample_json(sample: &ArgumentSample) -> Value {
    let arguments: Vec<Value> = sample
        .arguments
        .iter()
        .map(|argument| {
            let mut object = Map::new();
            object.insert("position".into(), Value::from(argument.position));
            if !argument.name.is_empty() {
                object.insert("name".into(), Value::String(argument.name.clone()));
            }
            object.insert(
                "type".into(),
                Value::String(argument.argument_type.proto_name().to_owned()),
            );
            // ABSENT for every composite type and for a redacted or unavailable scalar.
            // An absent value is never "the empty string" — `type` and the two flags
            // say which it is, and writing `""` would erase that distinction.
            if !argument.value.is_empty() {
                object.insert("value".into(), Value::String(argument.value.clone()));
            }
            if argument.redacted {
                object.insert("redacted".into(), Value::Bool(true));
            }
            if argument.truncated {
                object.insert("truncated".into(), Value::Bool(true));
            }
            Value::Object(object)
        })
        .collect();
    json!({
        "invocation": sample.invocation.to_string(),
        "arguments": Value::Array(arguments),
        "argumentsDropped": Value::from(sample.arguments_dropped),
    })
}

/// Content-addressed atomic write to the `.dprofile` spool.
pub fn write_atomic(spool_directory: &str, body: &str) -> std::io::Result<()> {
    spool_common::write_atomic(spool_directory, body, EXTENSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deterministic::{
        ArgumentType, CapturedArgument, DeterministicBuffer, DeterministicConfig, FunctionFacts,
        FunctionOrigin, NameInterner, TierCoverage, Truncation,
    };
    use std::rc::Rc;

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

    fn interval() -> Window {
        Window {
            from: "2026-09-02T10:15:00.123456Z".into(),
            to: "2026-09-02T10:15:00.481920Z".into(),
            wall_nanoseconds: 358_464_000,
        }
    }

    fn context() -> TraceContext {
        TraceContext {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "00f067aa0ba902b7".into(),
            parent_span_id: None,
            sampled: true,
            session_id: Some("018f5b2c-7a41-7c3d-9e2a-0f1b2c3d4e5f".into()),
        }
    }

    fn no_labels() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// A two-function window: `Repository::find` calls `PDO::prepare`, Tier 2 on.
    fn window_with_edges() -> DeterministicWindow {
        let mut interner = NameInterner::new();
        let repository = interner
            .intern(1, || FunctionFacts {
                name: "App\\Orders\\Repository::find".to_owned(),
                origin: FunctionOrigin {
                    module: Rc::from("php"),
                    file: Rc::from("/srv/app/src/Orders/Repository.php"),
                    line: 41,
                },
                internal: false,
            })
            .id;
        let prepare = interner
            .intern(2, || FunctionFacts {
                name: "PDO::prepare".to_owned(),
                origin: FunctionOrigin::default(),
                internal: true,
            })
            .id;
        let mut buffer = DeterministicBuffer::new(DeterministicConfig {
            edges: true,
            ..DeterministicConfig::default()
        });
        let mut outer = buffer.on_enter(repository, 0);
        let inner = buffer.on_enter(prepare, 10);
        outer.child_nanoseconds += buffer.on_leave(&inner, Some(repository), 40);
        buffer.on_leave(&outer, None, 100);
        buffer.drain(&interner)
    }

    #[test]
    fn the_envelope_mirrors_the_deterministic_batch_proto_shape() {
        let window = window_with_edges();
        let body = serialise_batch(
            &envelope(),
            &window,
            &interval(),
            Some(&context()),
            &no_labels(),
        );
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["unit"], "nanoseconds");
        assert_eq!(value["organisation"]["organisationId"], "org-local");
        assert_eq!(value["application"]["applicationId"], "deepwell");
        assert_eq!(
            value["application"]["project"]["projectId"], "proj",
            "the tenant triple is what joins a .dprofile row to a .profile sample"
        );
        assert_eq!(value["window"]["wallNanoseconds"], "358464000");
        assert_eq!(
            value["correlation"]["traceId"],
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(value["functionCount"], "2");
        assert!(value["processing"]["messageId"].is_string());
    }

    #[test]
    fn the_series_id_is_the_base_suffixed_so_counted_and_sampled_never_merge() {
        let window = window_with_edges();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value["profileSeriesId"], "php-deterministic");
    }

    #[test]
    fn the_canonical_identity_round_trips_byte_for_byte() {
        // The single most important property of the document: this string is the join
        // key across collector, engine and desktop, and a mangled backslash would look
        // like missing data rather than like a bug.
        let window = window_with_edges();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        let names: Vec<&str> = value["functions"]
            .as_array()
            .expect("array")
            .iter()
            .map(|row| row["function"].as_str().expect("string"))
            .collect();
        assert!(names.contains(&"App\\Orders\\Repository::find"));
        assert!(names.contains(&"PDO::prepare"));
    }

    #[test]
    fn wide_integers_are_strings_and_narrow_ones_are_numbers() {
        let window = window_with_edges();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        let row = &value["functions"][0];
        assert!(row["callCount"].is_string());
        assert!(row["inclusiveNanoseconds"].is_string());
        assert!(row["exclusiveNanoseconds"].is_string());
        assert!(value["functionCount"].is_string());
        assert!(value["window"]["wallNanoseconds"].is_string());
        // uint32 stays a number, exactly as `line` does in the sample batch.
        assert!(row["line"].is_number());
        assert!(row["maxRecursionDepth"].is_number());
    }

    #[test]
    fn tier_two_edges_hang_off_their_caller_and_name_only_the_callee() {
        let window = window_with_edges();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        let caller = value["functions"]
            .as_array()
            .expect("array")
            .iter()
            .find(|row| row["function"] == "App\\Orders\\Repository::find")
            .expect("caller row");
        assert_eq!(caller["callees"][0]["callee"], "PDO::prepare");
        assert_eq!(caller["callees"][0]["inclusiveNanoseconds"], "30");
        // Direction comes from the nesting, so the caller is never repeated per edge.
        assert!(caller["callees"][0].get("caller").is_none());
        assert_eq!(value["tiers"]["edges"], true);
    }

    #[test]
    fn the_callees_key_is_absent_rather_than_empty_when_tier_two_is_off() {
        let mut interner = NameInterner::new();
        let id = interner
            .intern(1, || FunctionFacts {
                name: "App\\Support\\slugify".to_owned(),
                origin: FunctionOrigin::default(),
                internal: false,
            })
            .id;
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        let frame = buffer.on_enter(id, 0);
        buffer.on_leave(&frame, None, 5);
        let window = buffer.drain(&interner);
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert!(value["functions"][0].get("callees").is_none());
        assert_eq!(value["tiers"]["edges"], false);
    }

    #[test]
    fn tiers_and_truncation_are_always_declared() {
        // A document that omitted these would let a reader render "Tier 2 off"
        // identically to "this function calls nothing", and a capped list identically
        // to a complete one.
        let window = DeterministicWindow {
            functions: Vec::new(),
            coverage: TierCoverage::default(),
            truncation: Truncation::default(),
        };
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert!(value["tiers"].is_object());
        assert!(value["truncation"].is_object());
        assert_eq!(value["truncation"]["functionsSeen"], "0");
    }

    #[test]
    fn labels_are_omitted_entirely_when_there_are_none() {
        let window = window_with_edges();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert!(value.get("labels").is_none());
    }

    #[test]
    fn labels_describe_the_window_once_rather_than_every_row() {
        let window = window_with_edges();
        let labels: BTreeMap<String, String> = [
            ("route".to_owned(), "/orders/{id}".to_owned()),
            ("action".to_owned(), "orderActions::show".to_owned()),
        ]
        .into_iter()
        .collect();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &labels);
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value["labels"]["route"], "/orders/{id}");
        assert!(value["functions"][0].get("labels").is_none());
    }

    #[test]
    fn a_composite_argument_carries_its_type_and_no_value() {
        let mut interner = NameInterner::new();
        let id = interner
            .intern(1, || FunctionFacts {
                name: "App\\Orders\\Service::purchaseLabel".to_owned(),
                origin: FunctionOrigin::default(),
                internal: false,
            })
            .id;
        let mut buffer = DeterministicBuffer::new(DeterministicConfig {
            arguments: true,
            ..DeterministicConfig::default()
        });
        buffer.arm_arguments();
        let frame = buffer.on_enter(id, 0);
        buffer.on_leave(&frame, None, 10);
        buffer.record_arguments(
            id,
            vec![
                CapturedArgument {
                    position: 0,
                    name: "orderId".to_owned(),
                    argument_type: ArgumentType::Int,
                    value: "4711".to_owned(),
                    redacted: false,
                    truncated: false,
                },
                CapturedArgument {
                    position: 1,
                    name: "apiToken".to_owned(),
                    argument_type: ArgumentType::Str,
                    value: crate::http_capture::MASK.to_owned(),
                    redacted: true,
                    truncated: false,
                },
                CapturedArgument {
                    position: 2,
                    name: "options".to_owned(),
                    argument_type: ArgumentType::Array,
                    value: String::new(),
                    redacted: false,
                    truncated: false,
                },
            ],
            0,
        );
        let window = buffer.drain(&interner);
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        let sample = &value["functions"][0]["argumentSamples"][0];
        assert_eq!(sample["invocation"], "1");
        assert_eq!(
            sample["arguments"][0]["type"],
            "DETERMINISTIC_ARGUMENT_TYPE_INT"
        );
        assert_eq!(sample["arguments"][0]["value"], "4711");
        assert_eq!(sample["arguments"][1]["redacted"], true);
        assert_eq!(sample["arguments"][1]["value"], "********");
        // The whole shape of Tier 3: "argument 2 was an array" is evidence and leaks
        // nothing; the array itself is unbounded application data and is refused.
        assert_eq!(
            sample["arguments"][2]["type"],
            "DETERMINISTIC_ARGUMENT_TYPE_ARRAY"
        );
        assert!(sample["arguments"][2].get("value").is_none());
        assert_eq!(value["tiers"]["arguments"], true);
    }

    #[test]
    fn the_argument_samples_key_is_absent_when_tier_three_did_not_run() {
        let window = window_with_edges();
        let body = serialise_batch(&envelope(), &window, &interval(), None, &no_labels());
        let value: Value = serde_json::from_str(&body).expect("valid json");
        assert!(value["functions"][0].get("argumentSamples").is_none());
        assert_eq!(value["tiers"]["arguments"], false);
    }

    #[test]
    fn an_empty_window_writes_no_document_at_all() {
        // An empty `.dprofile` would claim coverage of a request in which nothing was
        // observed, which a reader cannot distinguish from "not covered".
        let directory = std::env::temp_dir().join(format!("chronos-dprofile-{}", hex_bytes(8)));
        let path = directory.to_string_lossy().to_string();
        let mut envelope = envelope();
        envelope.spool_directory = path.clone();
        let window = DeterministicWindow::default();
        flush_with_budget(
            &envelope,
            &window,
            &interval(),
            None,
            &no_labels(),
            900 * 1024,
        )
        .expect("flush");
        assert!(
            std::fs::read_dir(&envelope.tenant_spool_directory()).is_err()
                || std::fs::read_dir(&envelope.tenant_spool_directory())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .count()
                    == 0
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_flushed_window_lands_as_a_content_addressed_dprofile_file() {
        let directory = std::env::temp_dir().join(format!("chronos-dprofile-{}", hex_bytes(8)));
        let mut envelope = envelope();
        envelope.spool_directory = directory.to_string_lossy().to_string();
        let window = window_with_edges();
        flush_with_budget(
            &envelope,
            &window,
            &interval(),
            Some(&context()),
            &no_labels(),
            900 * 1024,
        )
        .expect("flush");
        let entries: Vec<String> = std::fs::read_dir(envelope.tenant_spool_directory())
            .expect("spool dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1, "one window is one document");
        let name = &entries[0];
        assert!(name.ends_with(".dprofile"), "routed by extension: {name}");
        // Content-addressed: the stem is the sha256 of the body, 64 hex characters.
        assert_eq!(name.len(), 64 + ".dprofile".len());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_window_over_the_budget_is_split_and_every_chunk_repeats_the_header() {
        // Each chunk must be independently ingestible: a reader never has to have seen
        // chunk 1 to make sense of chunk 2.
        let mut interner = NameInterner::new();
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        for index in 0..64u64 {
            let id = interner
                .intern(index as usize + 1, || FunctionFacts {
                    name: format!("App\\Generated\\Function{index}::run"),
                    origin: FunctionOrigin {
                        module: Rc::from("php"),
                        file: Rc::from("/srv/app/src/Generated/VeryLongPathForPadding.php"),
                        line: 1,
                    },
                    internal: false,
                })
                .id;
            let frame = buffer.on_enter(id, index * 10);
            buffer.on_leave(&frame, None, index * 10 + 5);
        }
        let window = buffer.drain(&interner);
        let directory = std::env::temp_dir().join(format!("chronos-dprofile-{}", hex_bytes(8)));
        let mut envelope = envelope();
        envelope.spool_directory = directory.to_string_lossy().to_string();
        flush_with_budget(&envelope, &window, &interval(), None, &no_labels(), 2_048)
            .expect("flush");
        let bodies: Vec<String> = std::fs::read_dir(envelope.tenant_spool_directory())
            .expect("spool dir")
            .flatten()
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .collect();
        assert!(bodies.len() > 1, "64 padded rows must not fit in 2 KiB");
        let mut total = 0usize;
        for body in &bodies {
            let value: Value = serde_json::from_str(body).expect("valid json");
            assert_eq!(value["schema"], SCHEMA);
            assert_eq!(value["profileSeriesId"], "php-deterministic");
            assert!(value["tiers"].is_object());
            assert!(value["chunk"]["groupId"].is_string());
            total += value["functions"].as_array().expect("array").len();
        }
        assert_eq!(total, 64, "every row survives the split");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
