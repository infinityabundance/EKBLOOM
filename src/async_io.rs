//! Genuine completion-driven asynchronous publication of `.ekb` generations,
//! using Linux `io_uring`.
//!
//! This module is compiled only under `#[cfg(feature = "io-uring")]`. It is the
//! *real* asynchronous backend: it creates an `io_uring` ring, submits write
//! SQEs whose buffers are owned by this module and kept alive until their
//! matching completion events are reaped, waits on those completions, and
//! propagates each operation's error precisely. The synchronous writer
//! ([`crate::writer`]) remains the fallback used by [`crate::asyncpub`] whenever
//! a ring cannot be created or an operation fails.
//!
//! # Ring lifecycle
//!
//! A fresh ring is created per publication with [`IoUring::new`]
//! (`io_uring_setup(2)`). Ring creation is the runtime capability boundary: on a
//! kernel without `io_uring`, or under a container seccomp policy that denies
//! `io_uring_setup`, this returns `EPERM`/`ENOSYS` and the caller falls back.
//! [`probe`] creates a ring **and** completes a real write to a scratch file, so
//! it is a stronger check than mere ring creation.
//!
//! # Owned buffers and completion handling
//!
//! The serialized byte image is built as an owned `Vec<u8>` in the submitting
//! scope. SQEs reference it by raw pointer; the image outlives every SQE because
//! [`write_all`] waits for the completions of a submitted batch before dropping
//! the borrow and moving on. Every completion is matched back to its SQE via
//! `user_data` (the payload-relative offset), and a negative CQE result is turned
//! into the exact `errno` as [`std::io::Error`]; a short write is an error too.
//!
//! # Durability ordering
//!
//! The contract from `docs/DESIGN.md` §5 is preserved exactly:
//!
//! ```text
//! write payload (core + sections)  ->  fsync data  ->  write MANIFEST
//!   ->  append CommitFooter  ->  fsync  ->  advertise (rename / return)
//! ```
//!
//! [`publish`] writes the whole fresh image to a private `*.tmp` sibling, `fsync`s
//! it, and only then `rename`s it over the destination (so a reader never sees a
//! partial shell). [`append`] truncates any uncommitted tail, `pwrite`s a new,
//! fully-formed commit block (sections then `MANIFEST` then `CommitFooter`) with
//! explicit offsets, `fsync`s, and only then returns the new generation. Readers
//! validate footers, so a torn final block never invalidates an earlier commit.
//!
//! # Byte-identity with the synchronous writer
//!
//! The wire image is serialized here with the same public `format` primitives and
//! the same ordering the synchronous writer uses; the mirror functions
//! ([`all_sections`], [`build_commit`], [`new_sections`], ...) reproduce
//! [`crate::writer`]'s layout for the frozen format. `tests/async_io.rs` asserts
//! the async output is byte-for-byte identical to the synchronous output for both
//! `publish` and `append`.
//!
//! # Safety
//!
//! This is, besides `crate::simd`, the only module permitted to contain `unsafe`.
//! The unsafety is confined to the raw-buffer pointers handed to
//! `SubmissionQueue::push`, each guarded by a `// SAFETY:` justification stating
//! that the buffer outlives the submission and is only read by the kernel until
//! completion.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use io_uring::{opcode, types, IoUring};

use crate::bloom::BloomKind;
use crate::error::{EkbloomError, Result};
use crate::format::{
    self, align8, CommitFooter, GenesisHeader, ManifestComponent, ManifestRecord, ScannedCommit,
    SectionHeader, SectionKind, CORE_OFFSET, SECTION_HEADER_LEN,
};
use crate::manifest::{inspect, EkbloomBody};

/// Submission-queue entries requested when creating a ring.
pub const RING_ENTRIES: u32 = 128;

/// Maximum bytes carried by a single write SQE. `opcode::Write::len` is a `u32`,
/// so larger images are split across several SQEs at these offsets.
const WRITE_CHUNK: usize = 1 << 20;

/// `user_data` tag for the standalone `Fsync` SQE.
const FSYNC_TAG: u64 = u64::MAX;

