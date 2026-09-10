//! Shared spool utilities for content-addressed atomic writes.
//!
//! Both `spool.rs` (spans) and `profile_spool.rs` (profiler samples) produce the same
//! identity-envelope structure and use the same atomic-write pattern (SHA-256 content address,
//! tmp-then-rename, mode 0600). This module owns those shared primitives.

use crate::context::CollectorEnvelope;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;

pub fn identity_envelope(envelope: &CollectorEnvelope) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
        "organisation".into(),
        json!({ "organisationId": envelope.organisation_id }),
    );
    map.insert(
        "application".into(),
        json!({
            "project": {
                "organisation": { "organisationId": envelope.organisation_id },
                "projectId": envelope.project_id,
            },
            "applicationId": envelope.application_id,
        }),
    );
    map
}

pub fn hex_digest(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(unix)]
pub fn set_mode_0600(file: &fs::File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn set_mode_0600(_file: &fs::File) {}

/// Append one document to the tenant's spool log as a framed entry (ADR 0035).
///
/// The name is kept from the era when this wrote `<sha256(body)>.<ext>` through a
/// temp file, an `fsync` and a rename. Every caller's contract is unchanged — hand
/// over a body and the signal it is, and it reaches the agent — but the write is
/// now a single append to a shared segment, with no sync on the request path.
///
/// `extension` is carried into the frame header as the signal name, so what used
/// to select an ingest endpoint by filename now selects it by frame.
///
/// The content address survives as the frame's `id`: it is what deduplicates a
/// document that gets re-shipped after a failed POST, a job the filename used to
/// do.
pub fn write_atomic(spool_directory: &str, body: &str, extension: &str) -> std::io::Result<()> {
    crate::spool_log::append(
        spool_directory,
        extension,
        "json",
        &hex_digest(body),
        body.as_bytes(),
    )
}

/// The pre-ADR-0035 write: content-addressed, synced, renamed into place.
///
/// Retained for the alias window (an agent that predates the framed log still
/// ships whole files) and as the fallback for a spool directory the log layout
/// refuses. Not on the request path.
pub fn write_document_file(
    spool_directory: &str,
    body: &str,
    extension: &str,
) -> std::io::Result<()> {
    fs::create_dir_all(spool_directory)?;
    let id = hex_digest(body);
    let tmp = format!("{spool_directory}/{id}.{extension}.tmp");
    let final_path = format!("{spool_directory}/{id}.{extension}");

    let mut file = fs::File::create(&tmp)?;
    set_mode_0600(&file);
    file.write_all(body.as_bytes())?;
    drop(file);

    if let Err(err) = fs::rename(&tmp, &final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// The per-document byte budget for a chunked spool write.
///
/// Sized to sit under the engine's own ingest cap with headroom for the transport:
/// a document that only fails at the far end has already cost the write, the scan
/// and the POST, and it dead-letters as a whole batch rather than degrading.
pub const DEFAULT_MAX_BODY_BYTES: usize = 900 * 1024;

/// Truncate `value` to at most `max_bytes`, never splitting a UTF-8 character.
///
/// Byte-bounded rather than character-bounded because every consumer of this —
/// a jsonb column, a document budget — is counting bytes, and a cap that counted
/// characters would let one multi-byte string overrun a limit expressed in bytes.
pub fn cap(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// The configured budget (`CHRONOS_PHP_SPOOL_MAX_BYTES`), or the default.
pub fn max_body_bytes() -> usize {
    crate::settings::get("CHRONOS_PHP_SPOOL_MAX_BYTES")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

/// Write `items` as one or more documents, each under `max_body_bytes`.
///
/// Chunking is by BYTES, not by item count, because the items here have no
/// characteristic size: a bare profiler stack is a couple of hundred bytes and a
/// 127-frame one is tens of kilobytes, so any fixed count is either wasteful or
/// over the cap. Every chunk repeats `header` verbatim and carries its own count,
/// so each file is independently ingestible — a reader never has to have seen chunk
/// 1 to make sense of chunk 2.
///
/// `chunk.groupId` is how the files find each other again afterwards; it is the
/// FILE-reassembly identity and deliberately distinct from any id the header
/// already carries for the recording itself.
///
/// A single item that exceeds the budget on its own is still written, alone: dropping
/// it would lose the one sample or event most likely to matter, and the engine's cap
/// is the right place for that verdict.
pub fn write_chunked(
    spool_directory: &str,
    extension: &str,
    header: &Map<String, Value>,
    items_key: &str,
    count_key: Option<&str>,
    items: &[Value],
    max_body_bytes: usize,
) -> std::io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    // Measured once against an EMPTY item list so the fixed cost of the envelope is
    // known before packing, rather than rediscovered on every append.
    let overhead = document(header, items_key, count_key, &[], "", 0, 0).len();
    let budget = max_body_bytes.saturating_sub(overhead).max(1);

    let mut chunks: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut bytes = 0usize;
    for item in items {
        let size = serde_json::to_string(item).map(|s| s.len()).unwrap_or(0) + 1;
        if !current.is_empty() && bytes + size > budget {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push(item.clone());
        bytes += size;
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let group = crate::context::hex_bytes(16);
    let total = chunks.len();
    let mut result = Ok(());
    for (index, chunk) in chunks.iter().enumerate() {
        let body = document(header, items_key, count_key, chunk, &group, index, total);
        // Every chunk is attempted even after a failure: the alternative is to
        // abandon the rest of a recording because its third file could not be
        // written, which loses more than it protects.
        if let Err(error) = write_atomic(spool_directory, &body, extension) {
            result = Err(error);
        }
    }
    result
}

fn document(
    header: &Map<String, Value>,
    items_key: &str,
    count_key: Option<&str>,
    items: &[Value],
    group: &str,
    index: usize,
    total: usize,
) -> String {
    let mut object = header.clone();
    object.insert(items_key.to_owned(), Value::Array(items.to_vec()));
    if let Some(count_key) = count_key {
        object.insert(count_key.to_owned(), Value::String(items.len().to_string()));
    }
    if total > 1 {
        object.insert(
            "chunk".to_owned(),
            json!({
                "groupId": group,
                "index": index.to_string(),
                "count": total.to_string(),
            }),
        );
    }
    serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_owned())
}
