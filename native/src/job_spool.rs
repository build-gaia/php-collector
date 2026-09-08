//! The marker that says a job is running RIGHT NOW. Schema `chronos.messaging.job-run.v1`.
//!
//! Every other signal in this extension is written at the END of the unit of work
//! it describes: spans, metrics and DST recordings all leave at RSHUTDOWN, because
//! only then is there something complete to say. That is exactly why a background
//! job in flight is invisible — the span index only ever receives finished spans,
//! so a job that has been running for four minutes, the one an operator actually
//! wants to see, has no row anywhere until it stops being interesting.
//!
//! So this signal breaks the rule on purpose: it is written when the job STARTS
//! and it claims nothing about the outcome. It is the only spool document that
//! describes something still happening.
//!
//! # It closes itself
//!
//! There is no "job finished" counterpart, because there already is one: the job's
//! root span. The marker carries the trace and span ids the job-scoped request was
//! opened with, so the completing span closes the in-flight row by identity —
//! exact, and free. A second document would only be a second thing to lose.
//!
//! # Why a deadline and not a heartbeat
//!
//! A worker killed mid-job (OOM, SIGKILL, a machine going away) never sends its
//! span, so a row would otherwise sit in "running" forever. The obvious fix is a
//! heartbeat, and it is the wrong one: PHP hands userland no timer that does not
//! involve signals, and a worker's SIGALRM is already Laravel's job-timeout
//! mechanism. A job cannot legitimately outlive its own timeout, so the timeout IS
//! the deadline — stamped here, at the start, by the side that knows it. A reaper
//! comparing `now` to `deadline_at` needs nothing else, and no signal handling is
//! introduced into somebody's worker.
//!
//! `deadline_at` is absent when the framework reports no timeout. Absent, not
//! guessed: an invented deadline reaps a job that is still working.

use crate::context::CollectorEnvelope;
use crate::spool_common;
use serde_json::{json, Map, Value};

const SCHEMA: &str = "chronos.messaging.job-run.v1";

/// The maximum number of facts carried alongside the run, and the cap on each.
/// The same shape as the request-attribute caps: this document is written on a
/// hot path and must have a bounded size whatever a caller passes.
const MAX_FACTS: usize = 24;
const MAX_VALUE: usize = 512;

/// One job, at the moment it began.
pub struct JobRun {
    pub trace_id: String,
    pub span_id: String,
    /// The job's own name — `messaging.message.name`, the application's name for
    /// it rather than the framework's wrapper class.
    pub name: String,
    /// Seconds from now until the job may be presumed dead, when the framework
    /// knows. `None` leaves the document without a deadline.
    pub timeout_seconds: Option<u64>,
    /// The `messaging.*` facts the root span carries, so the in-flight view can
    /// group by queue and system without waiting for the span.
    pub facts: std::collections::HashMap<String, String>,
}

/// Serialise one starting job. Separated from the write so a test can read the
/// document without a spool directory.
pub fn serialise(envelope: &CollectorEnvelope, run: &JobRun) -> String {
    let now = chrono::Utc::now();

    let mut facts: Vec<(&String, &String)> = run
        .facts
        .iter()
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect();
    // Sorted so the same starting job serialises to the same bytes, which is what
    // makes the content-addressed filename a deduplicator rather than a random
    // name. Two markers for one job (a retried write, a doubled event) then
    // collapse to one file.
    facts.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut fact_map = Map::new();
    for (key, value) in facts.into_iter().take(MAX_FACTS) {
        fact_map.insert(
            key.clone(),
            Value::String(spool_common::cap(value, MAX_VALUE)),
        );
    }

    let mut body = spool_common::identity_envelope(envelope);
    body.insert("schema".into(), Value::String(SCHEMA.into()));
    // Derived from the run's identity rather than minted at random, so the
    // deduplication the transport already performs on `messageId` collapses a
    // re-shipped marker instead of admitting the same job twice.
    body.insert(
        "processing".into(),
        json!({
            "messageId": spool_common::hex_digest(&format!(
                "{}|{}|{}|{}",
                envelope.organisation_id, envelope.application_id, run.trace_id, run.span_id
            )),
        }),
    );
    body.insert("event".into(), Value::String("start".into()));
    body.insert("startedAt".into(), Value::String(format_instant(now)));
    // A worker is a process on a host, and "which box is chewing on this" is the
    // first question asked about a job that will not finish.
    body.insert("worker".into(), json!({ "pid": std::process::id() }));
    body.insert(
        "run".into(),
        json!({
            "traceId": run.trace_id,
            "spanId": run.span_id,
            "name": spool_common::cap(&run.name, MAX_VALUE),
        }),
    );
    if let Some(seconds) = run.timeout_seconds.filter(|seconds| *seconds > 0) {
        // Stamped as an instant rather than a duration: the reaper compares it to
        // its own clock, and a duration would make it reconstruct the sum from a
        // start it may have received late.
        let deadline = now + chrono::Duration::seconds(seconds as i64);
        body.insert("deadlineAt".into(), Value::String(format_instant(deadline)));
        body.insert("timeoutSeconds".into(), json!(seconds));
    }
    body.insert("facts".into(), Value::Object(fact_map));

    serde_json::to_string(&Value::Object(body)).unwrap_or_else(|_| "{}".to_owned())
}