/// Prove that `io_uring` actually works end-to-end in this environment.
///
/// Creates a ring, submits a real write plus an `fsync` to a private scratch file
/// through it, waits for both completions, and checks them. Returns `Ok(())` only
/// when the ring was created **and** the probe write completed successfully;
/// otherwise it returns the exact underlying [`std::io::Error`] (e.g. `EPERM`
/// from a seccomp policy denying `io_uring_setup`).
pub fn probe() -> io::Result<()> {
    let path = probe_path();
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)?;
    let result = (|| -> Result<()> {
        let mut ring = IoUring::new(RING_ENTRIES)?;
        let payload = b"ekbloom io_uring probe payload\n";
        write_all(&mut ring, file.as_raw_fd(), 0, payload)?;
        fsync_fd(&mut ring, file.as_raw_fd())?;
        Ok(())
    })();
    drop(file);
    let _ = std::fs::remove_file(&path);
    result.map_err(to_io_error)
}

/// Publish a complete body as a fresh committed file, using completion-driven
/// `io_uring` writes.
///
/// Writes the exact synchronous-writer image to a private `*.tmp` sibling,
/// `fsync`s it through the ring, and atomically renames it over `path`. On any
/// failure the temp file is removed and the error is returned, so no partial
/// shell is left behind and the caller may fall back to the synchronous writer.
pub fn publish(path: &Path, body: &EkbloomBody) -> Result<()> {
    let image = serialize_fresh(body)?;
    let tmp = tmp_path(path);
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        let mut ring = IoUring::new(RING_ENTRIES)?;
        write_all(&mut ring, file.as_raw_fd(), 0, &image)?;
        fsync_fd(&mut ring, file.as_raw_fd())?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Advertise only after the data is durable: rename + directory fsync.
    std::fs::rename(&tmp, path)?;
    fsync_dir(path)?;
    Ok(())
}

/// Append a new committed generation to `path` using completion-driven
/// `io_uring` writes, returning the new generation number.
///
/// Mirrors [`crate::writer::append_generation`]: any uncommitted tail is truncated
/// back to the newest valid commit, then only the new commit block is written (an
/// `io_uring` `pwrite` at the clean end) and `fsync`ed. If a failure occurs after
/// the tail truncation, the previous valid commit is untouched and the caller's
/// synchronous fallback re-appends cleanly.
pub fn append(path: &Path, body: &EkbloomBody) -> Result<u64> {
    let bytes = std::fs::read(path)?;
    let scan = format::scan(&bytes)?;
    let last = scan
        .commits
        .last()
        .ok_or_else(|| EkbloomError::RecoveryFailed {
            reason: "no valid committed generation to append to".to_string(),
        })?
        .clone();

    check_same_object(&scan.header, &bytes, &last, body)?;

    if body.generation <= last.generation {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "appended generation {} must exceed committed generation {}",
                body.generation, last.generation
            ),
        });
    }

    let counts = accumulated_counts(&scan.commits);
    let sections = new_sections(body, &counts)?;

    let clean_len = last.end;
    let base = align8(clean_len);
    let commit = build_commit(base, body.generation, last.generation, &sections)?;

    // Leading alignment padding (0..7 bytes) followed by the commit block, to be
    // written starting at the clean end.
    let mut image = Vec::with_capacity((base - clean_len) as usize + commit.len());
    image.resize((base - clean_len) as usize, 0);
    image.extend_from_slice(&commit);

    // Create the ring first: on failure the file is still untouched, so the
    // synchronous fallback can append to the intact file.
    let mut ring = IoUring::new(RING_ENTRIES)?;
    let file = OpenOptions::new().write(true).open(path)?;
    if bytes.len() as u64 > clean_len {
        file.set_len(clean_len)?;
    }
    write_all(&mut ring, file.as_raw_fd(), clean_len, &image)?;
    fsync_fd(&mut ring, file.as_raw_fd())?;
    drop(file);
    Ok(body.generation)
}

