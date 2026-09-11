//! The framed, append-only spool log the collector writes (ADR 0035).
//!
//! One tenant spool directory holds a sequence of segments —
//! `segment-000001.spool`, `segment-000002.spool`, … — and every document is
//! appended to the newest one as a single self-describing frame. This replaces
//! one-file-per-document, and with it the `fsync` that document write performed
//! on the request's own critical path.
//!
//! # The three rules that make it safe
//!
//! 1. **One frame is one `write`.** A frame is assembled fully in memory and
//!    emitted in a single syscall on a descriptor opened `O_APPEND`, which Linux
//!    serialises on the inode lock. That is what lets every PHP-FPM worker on
//!    the host append to one file with no lock and no interleaving. A short
//!    write is treated as a failed write, never continued — a torn frame would
//!    cost the reader the frames after it.
//! 2. **The writer rotates; the reader deletes.** A worker that finds the active
//!    segment at or over [`SEGMENT_MAX_BYTES`] creates the next generation with
//!    `create_new`, so exactly one racing worker wins and the losers open the
//!    winner's file. The agent deletes a retired segment once its cursor has
//!    passed the end of it — never the writer, which cannot know what has been
//!    shipped.
//! 3. **Nothing is synced.** ADR 0035 §1: the spool must survive process death,
//!    which the page cache already provides, not a power cut. The only `fsync`
//!    left in the pipeline is the agent's cursor checkpoint.
//!
//! # Fail-open, out loud
//!
//! When the segment budget ([`MAX_SEGMENTS`]) is reached the OLDEST segment is
//! removed to make room, which can discard frames the agent never shipped. That
//! is the intended behaviour for telemetry on a host whose ingest is down or
//! whose disk is finite — but it is only acceptable if it is counted, so every
//! drop increments [`dropped`] and says so once per occurrence.
//!
//! The same rule now covers the case that cost the most: a spool directory the
//! worker cannot WRITE. Every signal in this extension reaches the disk through
//! [`append`] — spans, logs, profiles, bodies, DST, job markers, deterministic
//! aggregates — and every one of their call sites discards the error with
//! `let _ =`, deliberately, because telemetry must never break a request. The
//! consequence was that an `EACCES` on the spool directory discarded a whole
//! host's telemetry in perfect silence, indistinguishable from a collector that
//! was never switched on. [`report_failure`] closes that: one chokepoint, one
//! line per `io::ErrorKind` per process, naming the directory, the errno kind
//! and the running totals. Latched per KIND rather than once globally, so a
//! `PermissionDenied` at boot cannot mask a `StorageFull` an hour later; and
//! once per process rather than per call, because a warning on a hot path is its
//! own outage — which is the rule the segment-budget warning above already
//! follows.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Serialize;

/// The four bytes that begin every frame. See `engine-core`'s `spool_frame`.
const FRAME_MAGIC: [u8; 4] = *b"CHRN";

/// The largest payload a single frame may carry, matching the reader's budget.
const MAX_FRAME_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// The size at which the active segment is retired and a new one started.
///
/// 64 MiB rather than the gigabyte an append log invites: un-shipped telemetry
/// is worth less than the host's remaining disk, and a full disk is a far worse
/// outcome for the application being observed than a dropped span batch.
pub const SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// How many segments one tenant directory may hold before the oldest is dropped.
pub const MAX_SEGMENTS: usize = 4;

const SEGMENT_PREFIX: &str = "segment-";
const SEGMENT_EXTENSION: &str = "spool";

static DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);
static DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);

/// One latch per `io::ErrorKind` this module can meet, indexed by
/// [`error_slot`]. A fixed array rather than a map so the failure path allocates
/// nothing and takes no lock — it is running because the disk already did not
/// work.
static ANNOUNCED: [AtomicBool; ERROR_KINDS] = [const { AtomicBool::new(false) }; ERROR_KINDS];

const ERROR_KINDS: usize = 6;

/// Which latch an error claims. Grouped by what an operator would DO about it,
/// not by the full `ErrorKind` enum: permissions, a missing path, a full disk, a
/// frame this module refused, a short write, and everything else.
fn error_slot(kind: std::io::ErrorKind) -> usize {
    match kind {
        std::io::ErrorKind::PermissionDenied => 0,
        std::io::ErrorKind::NotFound => 1,
        std::io::ErrorKind::StorageFull => 2,
        std::io::ErrorKind::InvalidInput => 3,
        std::io::ErrorKind::WriteZero => 4,
        _ => 5,
    }
}

