//! Whole HTTP bodies, spooled beside the span that carries their preview.
//! Schema `chronos.tracing.span-body.v1`.
//!
//! A body used to be one span attribute and nothing else, which put a hard
//! ceiling on it that had nothing to do with what anyone wanted to keep: a span
//! rides a `.trace` document bounded by `CHRONOS_PHP_SPOOL_MAX_BYTES`, and a span
//! cannot be split across documents, so a 2 MB JSON export was cut at 64 KiB and
//! the reader was shown the first two per cent of it.
//!
//! So the body leaves the span. The attribute keeps a preview — the common case
//! is small and an extra round trip to see it would be worse than the cap ever
//! was — and anything past that is written here as its own numbered documents,
//! keyed by the `(trace, span, side)` the span itself already reports. Nothing on
//! the read path has to change to KEEP working; only the "load the rest" button
//! is new.
//!
//! Two bounds, and they are different in kind:
//!
//! * `CHUNK_BYTES` is a transport fact. One document becomes one NATS message,
//!   and the engine's streams are provisioned at 1 MiB per message, so a document
//!   over that is not slow — it is refused at the far end after the write, the
//!   scan and the POST have already been paid for.
//! * The TOTAL captured is a policy, and it lives in `http_capture`'s config with
//!   the rest of what capture is allowed to keep.
//!
//! Chunks are numbered and carry their own count, so the indexer never has to
//! have seen chunk 0 to store chunk 3, and a body whose tail was lost is
//! detectable rather than silently short.
//!
//! # Two producers, one store
//!
//! The HTTP path drains at request end and hands over every body it kept for
//! ONE span — the request root — through [`flush`]. Userland (a message payload,
//! `chronos_store_span_body`) cannot work that way: its bodies belong to spans
//! the PHP SDK minted, and it hands them over one at a time while the request is
//! still running. Those are buffered request-locally as [`PendingBody`] and
//! written by [`flush_pending`] at the same moment, immediately before the span
//! batch — so the `.stored` marker on a span can never be shipped ahead of the
//! bytes it promises. The buffer exists rather than an inline write because a
//! `CollectorEnvelope` is only assembled at request end, and because a file
//! write per publish inside a loop would be its own outage.

use crate::context::{hex_bytes, CollectorEnvelope};
use crate::http_capture::StoredBody;
use crate::spool_common;
use serde_json::json;
use std::cell::RefCell;

const SCHEMA: &str = "chronos.tracing.span-body.v1";

/// Bytes of body per document, under the engine streams' 1 MiB message limit
/// with room for the envelope around it.
const CHUNK_BYTES: usize = 512 * 1024;

/// The most bodies one request may buffer for the userland store.
///
/// Deliberately NOT sized by analogy to `log_spool`'s 512 records: a log body is
/// capped at a kilobyte, while one of these is capped by
/// `CHRONOS_PHP_MESSAGING_CAPTURE_MAX_BODY`, whose hard clamp is 512 KiB — so 512
/// of them would be a quarter of a gigabyte held in a request that is still
/// running. Sixteen is one publish loop's worth of evidence: 1 MiB at the 64 KiB
/// default, 8 MiB at the clamp, both of which a request can carry. A publisher
/// that sends more than sixteen messages in one request keeps the first sixteen
/// payloads and the spans for all of them — the seventeenth span simply does not
/// claim `.stored`, which is the honest degradation, because
/// [`crate::body_spool::capture`] reports the refusal rather than swallowing it.
const MAX_PENDING_BODIES: usize = 16;

/// A whole payload handed over by userland, waiting for request end.
///
/// Its own `(trace, span)` rather than the request's, because the span this
/// belongs to is often NOT the request root: a publish span is a child minted by
/// the PHP SDK, and it is the span whose attributes carry the preview and the
/// `.stored` marker this row exists to honour. `observed_at` is the REQUEST's
/// start instant, carried for the same reason [`flush`] carries it — see its
/// docblock on derived identity.
pub struct PendingBody {
    pub trace_id: String,
    pub span_id: String,
    pub observed_at: String,
    pub body: StoredBody,
}

thread_local! {
    static PENDING: RefCell<Vec<PendingBody>> = const { RefCell::new(Vec::new()) };
}

/// Clear the request-local buffer. Called from request start, because these are
/// thread-locals that outlive one request inside an FPM worker.
pub fn reset() {
    PENDING.with(|pending| pending.borrow_mut().clear());
}

/// Buffer one payload for the flush at request end, reporting whether it was
/// taken.
///
/// The bool is the whole point: the caller only stamps
/// `messaging.message.body.stored` on its span when this said yes, so a marker
/// never promises bytes the budget refused. Buffered rather than written inline
/// because `CollectorEnvelope` and the request's start instant are only
/// assembled at request end — and because per-call file I/O inside a publish
/// loop would be its own outage.
pub fn capture(body: PendingBody) -> bool {
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.len() >= MAX_PENDING_BODIES {
            return false;
        }
        pending.push(body);
        true
    })
}

pub fn drain() -> Vec<PendingBody> {
    PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()))
}

/// Write every buffered payload, each keyed by the span that claimed it.
///
/// Separate from [`flush`] rather than a widening of it: the HTTP path holds one
/// (trace, span) for every body it drained — they all belong to the request root
/// — while these each name their own span. Both loop the same `flush_one`, so
/// the document shape, the chunking and the derived identity are shared.
pub fn flush_pending(
    envelope: &CollectorEnvelope,
    bodies: &[PendingBody],
) -> std::io::Result<()> {
    let mut result = Ok(());
    for pending in bodies {
        if pending.body.bytes.is_empty() {
            continue;
        }
        // Every body is attempted even after a failure, for the reason [`flush`]
        // gives about the chunks of one body: abandoning the rest because one
        // write failed loses more than it protects.
        if let Err(error) = flush_one(
            envelope,
            &pending.trace_id,
            &pending.span_id,
            &pending.observed_at,
            &pending.body,
        ) {
            result = Err(error);
        }
    }
    result
}