/// Submit a write of `data` starting at absolute file offset `base_offset` and
/// wait for every completion, propagating per-operation errors.
///
/// SQEs reference `data` by raw pointer; `data` is borrowed for the whole call
/// and every submitted batch is fully reaped before this function returns, so no
/// buffer is ever freed while the kernel may still be reading it.
fn write_all(ring: &mut IoUring, fd: RawFd, base_offset: u64, data: &[u8]) -> Result<()> {
    let total = data.len() as u64;
    let mut done: u64 = 0;
    while done < total {
        // (payload-relative offset => expected byte count) for this batch.
        let mut planned: Vec<(u64, u64)> = Vec::new();
        {
            let mut sq = ring.submission();
            let capacity = sq.capacity();
            let mut idx = done;
            while idx < total && planned.len() < capacity {
                let len = std::cmp::min(WRITE_CHUNK as u64, total - idx);
                let ptr = data[idx as usize..].as_ptr();
                let entry = opcode::Write::new(types::Fd(fd), ptr, len as u32)
                    .offset(base_offset + idx)
                    .build()
                    .user_data(idx);
                // SAFETY: `ptr` points into `data`, which is borrowed for the whole
                // call and therefore outlives every SQE here: this function waits
                // for each submitted batch's completions before returning or
                // dropping the borrow. The kernel only reads the buffer until the
                // completion event for this SQE is delivered.
                match unsafe { sq.push(&entry) } {
                    Ok(()) => {
                        planned.push((idx, len));
                        idx += len;
                    }
                    Err(_) => break,
                }
            }
        }
        if planned.is_empty() {
            return Err(short_write("io_uring submission queue made no progress"));
        }

        ring.submitter().submit_and_wait(planned.len())?;

        let mut reaped = 0usize;
        {
            let mut cq = ring.completion();
            for cqe in &mut cq {
                let user_data = cqe.user_data();
                let res = cqe.result();
                reaped += 1;
                let expected = planned
                    .iter()
                    .find(|(u, _)| *u == user_data)
                    .map(|(_, l)| *l)
                    .ok_or_else(|| {
                        EkbloomError::Io(io::Error::other(
                            "io_uring completion with unknown user_data",
                        ))
                    })?;
                if res < 0 {
                    return Err(EkbloomError::Io(io::Error::from_raw_os_error(-res)));
                }
                if res as u64 != expected {
                    return Err(short_write(&format!(
                        "io_uring write short: {res} of {expected} bytes"
                    )));
                }
            }
        }
        if reaped == 0 {
            return Err(short_write("io_uring reported no completions"));
        }
        done += planned.iter().map(|(_, l)| *l).sum::<u64>();
    }
    Ok(())
}

/// Submit an `fsync` SQE on `fd` and wait for its completion.
fn fsync_fd(ring: &mut IoUring, fd: RawFd) -> Result<()> {
    let entry = opcode::Fsync::new(types::Fd(fd))
        .build()
        .user_data(FSYNC_TAG);
    {
        let mut sq = ring.submission();
        // SAFETY: `Fsync` carries no buffer; it references only `fd`, which the
        // caller keeps open until this function has returned.
        unsafe { sq.push(&entry) }
            .map_err(|_| short_write("io_uring fsync submission queue full"))?;
    }
    ring.submitter().submit_and_wait(1)?;

    let mut reaped = false;
    let mut cq = ring.completion();
    for cqe in &mut cq {
        let res = cqe.result();
        if res < 0 {
            return Err(EkbloomError::Io(io::Error::from_raw_os_error(-res)));
        }
        reaped = true;
    }
    if !reaped {
        return Err(short_write("io_uring fsync reported no completion"));
    }
    Ok(())
}

