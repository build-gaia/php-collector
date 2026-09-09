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

use crate::context::{hex_bytes, CollectorEnvelope};
use crate::http_capture::StoredBody;
use crate::spool_common;
use serde_json::json;

const SCHEMA: &str = "chronos.tracing.span-body.v1";

/// Bytes of body per document, under the engine streams' 1 MiB message limit
/// with room for the envelope around it.
const CHUNK_BYTES: usize = 512 * 1024;

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