/// Write the marker to the tenant spool for the sidecar to ship.
pub fn flush(envelope: &CollectorEnvelope, run: &JobRun) -> std::io::Result<()> {
    let body = serialise(envelope, run);
    spool_common::write_atomic(&envelope.tenant_spool_directory(), &body, "jobrun")
}

fn format_instant(instant: chrono::DateTime<chrono::Utc>) -> String {
    instant.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> CollectorEnvelope {
        CollectorEnvelope {
            organisation_id: "org_local".into(),
            project_id: "proj".into(),
            application_id: "deepwell".into(),
            app_version: None,
            spool_directory: "/tmp/spool".into(),
            app_language: "php".into(),
            app_language_version: None,
            app_framework: None,
            app_framework_version: None,
        }
    }

    fn run() -> JobRun {
        JobRun {
            trace_id: "0123456789abcdef0123456789abcdef".into(),
            span_id: "0123456789abcdef".into(),
            name: "App\\Jobs\\IndexUser".into(),
            timeout_seconds: Some(60),
            facts: std::collections::HashMap::from([
                ("messaging.destination.name".into(), "default".into()),
                ("messaging.system".into(), "redis".into()),
                ("".into(), "dropped".into()),
                ("messaging.message.id".into(), "".into()),
            ]),
        }
    }

    fn parse(body: &str) -> Value {
        serde_json::from_str(body).expect("the marker is JSON")
    }

    #[test]
    fn the_marker_names_the_span_that_will_close_it() {
        let document = parse(&serialise(&envelope(), &run()));

        assert_eq!(document["schema"], json!(SCHEMA));
        assert_eq!(document["event"], json!("start"));
        assert_eq!(
            document["run"]["traceId"],
            json!("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(document["run"]["spanId"], json!("0123456789abcdef"));
        // One job run is one message id, whoever ships it and however often.
        let repeated = parse(&serialise(&envelope(), &run()));
        assert_eq!(
            document["processing"]["messageId"],
            repeated["processing"]["messageId"],
        );
        assert_ne!(
            document["processing"]["messageId"],
            parse(&serialise(
                &envelope(),
                &JobRun {
                    span_id: "fedcba9876543210".into(),
                    ..run()
                }
            ))["processing"]["messageId"],
            "a different run is a different message"
        );
        assert_eq!(
            document["application"]["applicationId"],
            json!("deepwell"),
            "the marker carries the same identity a span batch does"
        );
    }

    #[test]
    fn a_timeout_becomes_a_deadline_and_no_timeout_becomes_no_deadline() {
        let with = parse(&serialise(&envelope(), &run()));
        assert!(
            with["deadlineAt"].is_string(),
            "a job that can time out can be reaped"
        );
        assert_eq!(with["timeoutSeconds"], json!(60));

        // The reaper must not be handed a number nobody measured: a job whose
        // framework reports no timeout is left without a deadline rather than
        // given a guessed one, because reaping a job that is still working
        // reports a lie about the estate.
        for absent in [None, Some(0)] {
            let document = parse(&serialise(
                &envelope(),
                &JobRun {
                    timeout_seconds: absent,
                    ..run()
                },
            ));
            assert!(document.get("deadlineAt").is_none());
            assert!(document.get("timeoutSeconds").is_none());
        }
    }

    #[test]
    fn empty_and_overlong_facts_cannot_reach_the_document() {
        let document = parse(&serialise(&envelope(), &run()));
        let facts = document["facts"].as_object().expect("facts is an object");

        assert_eq!(facts["messaging.system"], json!("redis"));
        assert!(!facts.contains_key(""), "an empty key is dropped");
        assert!(
            !facts.contains_key("messaging.message.id"),
            "an empty value is dropped rather than written blank"
        );

        let long = "x".repeat(MAX_VALUE * 2);
        let document = parse(&serialise(
            &envelope(),
            &JobRun {
                facts: std::collections::HashMap::from([("messaging.system".into(), long)]),
                ..run()
            },
        ));
        assert_eq!(
            document["facts"]["messaging.system"]
                .as_str()
                .map(str::len),
            Some(MAX_VALUE)
        );
    }

    #[test]
    fn facts_are_written_in_a_fixed_order() {
        // Content-addressed naming makes identical bodies ONE file, which is the
        // de-duplication for a doubled JobProcessing event — but only while a
        // HashMap's iteration order cannot change the bytes. Asserted on the
        // serialised text rather than by comparing two maps, because the source
        // of the disorder is exactly the thing a test cannot ask for twice.
        let many: std::collections::HashMap<String, String> = (0..MAX_FACTS)
            .map(|index| (format!("messaging.fact.{index:03}"), "value".to_owned()))
            .collect();
        let body = serialise(
            &envelope(),
            &JobRun {
                facts: many,
                ..run()
            },
        );

        let positions: Vec<usize> = (0..MAX_FACTS)
            .map(|index| {
                body.find(&format!("messaging.fact.{index:03}"))
                    .expect("every fact under the cap is written")
            })
            .collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(positions, sorted, "facts appear in key order");
    }

    #[test]
    fn at_most_the_fact_cap_is_carried() {
        let many: std::collections::HashMap<String, String> = (0..MAX_FACTS * 2)
            .map(|index| (format!("messaging.fact.{index:03}"), "value".to_owned()))
            .collect();
        let document = parse(&serialise(
            &envelope(),
            &JobRun {
                facts: many,
                ..run()
            },
        ));

        assert_eq!(document["facts"].as_object().map(Map::len), Some(MAX_FACTS));
    }
}