/// The exact bytes [`crate::writer::write_body`] would place in a fresh file.
fn serialize_fresh(body: &EkbloomBody) -> Result<Vec<u8>> {
    let header = GenesisHeader {
        format_major: format::FORMAT_MAJOR,
        format_minor: format::FORMAT_MINOR,
        feature_flags: 0,
        object_id: body.object_id,
        logical_symbols: body.logical_len,
        logical_bytes: body.logical_len,
        core_len: body.core.len() as u64,
        core_machine_id: body.machine_id,
    };
    let header_bytes = header.encode();

    let sections = all_sections(body);
    let core_end = CORE_OFFSET + body.core.len() as u64;
    let base = align8(core_end);
    let commit = build_commit(base, body.generation, 0, &sections)?;

    let mut image = Vec::with_capacity(base as usize + commit.len());
    image.extend_from_slice(&header_bytes);
    image.extend_from_slice(&body.core);
    image.resize(base as usize, 0);
    image.extend_from_slice(&commit);
    Ok(image)
}

/// Serialize `sections` as an aligned commit block beginning at absolute offset
/// `base` (which must be 8-aligned), closing with a `CommitFooter`.
///
/// Mirrors `writer::build_commit` with no authentication section.
fn build_commit(
    base: u64,
    generation: u64,
    prev_generation: u64,
    sections: &[(SectionKind, Vec<u8>)],
) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut off = base;
    let mut manifest: Option<(u64, u64, [u8; 16])> = None;

    for (kind, payload) in sections {
        let pad = align8(off) - off;
        if pad > 0 {
            out.resize(out.len() + pad as usize, 0);
        }
        off = align8(off);
        let header_offset = off;
        let payload_offset = header_offset + SECTION_HEADER_LEN as u64;
        let checksum = crc32fast::hash(payload);
        let header = SectionHeader {
            kind: *kind,
            flags: 0,
            offset: payload_offset,
            length: payload.len() as u64,
            generation,
            checksum,
        };
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(payload);
        off = payload_offset + payload.len() as u64;

        if *kind == SectionKind::Manifest {
            let digest = *blake3::hash(payload).as_bytes();
            let mut hash = [0u8; 16];
            hash.copy_from_slice(&digest[..16]);
            manifest = Some((header_offset, payload.len() as u64, hash));
        }
    }

    let pad = align8(off) - off;
    if pad > 0 {
        out.resize(out.len() + pad as usize, 0);
    }

    let (manifest_offset, manifest_len, manifest_hash) =
        manifest.ok_or_else(|| EkbloomError::BadSection {
            reason: "commit must include a manifest section".to_string(),
        })?;
    let footer = CommitFooter {
        generation,
        manifest_offset,
        manifest_len,
        prev_generation,
        manifest_hash,
    };
    out.extend_from_slice(&footer.encode());
    Ok(out)
}

/// Every section of `body`, in publication order (manifest last).
fn all_sections(body: &EkbloomBody) -> Vec<(SectionKind, Vec<u8>)> {
    let mut out = Vec::new();
    if let Some(history) = &body.history {
        out.push((SectionKind::HistoryFull, history.to_bytes()));
    }
    for cp in &body.checkpoints {
        out.push((SectionKind::Checkpoints, cp.to_bytes()));
    }
    for shell in &body.blooms {
        out.push((section_kind_for_bloom(shell.kind), shell.to_bytes()));
    }
    for cert in &body.certificates {
        out.push((SectionKind::Certificate, cert.to_bytes()));
    }
    out.push((SectionKind::Manifest, manifest_record(body).encode()));
    out
}

/// Already-committed structure counts accumulated across a valid commit prefix.
#[derive(Clone, Copy, Debug, Default)]
struct CommittedCounts {
    history: bool,
    checkpoints: usize,
    blooms: usize,
    certificates: usize,
}

/// Count the accumulated structures of the valid commit prefix.
fn accumulated_counts(commits: &[format::ScannedCommit]) -> CommittedCounts {
    let mut counts = CommittedCounts::default();
    for commit in commits {
        for section in &commit.sections {
            match section.kind {
                SectionKind::HistoryFull => counts.history = true,
                SectionKind::Checkpoints => counts.checkpoints += 1,
                SectionKind::BloomGlobal
                | SectionKind::BloomContext
                | SectionKind::BloomRegion
                | SectionKind::BloomPath => counts.blooms += 1,
                SectionKind::Certificate => counts.certificates += 1,
                SectionKind::CoreMachine
                | SectionKind::HistorySegmented
                | SectionKind::Manifest
                | SectionKind::Auth => {}
            }
        }
    }
    counts
}