/// Write every body as numbered documents. Bodies with nothing in them are skipped.
///
/// `started_at` is the request's own start instant, carried verbatim so a chunk's
/// row lands in the same retention window as the span it belongs to — and so the
/// row's identity is DERIVED rather than assigned. A chunk redelivered by the
/// broker produces byte-identical keys, which is what makes the store's write
/// idempotent without a dedupe table.
pub fn flush(
    envelope: &CollectorEnvelope,
    trace_id: &str,
    span_id: &str,
    started_at: &str,
    bodies: &[StoredBody],
) -> std::io::Result<()> {
    let mut result = Ok(());
    for body in bodies {
        if body.bytes.is_empty() {
            continue;
        }
        // Every chunk is attempted even after a failure: abandoning the tail of a
        // body because its third document could not be written loses more than it
        // protects, and the count on each document is what makes the gap visible.
        if let Err(error) = flush_one(envelope, trace_id, span_id, started_at, body) {
            result = Err(error);
        }
    }
    result
}

fn flush_one(
    envelope: &CollectorEnvelope,
    trace_id: &str,
    span_id: &str,
    started_at: &str,
    body: &StoredBody,
) -> std::io::Result<()> {
    let chunks = split(&body.bytes);
    let total = chunks.len();
    let directory = envelope.tenant_spool_directory();
    let mut result = Ok(());
    for (index, chunk) in chunks.iter().enumerate() {
        let document = json!({
            "schema": SCHEMA,
            "processing": { "messageId": hex_bytes(16), "batchId": hex_bytes(16) },
            "organisation": { "organisationId": envelope.organisation_id },
            "application": { "applicationId": envelope.application_id },
            "traceId": trace_id,
            "spanId": span_id,
            "observedAt": started_at,
            "side": body.side,
            "contentType": body.content_type,
            "totalBytes": body.bytes.len().to_string(),
            "chunkIndex": index.to_string(),
            "chunkCount": total.to_string(),
            "bytes": chunk,
        });
        let serialised =
            serde_json::to_string(&document).unwrap_or_else(|_| "{}".to_owned());
        if let Err(error) = spool_common::write_atomic(&directory, &serialised, "body") {
            result = Err(error);
        }
    }
    result
}

/// Split on byte count without ever cutting a UTF-8 character.
///
/// A chunk therefore holds AT MOST `CHUNK_BYTES` and may hold less, which is why
/// reassembly concatenates in index order rather than seeking by offset.
fn split(body: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        if rest.len() <= CHUNK_BYTES {
            chunks.push(rest);
            break;
        }
        let mut end = CHUNK_BYTES;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        // A single character wider than the whole budget cannot happen (UTF-8 is
        // at most four bytes), so `end` is never 0 here for a non-empty chunk.
        let (head, tail) = rest.split_at(end);
        chunks.push(head);
        rest = tail;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_inside_one_chunk_is_one_chunk() {
        assert_eq!(split("hello"), vec!["hello"]);
        assert!(split("").is_empty());
    }

    #[test]
    fn chunks_reassemble_to_the_original_body() {
        let body = "x".repeat(CHUNK_BYTES * 2 + 17);
        let chunks = split(&body);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn the_pending_buffer_refuses_past_its_budget_and_says_so() {
        reset();
        for index in 0..MAX_PENDING_BODIES {
            assert!(
                capture(pending(&format!("span-{index}"))),
                "body {index} must be taken"
            );
        }
        // The refusal is the contract: a caller that is told "no" must not stamp
        // `.stored`, which is the only thing standing between a marker and a
        // promise nothing can honour.
        assert!(!capture(pending("one-too-many")));
        let drained = drain();
        assert_eq!(drained.len(), MAX_PENDING_BODIES);
        assert!(drain().is_empty(), "draining empties the buffer");
    }

    #[test]
    fn each_pending_body_keeps_its_own_span() {
        reset();
        assert!(capture(pending("publish-span")));
        assert!(capture(pending("root-span")));
        let drained = drain();
        assert_eq!(drained[0].span_id, "publish-span");
        assert_eq!(drained[1].span_id, "root-span");
        assert_eq!(drained[0].body.side, "message");
    }

    fn pending(span_id: &str) -> PendingBody {
        PendingBody {
            trace_id: "trace".to_owned(),
            span_id: span_id.to_owned(),
            observed_at: "2026-09-11T00:00:00.000000Z".to_owned(),
            body: StoredBody {
                side: "message",
                content_type: "application/x-protobuf".to_owned(),
                bytes: "AAEC".to_owned(),
            },
        }
    }

    #[test]
    fn a_multibyte_character_is_never_split_across_chunks() {
        // Every chunk must decode on its own — an indexer storing chunk 1 has no
        // access to the tail of a character left behind in chunk 0.
        let body = "é".repeat(CHUNK_BYTES);
        let chunks = split(&body);
        assert_eq!(chunks.concat(), body);
        for chunk in &chunks {
            assert!(chunk.len() <= CHUNK_BYTES);
            assert_eq!(chunk.chars().count() * 2, chunk.len());
        }
    }
}