/// Announce a spool write that failed — once per process per error kind.
///
/// Called from [`append`]'s own error returns rather than from its eight
/// callers. They all discard the error by design (a request must not fail
/// because a span could not be spooled), so a warning at each would be eight
/// latches, eight wordings and eight chances to forget the ninth; every one of
/// them reaches this syscall, so one report here makes all of them audible.
///
/// The wording names the SAPI reality deliberately: under PHP-FPM each worker is
/// its own process with its own latch, so a 32-worker pool prints up to 32 of
/// these when the spool first fails. That is the correct trade — each worker
/// counts its own losses — but a reader who did not know it would read the
/// repetition as a loop.
fn report_failure(spool_directory: &str, error: &std::io::Error) {
    let slot = error_slot(error.kind());
    if ANNOUNCED[slot].swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "[chronos-ext] spool write failed ({:?}: {error}) at {spool_directory}: this \
         process's telemetry is being discarded. {} frames ({} bytes) lost so far. \
         Reported once per process per error kind, and every PHP worker is its own \
         process.",
        error.kind(),
        dropped(),
        dropped_bytes(),
    );
}

/// Frames this process has discarded, by any path: segment budget reached, or a
/// write that could not be completed atomically.
pub fn dropped() -> u64 {
    DROPPED_FRAMES.load(Ordering::Relaxed)
}

/// Bytes discarded alongside [`dropped`].
pub fn dropped_bytes() -> u64 {
    DROPPED_BYTES.load(Ordering::Relaxed)
}

#[derive(Serialize)]
struct FrameHeader<'a> {
    signal: &'a str,
    encoding: &'a str,
    id: &'a str,
}

/// CRC-32 (IEEE) table, computed at compile time.
const fn checksum_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0_usize;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ 0xEDB8_8320
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

static CHECKSUM_TABLE: [u32; 256] = checksum_table();

/// CRC-32 (IEEE) over `bytes` — the reader's `spool_frame::checksum`, byte for
/// byte. Table-driven because this runs per request over documents up to the
/// spool's 900 KiB budget.
fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        let index = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CHECKSUM_TABLE[index];
    }
    !crc
}

/// Encode one frame, or `None` if the document cannot be framed at all.
fn frame(signal: &str, encoding: &str, id: &str, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > MAX_FRAME_PAYLOAD_BYTES || signal.is_empty() || encoding.is_empty() {
        return None;
    }
    let header = serde_json::to_vec(&FrameHeader {
        signal,
        encoding,
        id,
    })
    .ok()?;
    let header_len = u16::try_from(header.len()).ok()?;
    let payload_len = u32::try_from(payload.len()).ok()?;

    let mut body = Vec::with_capacity(header.len() + payload.len());
    body.extend_from_slice(&header);
    body.extend_from_slice(payload);

    let mut encoded = Vec::with_capacity(14 + body.len());
    encoded.extend_from_slice(&FRAME_MAGIC);
    encoded.extend_from_slice(&header_len.to_le_bytes());
    encoded.extend_from_slice(&payload_len.to_le_bytes());
    encoded.extend_from_slice(&checksum(&body).to_le_bytes());
    encoded.extend_from_slice(&body);
    Some(encoded)
}

fn segment_name(generation: u64) -> String {
    format!("{SEGMENT_PREFIX}{generation:06}.{SEGMENT_EXTENSION}")
}

fn generation_of(name: &str) -> Option<u64> {
    name.strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(&format!(".{SEGMENT_EXTENSION}"))?
        .parse()
        .ok()
}

/// Every segment generation present, ascending.
pub fn generations(spool_directory: &str) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(spool_directory) else {
        return Vec::new();
    };
    let mut generations = entries
        .filter_map(Result::ok)
        .filter_map(|entry| generation_of(entry.file_name().to_string_lossy().as_ref()))
        .collect::<Vec<_>>();
    generations.sort_unstable();
    generations
}