/// The sections to append for `body` given what is already committed.
fn new_sections(
    body: &EkbloomBody,
    counts: &CommittedCounts,
) -> Result<Vec<(SectionKind, Vec<u8>)>> {
    let mut out = Vec::new();

    match (&body.history, counts.history) {
        (Some(history), false) => out.push((SectionKind::HistoryFull, history.to_bytes())),
        (Some(_), true) => {}
        (None, true) => {
            return Err(EkbloomError::InvalidArgument {
                what: "body dropped a committed history".to_string(),
            })
        }
        (None, false) => {}
    }

    if body.checkpoints.len() < counts.checkpoints
        || body.blooms.len() < counts.blooms
        || body.certificates.len() < counts.certificates
    {
        return Err(EkbloomError::InvalidArgument {
            what: "body withdraws committed structures".to_string(),
        });
    }
    for cp in &body.checkpoints[counts.checkpoints..] {
        out.push((SectionKind::Checkpoints, cp.to_bytes()));
    }
    for shell in &body.blooms[counts.blooms..] {
        out.push((section_kind_for_bloom(shell.kind), shell.to_bytes()));
    }
    for cert in &body.certificates[counts.certificates..] {
        out.push((SectionKind::Certificate, cert.to_bytes()));
    }

    out.push((SectionKind::Manifest, manifest_record(body).encode()));
    Ok(out)
}

/// Map a bloom construction to its section kind.
fn section_kind_for_bloom(kind: BloomKind) -> SectionKind {
    match kind {
        BloomKind::Universal => SectionKind::BloomGlobal,
        BloomKind::Contextual => SectionKind::BloomContext,
        BloomKind::Regional => SectionKind::BloomRegion,
        BloomKind::Path => SectionKind::BloomPath,
    }
}

/// Build the self-describing manifest record for `body`.
fn manifest_record(body: &EkbloomBody) -> ManifestRecord {
    let report = inspect(body);
    ManifestRecord {
        generation: body.generation,
        logical_len: body.logical_len,
        object_id: body.object_id,
        machine_id: body.machine_id,
        core_hash: body.core_hash,
        profiles: report.profiles,
        components: report
            .components
            .into_iter()
            .map(|c| ManifestComponent {
                name: c.name,
                kind: c.kind,
                bytes: c.bytes,
            })
            .collect(),
    }
}

/// Verify that `body` describes the same object as the file's newest commit.
fn check_same_object(
    header: &GenesisHeader,
    bytes: &[u8],
    last: &ScannedCommit,
    body: &EkbloomBody,
) -> Result<()> {
    let manifest = format::commit_manifest(bytes, last)?;
    let same = header.core_machine_id == body.machine_id
        && header.logical_symbols == body.logical_len
        && header.object_id == body.object_id
        && header.core_len == body.core.len() as u64
        && manifest.core_hash == body.core_hash
        && *blake3::hash(&body.core).as_bytes() == body.core_hash;
    if !same {
        return Err(EkbloomError::InvalidArgument {
            what: "appended body does not match the committed object".to_string(),
        });
    }
    Ok(())
}

/// A sibling temporary path used for atomic replacement.
fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

/// `fsync` the directory containing `path`, so a rename is durable.
fn fsync_dir(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let file = std::fs::File::open(dir)?;
    file.sync_all()?;
    Ok(())
}

/// A unique scratch path for the capability probe.
fn probe_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "ekbloom-uring-probe-{}-{}",
        std::process::id(),
        nanos
    ));
    path
}

/// Construct a short-write [`EkbloomError`].
fn short_write(reason: &str) -> EkbloomError {
    EkbloomError::Io(io::Error::new(io::ErrorKind::WriteZero, reason.to_string()))
}

/// Map an [`EkbloomError`] to the most precise [`std::io::Error`] available.
fn to_io_error(e: EkbloomError) -> io::Error {
    match e {
        EkbloomError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}