/// Open the newest segment for appending, creating the first one if the
/// directory is empty.
fn open_active(spool_directory: &str) -> std::io::Result<(u64, File)> {
    loop {
        let generation = generations(spool_directory).last().copied();
        match generation {
            Some(generation) => {
                let path = format!("{spool_directory}/{}", segment_name(generation));
                match OpenOptions::new().append(true).open(&path) {
                    Ok(file) => return Ok((generation, file)),
                    // Retired under us by the agent between the scan and the
                    // open: rescan rather than recreate a generation the reader
                    // has already finished with.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            }
            None => match create_segment(spool_directory, 1) {
                Ok(file) => return Ok((1, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            },
        }
    }
}

fn create_segment(spool_directory: &str, generation: u64) -> std::io::Result<File> {
    let path = format!("{spool_directory}/{}", segment_name(generation));
    let file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)?;
    crate::spool_common::set_mode_0600(&file);
    Ok(file)
}

/// Retire the active segment and return the next one.
///
/// `create_new` is the whole concurrency story: several workers can reach the
/// threshold at once, exactly one creates generation N+1, and the rest see
/// `AlreadyExists` and open what the winner made.
fn rotate(spool_directory: &str, active: u64) -> std::io::Result<(u64, File)> {
    let next = active.saturating_add(1);
    match create_segment(spool_directory, next) {
        Ok(file) => {
            enforce_segment_budget(spool_directory);
            Ok((next, file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_active(spool_directory)
        }
        Err(error) => Err(error),
    }
}

/// Drop the oldest segments until the directory holds at most [`MAX_SEGMENTS`].
///
/// This is the fail-open edge of the whole design: those frames may never have
/// been shipped. It is accounted for and announced, because a collector that
/// discards telemetry silently is indistinguishable from one that was never
/// switched on.
fn enforce_segment_budget(spool_directory: &str) {
    let generations = generations(spool_directory);
    if generations.len() <= MAX_SEGMENTS {
        return;
    }
    for generation in &generations[..generations.len() - MAX_SEGMENTS] {
        let path = format!("{spool_directory}/{}", segment_name(*generation));
        let bytes = fs::metadata(&path).map(|data| data.len()).unwrap_or(0);
        if fs::remove_file(&path).is_ok() {
            DROPPED_BYTES.fetch_add(bytes, Ordering::Relaxed);
            DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[chronos-ext] spool budget reached: dropped {} ({bytes} bytes) unshipped. \
                 The agent is not draining this spool.",
                segment_name(*generation)
            );
        }
    }
}

/// Append one document to the tenant's spool log.
///
/// `signal` is the name the document used to carry as a filename extension
/// (`trace`, `log`, `profile`, …) and is what ingest dispatches on; `id` is the
/// document's identity, which used to be the content-addressed filename and is
/// now what deduplication reads.
pub fn append(
    spool_directory: &str,
    signal: &str,
    encoding: &str,
    id: &str,
    payload: &[u8],
) -> std::io::Result<()> {
    let Some(encoded) = frame(signal, encoding, id, payload) else {
        DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
        DROPPED_BYTES.fetch_add(payload.len() as u64, Ordering::Relaxed);
        let error = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "document cannot be framed",
        );
        // Counted since ADR 0035 and, until now, never announced: an oversized
        // document was refused here and the signal simply never appeared.
        report_failure(spool_directory, &error);
        return Err(error);
    };
    fs::create_dir_all(spool_directory).map_err(|error| {
        report_failure(spool_directory, &error);
        error
    })?;
    let (generation, mut file) = open_active(spool_directory).map_err(|error| {
        report_failure(spool_directory, &error);
        error
    })?;
    let length = file.metadata().map(|data| data.len()).unwrap_or(0);
    if length >= SEGMENT_MAX_BYTES {
        let (_, rotated) = rotate(spool_directory, generation).map_err(|error| {
            report_failure(spool_directory, &error);
            error
        })?;
        file = rotated;
    }
    // ONE write, and its result inspected. `write_all` would loop on a short
    // write and split the frame across two appends, which is precisely the
    // interleaving this design exists to avoid.
    let written = file.write(&encoded).map_err(|error| {
        DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
        DROPPED_BYTES.fetch_add(encoded.len() as u64, Ordering::Relaxed);
        report_failure(spool_directory, &error);
        error
    })?;
    if written != encoded.len() {
        DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
        DROPPED_BYTES.fetch_add(encoded.len() as u64, Ordering::Relaxed);
        // Latched on the WriteZero slot like every other failure, NOT printed per
        // occurrence. A short write is exactly what a full filesystem produces —
        // write(2) returns a partial count while anything still fits and only
        // reports StorageFull once nothing does — and each segment rotation
        // re-arms it, so a disk hovering at capacity would have every FPM worker
        // streaming this line onto the disk that is already full. That is the
        // outage this module's own rule forbids: once per process, because a
        // warning on a hot path is its own outage.
        if !ANNOUNCED[error_slot(std::io::ErrorKind::WriteZero)].swap(true, Ordering::Relaxed) {
            eprintln!(
                "[chronos-ext] spool append was short ({written} of {} bytes) at \
                 {spool_directory}: frame dropped. {} frames ({} bytes) lost so far. \
                 Reported once per process, and every PHP worker is its own process.",
                encoded.len(),
                dropped(),
                dropped_bytes(),
            );
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "short spool append",
        ));
    }
    // No sync. See the module documentation and ADR 0035 §1.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("chronos-spool-log-{name}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        path.to_string_lossy().into_owned()
    }

    /// The same vector `engine-core`'s `spool_frame` tests assert. If these two
    /// ever disagree the reader stops reading what the writer writes, so the
    /// bytes are pinned on both sides rather than trusted to one.
    #[test]
    fn the_golden_vector_matches_the_readers() {
        let encoded = frame("trace", "json", "abc123", b"{}").expect("frame");
        let rendered = encoded
            .iter()
            .fold(String::new(), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            });
        assert_eq!(
            rendered,
            "4348524e3200020000001980212a7b227369676e616c223a227472616365222c\
             22656e636f64696e67223a226a736f6e222c226964223a22616263313233227d7b7d"
        );
        assert_eq!(checksum(b"chronos"), 0x4AE2_E10F);
    }

    #[test]
    fn appends_land_in_one_segment_in_order() {
        let directory = temporary_directory("order");
        append(&directory, "trace", "json", "one", b"{\"a\":1}").expect("first");
        append(&directory, "log", "json", "two", b"{\"b\":2}").expect("second");
        assert_eq!(generations(&directory), vec![1]);
        let bytes = fs::read(format!("{directory}/{}", segment_name(1))).expect("segment");
        let first = frame("trace", "json", "one", b"{\"a\":1}").expect("frame");
        let second = frame("log", "json", "two", b"{\"b\":2}").expect("frame");
        assert_eq!(bytes, [first, second].concat());
    }

    #[test]
    fn the_segment_budget_drops_the_oldest_and_says_so() {
        let directory = temporary_directory("budget");
        for generation in 1..=MAX_SEGMENTS as u64 + 2 {
            let _ = create_segment(&directory, generation);
        }
        let before = dropped();
        enforce_segment_budget(&directory);
        let generations = generations(&directory);
        assert_eq!(generations.len(), MAX_SEGMENTS);
        assert_eq!(
            *generations.first().expect("oldest kept"),
            3,
            "the oldest generations are the ones dropped"
        );
        assert!(dropped() > before, "a drop must be counted");
    }

    #[test]
    fn rotation_is_won_by_exactly_one_writer() {
        let directory = temporary_directory("rotate");
        let _ = create_segment(&directory, 1);
        let (first, _) = rotate(&directory, 1).expect("first rotation");
        let (second, _) = rotate(&directory, 1).expect("racing rotation");
        assert_eq!(first, 2);
        assert_eq!(
            second, 2,
            "a writer that loses the race appends to the winner's segment"
        );
        assert_eq!(generations(&directory), vec![1, 2]);
    }

    #[test]
    fn an_oversized_document_is_refused_rather_than_framed() {
        let directory = temporary_directory("oversized");
        let payload = vec![b'x'; MAX_FRAME_PAYLOAD_BYTES + 1];
        let before = dropped();
        assert!(append(&directory, "trace", "json", "big", &payload).is_err());
        assert!(dropped() > before);
        assert!(generations(&directory).is_empty());
    }

    /// The claim the whole design rests on: N workers appending concurrently
    /// produce N intact frames and no interleaving. If a single `write` were
    /// ever split, the log would decode as garbage from the first torn frame on.
    #[test]
    fn concurrent_writers_never_interleave_a_frame() {
        let directory = temporary_directory("concurrent");
        let writers = 8;
        let per_writer = 40;
        let mut threads = Vec::new();
        for writer in 0..writers {
            let directory = directory.clone();
            threads.push(std::thread::spawn(move || {
                for sequence in 0..per_writer {
                    // Bodies of differing length on purpose: equal-sized writes
                    // could interleave and still decode by luck.
                    let payload = format!(
                        "{{\"writer\":{writer},\"sequence\":{sequence},\"pad\":\"{}\"}}",
                        "x".repeat(writer * 37 + sequence)
                    );
                    append(
                        &directory,
                        "trace",
                        "json",
                        &format!("{writer}-{sequence}"),
                        payload.as_bytes(),
                    )
                    .expect("append");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("writer thread");
        }

        // Read every segment back as a frame sequence, the way the agent does.
        let mut ids = std::collections::BTreeSet::new();
        for generation in generations(&directory) {
            let bytes =
                fs::read(format!("{directory}/{}", segment_name(generation))).expect("segment");
            let mut offset = 0;
            while offset < bytes.len() {
                let rest = &bytes[offset..];
                assert_eq!(&rest[..4], &FRAME_MAGIC, "frame boundary at byte {offset}");
                let header_len = usize::from(u16::from_le_bytes([rest[4], rest[5]]));
                let payload_len =
                    u32::from_le_bytes([rest[6], rest[7], rest[8], rest[9]]) as usize;
                let expected = u32::from_le_bytes([rest[10], rest[11], rest[12], rest[13]]);
                let total = 14 + header_len + payload_len;
                let body = &rest[14..total];
                assert_eq!(checksum(body), expected, "checksum at byte {offset}");
                let header: serde_json::Value =
                    serde_json::from_slice(&body[..header_len]).expect("header");
                ids.insert(header["id"].as_str().expect("id").to_owned());
                offset += total;
            }
        }
        assert_eq!(
            ids.len(),
            writers * per_writer,
            "every appended frame must be present exactly once"
        );
    }

    /// The failure that cost hours: a spool directory the worker cannot write.
    /// The write must still fail open (the error is returned, never panicked)
    /// AND be counted, which is what makes it findable in a container log.
    #[test]
    #[cfg(unix)]
    fn an_unwritable_spool_directory_fails_open_and_is_announced() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_directory("unwritable");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500))
            .expect("make it read-only");
        let slot = error_slot(std::io::ErrorKind::PermissionDenied);
        ANNOUNCED[slot].store(false, Ordering::Relaxed);
        let result = append(&directory, "trace", "json", "denied", b"{}");
        // Restored before asserting, so a failure here cannot leave an
        // undeletable directory behind for the next run.
        let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700));
        assert!(result.is_err(), "an unwritable spool must report the failure");
        assert!(
            ANNOUNCED[slot].load(Ordering::Relaxed),
            "the failure must have been announced once"
        );
    }

    #[test]
    fn an_error_kind_is_latched_separately_from_the_others() {
        // A PermissionDenied at boot must not silence a StorageFull an hour
        // later — that is the whole reason the latch is per kind.
        for slot in 0..ERROR_KINDS {
            ANNOUNCED[slot].store(false, Ordering::Relaxed);
        }
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        report_failure("/nowhere", &denied);
        report_failure("/nowhere", &denied);
        assert!(ANNOUNCED[error_slot(std::io::ErrorKind::PermissionDenied)].load(Ordering::Relaxed));
        assert!(
            !ANNOUNCED[error_slot(std::io::ErrorKind::StorageFull)].load(Ordering::Relaxed),
            "a different kind keeps its own unspent latch"
        );
    }

    #[test]
    fn a_write_to_a_retired_segment_reopens_rather_than_failing() {
        let directory = temporary_directory("retired");
        append(&directory, "trace", "json", "one", b"{}").expect("first");
        fs::remove_file(format!("{directory}/{}", segment_name(1))).expect("agent deletes it");
        append(&directory, "trace", "json", "two", b"{}").expect("after deletion");
        assert_eq!(generations(&directory), vec![1]);
    }
}
