//! In-memory logical reads across every manifested access path.
//!
//! [`read`] resolves a logical range to authoritative bytes. It selects the
//! fastest available *correct* path, in priority order:
//!
//! 1. a certified **path bloom** covering the range (exact by construction),
//! 2. the **nearest checkpoint restart** (bounded replay from a stored ordinal),
//! 3. the full **history** (O(1) state per position),
//! 4. the authoritative **machine from `Omega_0`**.
//!
//! Every path returns identical bytes; the module tests assert this agreement
//! across all four paths.
//!
//! [`FileBody`] additionally provides a fully-safe, file-backed reader that keeps
//! only parsed offsets in memory and reads exactly the byte ranges a request
//! needs. No memory mapping and no `unsafe` are used anywhere in this crate.

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certificate, Certification};
use crate::bloom_path::PathBloom;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::format::{self, SectionKind};
use crate::history::History;
use crate::machine::{machine_by_id, MACHINE_NL2};
use crate::manifest::{ComponentReport, EkbloomBody, ManifestationReport};
use crate::state::Snapshot;
use crate::ReadRequest;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Read the logical bytes `req.offset .. req.offset + req.length`.
///
/// Bounds-checked: an offset/length that overflows yields
/// [`EkbloomError::Overflow`], and a range beyond `logical_len` yields
/// [`EkbloomError::OutOfBounds`]. A zero-length read always succeeds, including
/// at the very end of the object.
pub fn read(body: &EkbloomBody, req: ReadRequest) -> Result<Vec<u8>> {
    let end = req
        .offset
        .checked_add(req.length)
        .ok_or(EkbloomError::Overflow { what: "read range" })?;
    if end > body.logical_len {
        return Err(EkbloomError::OutOfBounds {
            offset: req.offset,
            length: req.length,
            logical_len: body.logical_len,
        });
    }
    if req.length == 0 {
        return Ok(Vec::new());
    }

    if let Some(bytes) = read_via_path_bloom(body, req.offset, end)? {
        return Ok(bytes);
    }
    if let Some(bytes) = read_via_checkpoint(body, req.offset, req.length)? {
        return Ok(bytes);
    }
    if let Some(bytes) = read_via_history(body, req.offset, end)? {
        return Ok(bytes);
    }
    read_via_machine(body, req.offset, end)
}

/// Open a `.ekb` file, validate it, and return the newest valid committed body.
///
/// The writer's publication contract makes every committed generation
/// self-validating: this parses the header, cross-checks the core hash against
/// the `MANIFEST`, decodes every accumulated section for the newest *valid*
/// committed generation, and reconstructs an [`EkbloomBody`] that [`crate::verify`]
/// accepts. A corrupt or torn newest generation is not used; the reader falls
/// back to the newest earlier valid commit. Corruption that leaves no valid
/// commit is rejected (never silently ignored).
pub fn open(path: &Path) -> Result<EkbloomBody> {
    let bytes = std::fs::read(path)?;
    open_bytes(&bytes)
}

/// Open and validate a body from an in-memory `.ekb` image.
pub fn open_bytes(bytes: &[u8]) -> Result<EkbloomBody> {
    let scan = format::scan(bytes)?;
    let selected = match scan.commits.last().cloned() {
        Some(commit) => commit,
        None => {
            return Err(scan
                .torn_error
                .unwrap_or_else(|| EkbloomError::RecoveryFailed {
                    reason: "no valid committed generation".to_string(),
                }));
        }
    };
    let upto = scan.commits.len() - 1;
    open_selected(bytes, &scan, selected, upto)
}

/// Open a `.ekb` file authenticated with `key`.
///
/// The file must carry an `AUTH` section (written by
/// [`crate::writer::write_body_authenticated`] or
/// [`crate::writer::append_generation_authenticated`]). The newest structurally
/// valid committed generation whose MAC verifies is selected; a generation with
/// a missing or mismatching `AUTH` is treated as invalid and skipped, exactly as
/// recovery skips a torn generation. If no generation authenticates, this fails.
pub fn open_authenticated(path: &Path, key: &[u8; 32]) -> Result<EkbloomBody> {
    let bytes = std::fs::read(path)?;
    open_bytes_authenticated(&bytes, key)
}

/// [`open_authenticated`] over an in-memory `.ekb` image.
pub fn open_bytes_authenticated(bytes: &[u8], key: &[u8; 32]) -> Result<EkbloomBody> {
    let scan = format::scan(bytes)?;
    if scan.commits.is_empty() {
        return Err(scan
            .torn_error
            .unwrap_or_else(|| EkbloomError::RecoveryFailed {
                reason: "no authenticated committed generation".to_string(),
            }));
    }
    let mut newest_error: Option<EkbloomError> = None;
    for (idx, commit) in scan.commits.iter().enumerate().rev() {
        match verify_commit_auth(bytes, commit, key) {
            Ok(()) => return open_selected(bytes, &scan, commit.clone(), idx),
            Err(e) => {
                if newest_error.is_none() {
                    newest_error = Some(e);
                }
            }
        }
    }
    Err(newest_error.unwrap_or(EkbloomError::ChecksumMismatch { section: "auth" }))
}

/// Recompute and verify the keyed MAC of one scanned commit.
///
/// The commit must contain exactly one `AUTH` section, as its final section, whose
/// 32-byte payload equals `blake3::keyed_hash(key, blake3(bytes[0 .. auth_offset]))`
/// — i.e. the keyed MAC over the exact preceding bytes of the file.
fn verify_commit_auth(bytes: &[u8], commit: &format::ScannedCommit, key: &[u8; 32]) -> Result<()> {
    let auths: Vec<&format::ScannedSection> = commit
        .sections
        .iter()
        .filter(|s| s.kind == SectionKind::Auth)
        .collect();
    if auths.is_empty() {
        return Err(EkbloomError::CorruptGeneration {
            generation: commit.generation,
            reason: "authenticated open requires an AUTH section".to_string(),
        });
    }
    if auths.len() != 1 || commit.sections.last().map(|s| s.kind) != Some(SectionKind::Auth) {
        return Err(corrupt(
            commit.generation,
            "malformed AUTH section placement",
        ));
    }
    let auth = auths[0];
    let covered_end = usize::try_from(auth.header_offset).map_err(|_| EkbloomError::Overflow {
        what: "auth covered range",
    })?;
    let covered = bytes.get(..covered_end).ok_or(EkbloomError::Truncated {
        needed: auth.header_offset,
        available: bytes.len() as u64,
    })?;
    let expected = format::auth_mac(key, covered);
    let payload = format::section_payload(bytes, auth);
    if payload != &expected[..] {
        return Err(EkbloomError::ChecksumMismatch { section: "auth" });
    }
    Ok(())
}

/// Reconstruct a validated [`EkbloomBody`] from the commit at index `upto`,
/// accumulating structures from `scan.commits[..=upto]` only.
fn open_selected(
    bytes: &[u8],
    scan: &format::ScanOutcome,
    selected: format::ScannedCommit,
    upto: usize,
) -> Result<EkbloomBody> {
    let mut history: Option<History> = None;
    let mut checkpoints: Vec<CheckpointSet> = Vec::new();
    let mut blooms: Vec<BloomShell> = Vec::new();
    let mut certificates: Vec<Certificate> = Vec::new();
    let mut segmented_count = 0usize;
    let mut segmented_link: Option<([u8; 32], u64)> = None;

    for commit in &scan.commits[..=upto] {
        for section in &commit.sections {
            let payload = format::section_payload(bytes, section);
            match section.kind {
                SectionKind::HistoryFull => history = Some(History::from_bytes(payload)?),
                SectionKind::Checkpoints => {
                    let machine = machine_by_id(scan.header.core_machine_id).ok_or(
                        EkbloomError::UnknownMachine {
                            id: scan.header.core_machine_id,
                        },
                    )?;
                    checkpoints.push(CheckpointSet::from_bytes_with(machine, payload)?);
                }
                SectionKind::BloomGlobal
                | SectionKind::BloomContext
                | SectionKind::BloomRegion
                | SectionKind::BloomPath => {
                    let shell = BloomShell::from_bytes(payload)?;
                    if section_kind_of(shell.kind) != section.kind {
                        return Err(corrupt(
                            commit.generation,
                            "bloom kind disagrees with its section",
                        ));
                    }
                    blooms.push(shell);
                }
                SectionKind::Certificate => certificates.push(Certificate::from_bytes(payload)?),
                SectionKind::HistorySegmented => {
                    // The in-memory `EkbloomBody` has no field for a segmented
                    // projection, so the section is validated for linkage and
                    // accounted for here, while `FileBody` is the streaming
                    // access path for its states.
                    let h = decode_segmented_header(payload)?;
                    segmented_count += 1;
                    segmented_link = Some((h.core_hash, h.logical_len));
                }
                SectionKind::Manifest | SectionKind::Auth => {}
                SectionKind::CoreMachine => {
                    return Err(corrupt(commit.generation, "unexpected core section"))
                }
            }
        }
    }

    let manifest = format::commit_manifest(bytes, &selected)?;
    let core = bytes[scan.core_range.clone()].to_vec();

    if manifest.generation != selected.generation {
        return Err(corrupt(selected.generation, "manifest generation mismatch"));
    }
    if scan.header.core_machine_id == crate::machine::MACHINE_NL2
        && scan.header.logical_bytes != scan.header.logical_symbols
    {
        return Err(corrupt(
            selected.generation,
            "header logical_bytes disagrees with logical_symbols",
        ));
    }
    if manifest.logical_len != scan.header.logical_symbols {
        return Err(corrupt(
            selected.generation,
            "logical length disagrees with the header",
        ));
    }
    if manifest.object_id != scan.header.object_id {
        return Err(corrupt(
            selected.generation,
            "object id disagrees with the header",
        ));
    }
    if manifest.machine_id != scan.header.core_machine_id {
        return Err(corrupt(
            selected.generation,
            "machine id disagrees with the header",
        ));
    }
    // The machine consumes exactly one core byte per logical symbol, so a
    // declared logical length greater than the visible core is unsatisfiable.
    // Rejecting it here (rather than later, per read) also removes an
    // untrusted-input allocation vector: a crafted header could otherwise
    // declare a huge `logical_len` over a tiny core, and a forged-but-checksum-
    // valid checkpoint would make `read` size a buffer from that declaration.
    if scan.header.core_machine_id == crate::machine::MACHINE_NL2
        && manifest.logical_len > core.len() as u64
    {
        return Err(corrupt(
            selected.generation,
            "logical length exceeds core length",
        ));
    }
    // One digest over the authoritative core, cross-checked directly against
    // every structure that carries a `core_hash`: the manifest, the history, and
    // every bloom shell (and their certificates). Comparing each structure to the
    // freshly recomputed digest — rather than only transitively via the manifest
    // — makes any structure that disagrees with the core a rejection at `open`.
    let core_digest = *blake3::hash(&core).as_bytes();
    if core_digest != manifest.core_hash {
        return Err(corrupt(selected.generation, "core hash mismatch"));
    }
    if let Some(h) = &history {
        if h.core_hash != core_digest || h.logical_len != manifest.logical_len {
            return Err(corrupt(
                selected.generation,
                "history core linkage mismatch",
            ));
        }
    }
    for shell in &blooms {
        if shell.core_hash != core_digest {
            return Err(corrupt(selected.generation, "bloom core linkage mismatch"));
        }
    }
    for cert in &certificates {
        if cert.core_hash != core_digest {
            return Err(corrupt(
                selected.generation,
                "certificate core linkage mismatch",
            ));
        }
    }
    if let Some((core_hash, logical_len)) = segmented_link {
        if core_hash != core_digest || logical_len != manifest.logical_len {
            return Err(corrupt(
                selected.generation,
                "history-segmented core linkage mismatch",
            ));
        }
    }
    let expected_components = 1
        + usize::from(history.is_some())
        + checkpoints.len()
        + blooms.len()
        + certificates.len()
        + segmented_count;
    if manifest.components.len() != expected_components {
        return Err(corrupt(
            selected.generation,
            "component count disagrees with the manifest",
        ));
    }

    Ok(EkbloomBody {
        object_id: manifest.object_id,
        core,
        logical_len: manifest.logical_len,
        machine_id: manifest.machine_id,
        core_hash: manifest.core_hash,
        history,
        checkpoints,
        blooms,
        certificates,
        generation: selected.generation,
        profiles: manifest.profiles,
    })
}

/// Convenience: [`open`] then [`read`].
pub fn read_file(path: &Path, req: ReadRequest) -> Result<Vec<u8>> {
    let body = open(path)?;
    read(&body, req)
}

/// Convenience: [`open`] then [`crate::manifest::inspect`].
pub fn inspect_file(path: &Path) -> Result<ManifestationReport> {
    let body = open(path)?;
    Ok(crate::manifest::inspect(&body))
}

/// Map a bloom construction to its on-disk section kind.
fn section_kind_of(kind: BloomKind) -> SectionKind {
    match kind {
        BloomKind::Universal => SectionKind::BloomGlobal,
        BloomKind::Contextual => SectionKind::BloomContext,
        BloomKind::Regional => SectionKind::BloomRegion,
        BloomKind::Path => SectionKind::BloomPath,
    }
}

/// Construct a [`EkbloomError::CorruptGeneration`].
fn corrupt(generation: u64, reason: &str) -> EkbloomError {
    EkbloomError::CorruptGeneration {
        generation,
        reason: reason.to_string(),
    }
}

/// Path 1: a certified path bloom whose extent covers the request.
fn read_via_path_bloom(body: &EkbloomBody, offset: u64, end: u64) -> Result<Option<Vec<u8>>> {
    for shell in &body.blooms {
        if shell.kind != BloomKind::Path
            || shell.status != Certification::Certified
            || shell.logical_len != body.logical_len
        {
            continue;
        }
        let bloom = match PathBloom::from_shell(shell) {
            Ok(bloom) => bloom,
            Err(_) => continue,
        };
        if bloom.logical_len < end {
            continue;
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            out.push(bloom.symbol_at(pos)?);
            pos += 1;
        }
        return Ok(Some(out));
    }
    Ok(None)
}

/// Path 2: the checkpoint set with the fewest replay steps for `offset`.
fn read_via_checkpoint(body: &EkbloomBody, offset: u64, length: u64) -> Result<Option<Vec<u8>>> {
    let mut best: Option<(u64, &CheckpointSet)> = None;
    for cp in &body.checkpoints {
        if cp.interval == 0 || cp.restart_point(offset).is_none() {
            continue;
        }
        let steps = offset % u64::from(cp.interval);
        let better = match best {
            Some((best_steps, _)) => steps < best_steps,
            None => true,
        };
        if better {
            best = Some((steps, cp));
        }
    }
    let Some((_, cp)) = best else {
        return Ok(None);
    };

    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut omega = cp.restore_with(machine, &body.core, offset)?;
    let mut out = Vec::with_capacity(length as usize);
    for _ in 0..length {
        out.push(machine.reveal(&body.core, &mut omega)?);
    }
    Ok(Some(out))
}

/// Path 3: the full history, one O(1) snapshot per position.
fn read_via_history(body: &EkbloomBody, offset: u64, end: u64) -> Result<Option<Vec<u8>>> {
    let Some(history) = &body.history else {
        return Ok(None);
    };
    if history.logical_len != body.logical_len {
        return Ok(None);
    }
    // The history state-only projection is directly resumable only for a
    // cursor==position machine; other machines fall through to replay.
    if body.machine_id != MACHINE_NL2 {
        return Ok(None);
    }
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut out = Vec::with_capacity((end - offset) as usize);
    let mut pos = offset;
    while pos < end {
        let Some(mut omega) = history.snapshot_at(pos) else {
            return Ok(None);
        };
        out.push(machine.reveal(&body.core, &mut omega)?);
        pos += 1;
    }
    Ok(Some(out))
}

/// Path 4: authoritative replay from `Omega_0`.
fn read_via_machine(body: &EkbloomBody, offset: u64, end: u64) -> Result<Vec<u8>> {
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    machine.check_core(&body.core, body.logical_len)?;
    let mut omega = machine.init(&body.core)?;
    let mut pos = 0u64;
    while pos < offset {
        machine.reveal(&body.core, &mut omega)?;
        pos += 1;
    }
    let mut out = Vec::with_capacity((end - offset) as usize);
    while pos < end {
        out.push(machine.reveal(&body.core, &mut omega)?);
        pos += 1;
    }
    Ok(out)
}

/// Fixed byte size of a `BloomShell` wire header (mirrors `bloom::SHELL_HEADER_LEN`).
const SHELL_HEADER_LEN: usize = 4 + 2 + 8 + 8 + 8 + 4 + 32 + 8;
/// Fixed byte size of a path-bloom payload prefix (`depth`, `logical_len`, `nblocks`).
const PATH_PAYLOAD_PREFIX_LEN: usize = 2 + 8 + 8;
/// Fixed byte size of a history wire header (mirrors `history::HEADER_LEN`).
const HISTORY_HEADER_LEN: usize = 8 + 8 + 8 + 32 + 8;
/// Fixed byte size of a checkpoint-set wire header (`interval`, `count`).
const CHECKPOINT_HEADER_LEN: usize = 4 + 8;
/// Bounded streaming chunk for open-time checksums/hashes (64 KiB).
const IO_CHUNK: usize = 64 * 1024;
/// Number of checkpoint resume points validated per bounded read.
const CHECKPOINT_VALIDATE_BATCH: u64 = 4096;

/// Construct a [`EkbloomError::BadSection`].
fn bad(reason: impl Into<String>) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.into(),
    }
}

/// Construct an [`EkbloomError::OutOfBounds`].
#[inline]
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

/// Read a little-endian `u64` at `off` from a bounds-checked header buffer.
fn le_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

/// Read exactly `buf.len()` bytes at file offset `off`, after checking the
/// extent against the known file length so failure is a clean [`EkbloomError`].
fn read_exact_at_len(file: &File, buf: &mut [u8], off: u64, total: u64) -> Result<()> {
    let end = off
        .checked_add(buf.len() as u64)
        .ok_or(EkbloomError::Overflow {
            what: "file read extent",
        })?;
    if end > total {
        return Err(EkbloomError::Truncated {
            needed: end,
            available: total,
        });
    }
    file.read_exact_at(buf, off)?;
    Ok(())
}

/// Read `len` bytes at `off` into a fresh buffer (bounds-checked).
fn read_vec_at(file: &File, off: u64, len: u64, total: u64) -> Result<Vec<u8>> {
    let n = usize::try_from(len).map_err(|_| EkbloomError::Overflow {
        what: "file read length",
    })?;
    let mut buf = vec![0u8; n];
    read_exact_at_len(file, &mut buf, off, total)?;
    Ok(buf)
}

/// Stream a CRC-32 over `[off, off + len)` in bounded chunks.
fn crc32_range(file: &File, off: u64, len: u64, total: u64) -> Result<u32> {
    let end = off.checked_add(len).ok_or(EkbloomError::Overflow {
        what: "checksum extent",
    })?;
    if end > total {
        return Err(EkbloomError::Truncated {
            needed: end,
            available: total,
        });
    }
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; IO_CHUNK];
    let mut at = off;
    let mut remaining = len;
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        file.read_exact_at(&mut buf[..n], at)?;
        hasher.update(&buf[..n]);
        at += n as u64;
        remaining -= n as u64;
    }
    Ok(hasher.finalize())
}

/// Stream a BLAKE3 digest over `[off, off + len)` in bounded chunks.
fn blake3_range(file: &File, off: u64, len: u64, total: u64) -> Result<[u8; 32]> {
    let end = off.checked_add(len).ok_or(EkbloomError::Overflow {
        what: "hash extent",
    })?;
    if end > total {
        return Err(EkbloomError::Truncated {
            needed: end,
            available: total,
        });
    }
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_CHUNK];
    let mut at = off;
    let mut remaining = len;
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        file.read_exact_at(&mut buf[..n], at)?;
        hasher.update(&buf[..n]);
        at += n as u64;
        remaining -= n as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// A bloom shell header plus the absolute file offset/length of its whole
/// payload (shell header included). The payload is never loaded at once.
#[derive(Clone, Copy, Debug)]
struct FileBloom {
    kind: BloomKind,
    depth: u16,
    logical_len: u64,
    entries: u64,
    status: Certification,
    core_hash: [u8; 32],
    payload_offset: u64,
    payload_len: u64,
}

/// A checkpoint-set header plus the absolute range of its payload. Only the one
/// resume point a request needs is ever read.
#[derive(Clone, Copy, Debug)]
struct FileCheckpoint {
    interval: u32,
    count: u64,
    payload_offset: u64,
}

/// A full-history header plus the absolute range of its payload. Only the `u32`
/// states a request needs are ever read.
#[derive(Clone, Copy, Debug)]
struct FileHistory {
    logical_len: u64,
    core_hash: [u8; 32],
    payload_offset: u64,
}

/// A parsed certificate reduced to the fields `inspect` and linkage need.
#[derive(Clone, Copy, Debug)]
struct FileCertificate {
    kind: BloomKind,
    core_hash: [u8; 32],
    bytes: u64,
}

/// The fixed header of a [`SectionKind::HistorySegmented`] payload.
#[derive(Clone, Copy, Debug)]
struct SegmentedHeader {
    segment_bytes: u32,
    checkpoint_interval: u32,
    logical_len: u64,
    index_offset: u64,
    states_offset: u64,
    checkpoints_offset: u64,
    checkpoints_len: u64,
    core_hash: [u8; 32],
}

/// Decode and structurally validate a [`SegmentedHeader`].
///
/// Every declared offset is consistency-checked against `segment_bytes` and
/// `logical_len`, so a malformed (yet checksum-valid) header is rejected rather
/// than trusted.
fn decode_segmented_header(bytes: &[u8]) -> Result<SegmentedHeader> {
    use crate::format::HS_FIXED_LEN;
    if bytes.len() < HS_FIXED_LEN {
        return Err(EkbloomError::Truncated {
            needed: HS_FIXED_LEN as u64,
            available: bytes.len() as u64,
        });
    }
    let segment_bytes = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let checkpoint_interval = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if segment_bytes == 0 || !segment_bytes.is_multiple_of(4) {
        return Err(bad(
            "history-segmented segment_bytes must be a non-zero multiple of 4",
        ));
    }
    if !CheckpointSet::supported_interval(checkpoint_interval) {
        return Err(EkbloomError::BadInterval {
            interval: checkpoint_interval,
        });
    }
    let logical_len = le_u64(bytes, 8);
    let state_count = le_u64(bytes, 16);
    let segment_count = le_u64(bytes, 24);
    let index_offset = le_u64(bytes, 32);
    let states_offset = le_u64(bytes, 40);
    let checkpoints_offset = le_u64(bytes, 48);
    let checkpoints_len = le_u64(bytes, 56);
    let mut core_hash = [0u8; 32];
    core_hash.copy_from_slice(&bytes[64..96]);

    let want_states = logical_len.checked_add(1).ok_or(EkbloomError::Overflow {
        what: "history-segmented state count",
    })?;
    if state_count != want_states {
        return Err(bad(format!(
            "history-segmented state count {state_count} inconsistent with logical_len {logical_len}"
        )));
    }
    let sps = u64::from(segment_bytes) / 4;
    let want_segments = state_count
        .checked_add(sps - 1)
        .ok_or(EkbloomError::Overflow {
            what: "history-segmented segment count",
        })?
        / sps;
    if segment_count != want_segments {
        return Err(bad(format!(
            "history-segmented segment count {segment_count} inconsistent with {want_segments}"
        )));
    }
    let fixed = HS_FIXED_LEN as u64;
    if index_offset != fixed {
        return Err(bad("history-segmented index offset mismatch"));
    }
    let want_states_offset = fixed
        .checked_add(segment_count.checked_mul(8).ok_or(EkbloomError::Overflow {
            what: "history-segmented index",
        })?)
        .ok_or(EkbloomError::Overflow {
            what: "history-segmented layout",
        })?;
    if states_offset != want_states_offset {
        return Err(bad("history-segmented states offset mismatch"));
    }
    let want_cp_offset = states_offset
        .checked_add(state_count.checked_mul(4).ok_or(EkbloomError::Overflow {
            what: "history-segmented states",
        })?)
        .ok_or(EkbloomError::Overflow {
            what: "history-segmented layout",
        })?;
    if checkpoints_offset != want_cp_offset {
        return Err(bad("history-segmented checkpoints offset mismatch"));
    }
    checkpoints_offset
        .checked_add(checkpoints_len)
        .ok_or(EkbloomError::Overflow {
            what: "history-segmented layout",
        })?;

    Ok(SegmentedHeader {
        segment_bytes,
        checkpoint_interval,
        logical_len,
        index_offset,
        states_offset,
        checkpoints_offset,
        checkpoints_len,
        core_hash,
    })
}

/// A parsed [`SectionKind::HistorySegmented`] section: the header plus the
/// absolute payload range. States are read by `pread`, never loaded wholesale.
#[derive(Clone, Copy, Debug)]
struct FileSegmentedHistory {
    header: SegmentedHeader,
    payload_offset: u64,
    payload_len: u64,
    physical_len: u64,
}

impl FileSegmentedHistory {
    /// Read `x_i` via one bounded index read plus one 4-byte state read.
    fn state_at(&self, file: &File, i: u64) -> Result<u32> {
        let h = &self.header;
        if i > h.logical_len {
            return Err(oob(i, 1, h.logical_len));
        }
        let sps = u64::from(h.segment_bytes) / 4;
        let seg = i / sps;
        let within = i % sps;
        let index_rel = h
            .index_offset
            .checked_add(seg.checked_mul(8).ok_or(EkbloomError::Overflow {
                what: "history-segmented index",
            })?)
            .ok_or(EkbloomError::Overflow {
                what: "history-segmented index",
            })?;
        let index_off =
            self.payload_offset
                .checked_add(index_rel)
                .ok_or(EkbloomError::Overflow {
                    what: "history-segmented index",
                })?;
        let mut entry = [0u8; 8];
        read_exact_at_len(file, &mut entry, index_off, self.physical_len)?;
        let seg_rel = le_u64(&entry, 0);
        let byte_rel = seg_rel
            .checked_add(within.checked_mul(4).ok_or(EkbloomError::Overflow {
                what: "history-segmented state offset",
            })?)
            .ok_or(EkbloomError::Overflow {
                what: "history-segmented state offset",
            })?;
        let end_rel = byte_rel.checked_add(4).ok_or(EkbloomError::Overflow {
            what: "history-segmented state offset",
        })?;
        if seg_rel < h.states_offset || end_rel > h.checkpoints_offset {
            return Err(bad("history-segmented index entry out of range"));
        }
        let byte_off = self
            .payload_offset
            .checked_add(byte_rel)
            .ok_or(EkbloomError::Overflow {
                what: "history-segmented state offset",
            })?;
        let mut buf = [0u8; 4];
        read_exact_at_len(file, &mut buf, byte_off, self.physical_len)?;
        Ok(u32::from_le_bytes(buf))
    }
}

/// Parse a [`SectionKind::HistorySegmented`] section header from the file.
fn parse_segmented(
    file: &File,
    payload_offset: u64,
    payload_len: u64,
    total: u64,
) -> Result<FileSegmentedHistory> {
    use crate::format::HS_FIXED_LEN;
    if payload_len < HS_FIXED_LEN as u64 {
        return Err(EkbloomError::Truncated {
            needed: HS_FIXED_LEN as u64,
            available: payload_len,
        });
    }
    let mut fixed = [0u8; HS_FIXED_LEN];
    read_exact_at_len(file, &mut fixed, payload_offset, total)?;
    let header = decode_segmented_header(&fixed)?;
    let need = header
        .checkpoints_offset
        .checked_add(header.checkpoints_len)
        .ok_or(EkbloomError::Overflow {
            what: "history-segmented layout",
        })?;
    if payload_len < need {
        return Err(EkbloomError::Truncated {
            needed: need,
            available: payload_len,
        });
    }
    Ok(FileSegmentedHistory {
        header,
        payload_offset,
        payload_len,
        physical_len: total,
    })
}

/// A fully-safe, file-backed, streaming view of a validated `.ekb` file.
///
/// Unlike [`open`], which materialises the whole logical body (core, history,
/// checkpoints and bloom payloads) into owned `Vec`s, `FileBody` opens the file
/// and keeps only parsed offsets. A read then reads exactly the bytes it needs
/// with `pread`-style `File::read_exact_at` calls:
///
/// * a certified **path bloom** read is a single contiguous read of the
///   `hot[offset..end]` slice (no per-symbol decode),
/// * a **checkpoint** read touches one 12-byte resume point plus the bounded core
///   window `[j*K, end)`,
/// * a **history** read touches the `u32` states for the requested positions plus
///   the core window `[offset, end)`,
/// * the **machine** fallback replays over the core window `[0, end)`.
///
/// At open time the file is validated by streaming its sections through a fixed
/// 64 KiB buffer (checksums/hashes), so a large object is never fully resident.
/// Every absolute offset is validated against the file length before it is read,
/// so a corrupt or truncated file yields an [`EkbloomError`] — never a panic.
/// For any valid file, [`FileBody::read`] returns bytes identical to [`read`].
pub struct FileBody {
    file: File,
    logical_len: u64,
    physical_len: u64,
    machine_id: u32,
    generation: u64,
    core_offset: u64,
    core_len: u64,
    history: Option<FileHistory>,
    checkpoints: Vec<FileCheckpoint>,
    blooms: Vec<FileBloom>,
    certificates: Vec<FileCertificate>,
    segmented: Option<FileSegmentedHistory>,
    profiles: Vec<String>,
}

impl FileBody {
    /// Open `path`, validate the header, footer and every section, and keep only
    /// parsed offsets. Core, history and bloom payloads are never held in RAM.
    pub fn open(path: &Path) -> Result<FileBody> {
        let file = File::open(path)?;
        Self::parse(file)
    }

    /// The number of logical symbols (`n`) in the object.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The on-disk file length in bytes.
    #[inline]
    pub fn physical_len(&self) -> u64 {
        self.physical_len
    }

    /// The logical length (`N`) of the `HISTORY_SEGMENTED` section, if present.
    pub fn segmented_logical_len(&self) -> Option<u64> {
        self.segmented.as_ref().map(|s| s.header.logical_len)
    }

    /// The checkpoint interval of the `HISTORY_SEGMENTED` section, if present.
    pub fn segmented_checkpoint_interval(&self) -> Option<u32> {
        self.segmented
            .as_ref()
            .map(|s| s.header.checkpoint_interval)
    }

    /// Read state `x_i` from the `HISTORY_SEGMENTED` section by `pread`.
    ///
    /// Bounded: one index read plus one 4-byte state read; the whole history is
    /// never loaded. Errors with [`EkbloomError::InvalidArgument`] when the file
    /// has no such section, and with [`EkbloomError::OutOfBounds`] for `i > N`.
    pub fn segmented_state_at(&self, i: u64) -> Result<u32> {
        match &self.segmented {
            Some(seg) => seg.state_at(&self.file, i),
            None => Err(EkbloomError::InvalidArgument {
                what: "file has no history-segmented section".to_string(),
            }),
        }
    }

    /// Read `req` by reading only the bytes it needs.
    ///
    /// Bounds and overflow semantics match [`read`]: an overflowing range yields
    /// [`EkbloomError::Overflow`], a range past `logical_len` yields
    /// [`EkbloomError::OutOfBounds`], and a zero-length read always succeeds.
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_into(req, &mut out)?;
        Ok(out)
    }

    /// Like [`FileBody::read`], but appends into `out` instead of allocating.
    /// `out` is not cleared.
    pub fn read_into(&self, req: ReadRequest, out: &mut Vec<u8>) -> Result<()> {
        let end = req
            .offset
            .checked_add(req.length)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > self.logical_len {
            return Err(oob(req.offset, req.length, self.logical_len));
        }
        if req.length == 0 {
            return Ok(());
        }
        if self.read_via_path(req.offset, end, out)? {
            return Ok(());
        }
        if self.read_via_checkpoint(req.offset, req.length, end, out)? {
            return Ok(());
        }
        if self.read_via_history(req.offset, end, out)? {
            return Ok(());
        }
        self.read_via_machine(req.offset, end, out)
    }

    /// Byte-accounting report, equal to [`crate::manifest::inspect`] of the body
    /// [`open`] would produce, but computed from parsed offsets only.
    pub fn inspect(&self) -> Result<ManifestationReport> {
        let mut components: Vec<ComponentReport> = Vec::new();
        components.push(ComponentReport {
            name: "core".to_string(),
            kind: "core".to_string(),
            bytes: self.core_len,
        });
        if let Some(h) = &self.history {
            let states = h.logical_len.checked_add(1).ok_or(EkbloomError::Overflow {
                what: "history count",
            })?;
            components.push(ComponentReport {
                name: "history".to_string(),
                kind: "history".to_string(),
                bytes: states.saturating_mul(4),
            });
        }
        for (i, cp) in self.checkpoints.iter().enumerate() {
            components.push(ComponentReport {
                name: format!("checkpoint[{i}] K={}", cp.interval),
                kind: "checkpoint".to_string(),
                bytes: cp.count.saturating_mul(Snapshot::PACKED_LEN as u64),
            });
        }
        for (i, b) in self.blooms.iter().enumerate() {
            components.push(ComponentReport {
                name: format!("bloom[{i}] {}", b.kind.as_str()),
                kind: "bloom".to_string(),
                bytes: b.payload_len.saturating_sub(SHELL_HEADER_LEN as u64),
            });
        }
        for (i, c) in self.certificates.iter().enumerate() {
            components.push(ComponentReport {
                name: format!("certificate[{i}] {}", c.kind.as_str()),
                kind: "certificate".to_string(),
                bytes: c.bytes,
            });
        }
        if let Some(seg) = &self.segmented {
            components.push(ComponentReport {
                name: "history-segmented".to_string(),
                kind: "history-segmented".to_string(),
                bytes: seg.payload_len,
            });
        }

        let visible = self.core_len;
        let physical = components
            .iter()
            .fold(0u64, |acc, c| acc.saturating_add(c.bytes));
        let expansion_ratio = if visible == 0 {
            0.0
        } else {
            physical as f64 / visible as f64
        };
        Ok(ManifestationReport {
            logical_len: self.logical_len,
            visible_bytes: visible,
            physical_bytes: physical,
            concealed_bytes: physical.saturating_sub(visible),
            expansion_ratio,
            generation: self.generation,
            profiles: self.profiles.clone(),
            components,
        })
    }

    /// Validate a file by streaming, keeping only offsets and headers.
    fn parse(file: File) -> Result<FileBody> {
        let physical_len = file.metadata()?.len();
        let (header, commits, torn_error) = scan_file(&file, physical_len)?;
        let selected = match commits.last().cloned() {
            Some(commit) => commit,
            None => {
                return Err(torn_error.unwrap_or_else(|| EkbloomError::RecoveryFailed {
                    reason: "no valid committed generation".to_string(),
                }))
            }
        };

        let core_offset = format::CORE_OFFSET;
        let core_len = header.core_len;
        let machine =
            machine_by_id(header.core_machine_id).ok_or(EkbloomError::UnknownMachine {
                id: header.core_machine_id,
            })?;

        let mut history: Option<FileHistory> = None;
        let mut checkpoints: Vec<FileCheckpoint> = Vec::new();
        let mut blooms: Vec<FileBloom> = Vec::new();
        let mut certificates: Vec<FileCertificate> = Vec::new();
        let mut segmented: Option<FileSegmentedHistory> = None;

        for commit in &commits {
            for section in &commit.sections {
                let off = section.payload_offset;
                let len = section.payload_len;
                match section.kind {
                    SectionKind::HistoryFull => {
                        history = Some(parse_history(&file, off, len, physical_len)?)
                    }
                    SectionKind::Checkpoints => {
                        checkpoints.push(parse_checkpoint(&file, off, len, physical_len, machine)?)
                    }
                    SectionKind::BloomGlobal
                    | SectionKind::BloomContext
                    | SectionKind::BloomRegion
                    | SectionKind::BloomPath => {
                        let bloom = parse_bloom(&file, off, len, physical_len)?;
                        if section_kind_of(bloom.kind) != section.kind {
                            return Err(corrupt(
                                commit.generation,
                                "bloom kind disagrees with its section",
                            ));
                        }
                        blooms.push(bloom);
                    }
                    SectionKind::Certificate => {
                        certificates.push(parse_certificate(&file, off, len, physical_len)?)
                    }
                    SectionKind::HistorySegmented => {
                        segmented = Some(parse_segmented(&file, off, len, physical_len)?)
                    }
                    SectionKind::Manifest | SectionKind::Auth => {}
                    SectionKind::CoreMachine => {
                        return Err(corrupt(commit.generation, "unexpected core section"))
                    }
                }
            }
        }

        let manifest = manifest_record(&file, &selected, physical_len)?;

        if manifest.generation != selected.generation {
            return Err(corrupt(selected.generation, "manifest generation mismatch"));
        }
        if header.core_machine_id == crate::machine::MACHINE_NL2
            && header.logical_bytes != header.logical_symbols
        {
            return Err(corrupt(
                selected.generation,
                "header logical_bytes disagrees with logical_symbols",
            ));
        }
        if manifest.logical_len != header.logical_symbols {
            return Err(corrupt(
                selected.generation,
                "logical length disagrees with the header",
            ));
        }
        if manifest.object_id != header.object_id {
            return Err(corrupt(
                selected.generation,
                "object id disagrees with the header",
            ));
        }
        if manifest.machine_id != header.core_machine_id {
            return Err(corrupt(
                selected.generation,
                "machine id disagrees with the header",
            ));
        }
        if header.core_machine_id == crate::machine::MACHINE_NL2 && manifest.logical_len > core_len
        {
            return Err(corrupt(
                selected.generation,
                "logical length exceeds core length",
            ));
        }
        let core_digest = blake3_range(&file, core_offset, core_len, physical_len)?;
        if core_digest != manifest.core_hash {
            return Err(corrupt(selected.generation, "core hash mismatch"));
        }
        if let Some(h) = &history {
            if h.core_hash != core_digest || h.logical_len != manifest.logical_len {
                return Err(corrupt(
                    selected.generation,
                    "history core linkage mismatch",
                ));
            }
        }
        for bloom in &blooms {
            if bloom.core_hash != core_digest {
                return Err(corrupt(selected.generation, "bloom core linkage mismatch"));
            }
        }
        for cert in &certificates {
            if cert.core_hash != core_digest {
                return Err(corrupt(
                    selected.generation,
                    "certificate core linkage mismatch",
                ));
            }
        }
        if let Some(seg) = &segmented {
            if seg.header.core_hash != core_digest || seg.header.logical_len != manifest.logical_len
            {
                return Err(corrupt(
                    selected.generation,
                    "history-segmented core linkage mismatch",
                ));
            }
        }
        let expected_components = 1
            + usize::from(history.is_some())
            + checkpoints.len()
            + blooms.len()
            + certificates.len()
            + usize::from(segmented.is_some());
        if manifest.components.len() != expected_components {
            return Err(corrupt(
                selected.generation,
                "component count disagrees with the manifest",
            ));
        }

        Ok(FileBody {
            file,
            logical_len: manifest.logical_len,
            physical_len,
            machine_id: manifest.machine_id,
            generation: selected.generation,
            core_offset,
            core_len,
            history,
            checkpoints,
            blooms,
            certificates,
            segmented,
            profiles: manifest.profiles,
        })
    }

    /// Read the core window `[from, to)` into a fresh buffer (bounds-checked).
    fn read_core(&self, from: u64, to: u64) -> Result<Vec<u8>> {
        if to < from {
            return Err(bad("core window is reversed"));
        }
        if to > self.core_len {
            return Err(EkbloomError::CoreTooShort {
                needed: to,
                available: self.core_len,
            });
        }
        let off = self
            .core_offset
            .checked_add(from)
            .ok_or(EkbloomError::Overflow {
                what: "core window offset",
            })?;
        read_vec_at(&self.file, off, to - from, self.physical_len)
    }

    /// Path 1: read the certified path-bloom `hot` slice directly. Returns `true`
    /// when it served the request.
    fn read_via_path(&self, offset: u64, end: u64, out: &mut Vec<u8>) -> Result<bool> {
        for bloom in &self.blooms {
            if bloom.kind != BloomKind::Path
                || bloom.status != Certification::Certified
                || bloom.logical_len != self.logical_len
            {
                continue;
            }
            let Some(hot_off) = self.path_hot_offset(bloom)? else {
                continue;
            };
            if bloom.logical_len < end {
                continue;
            }
            let len = end - offset;
            let n = usize::try_from(len).map_err(|_| oob(offset, len, self.logical_len))?;
            let prev = out.len();
            let new_len = prev.checked_add(n).ok_or(EkbloomError::Overflow {
                what: "read buffer",
            })?;
            out.resize(new_len, 0);
            let file_off = hot_off.checked_add(offset).ok_or(EkbloomError::Overflow {
                what: "path hot offset",
            })?;
            read_exact_at_len(&self.file, &mut out[prev..], file_off, self.physical_len)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Absolute file offset of a path shell's `hot` array, applying exactly the
    /// structural checks of [`crate::bloom_path::PathBloom::from_shell`] so the
    /// same shell is selected as [`read`]. `None` means "not a usable path bloom".
    fn path_hot_offset(&self, bloom: &FileBloom) -> Result<Option<u64>> {
        if !supported_depth(bloom.depth) {
            return Ok(None);
        }
        let Some(payload_len) = bloom.payload_len.checked_sub(SHELL_HEADER_LEN as u64) else {
            return Ok(None);
        };
        if payload_len < PATH_PAYLOAD_PREFIX_LEN as u64 {
            return Ok(None);
        }
        let prefix_off = bloom
            .payload_offset
            .checked_add(SHELL_HEADER_LEN as u64)
            .ok_or(EkbloomError::Overflow {
                what: "path payload offset",
            })?;
        let mut p = [0u8; PATH_PAYLOAD_PREFIX_LEN];
        read_exact_at_len(&self.file, &mut p, prefix_off, self.physical_len)?;
        let depth = u16::from_le_bytes([p[0], p[1]]);
        if depth != bloom.depth {
            return Ok(None);
        }
        let payload_logical = le_u64(&p, 2);
        if payload_logical != bloom.logical_len {
            return Ok(None);
        }
        let nblocks = le_u64(&p, 10);
        if nblocks != bloom.logical_len.div_ceil(u64::from(depth)) {
            return Ok(None);
        }
        if bloom.entries != nblocks {
            return Ok(None);
        }
        let Some(cold_bytes) = nblocks.checked_mul(4) else {
            return Ok(None);
        };
        let Some(need) = (PATH_PAYLOAD_PREFIX_LEN as u64)
            .checked_add(bloom.logical_len)
            .and_then(|n| n.checked_add(cold_bytes))
        else {
            return Ok(None);
        };
        if payload_len != need {
            return Ok(None);
        }
        let hot_off = bloom
            .payload_offset
            .checked_add((SHELL_HEADER_LEN + PATH_PAYLOAD_PREFIX_LEN) as u64)
            .ok_or(EkbloomError::Overflow {
                what: "path hot offset",
            })?;
        Ok(Some(hot_off))
    }

    /// Path 2: the checkpoint set with the fewest replay steps. Only one resume
    /// point and the bounded core window `[j*K, end)` are read.
    fn read_via_checkpoint(
        &self,
        offset: u64,
        length: u64,
        end: u64,
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        // The checkpoint fast path rebases the core window at the implicit
        // position `j*K`, which is only valid when the cursor equals the logical
        // position. Cursor-divergent machines replay authoritatively instead.
        if self.machine_id != MACHINE_NL2 {
            return Ok(false);
        }
        let mut best: Option<(u64, &FileCheckpoint)> = None;
        for cp in &self.checkpoints {
            if cp.interval == 0 {
                continue;
            }
            let k = u64::from(cp.interval);
            if offset / k >= cp.count {
                continue;
            }
            let steps = offset % k;
            let better = match best {
                Some((best_steps, _)) => steps < best_steps,
                None => true,
            };
            if better {
                best = Some((steps, cp));
            }
        }
        let Some((_, cp)) = best else {
            return Ok(false);
        };

        let k = u64::from(cp.interval);
        let j = offset / k;
        let expected_cursor = j.checked_mul(k).ok_or(EkbloomError::Overflow {
            what: "checkpoint cursor",
        })?;

        let rel = (CHECKPOINT_HEADER_LEN as u64)
            .checked_add(j.checked_mul(Snapshot::PACKED_LEN as u64).ok_or(
                EkbloomError::Overflow {
                    what: "checkpoint entry",
                },
            )?)
            .ok_or(EkbloomError::Overflow {
                what: "checkpoint entry",
            })?;
        let entry_off = cp
            .payload_offset
            .checked_add(rel)
            .ok_or(EkbloomError::Overflow {
                what: "checkpoint entry",
            })?;
        let mut entry = [0u8; Snapshot::PACKED_LEN];
        read_exact_at_len(&self.file, &mut entry, entry_off, self.physical_len)?;
        let omega = Snapshot::unpack(&entry)?;
        if omega.cursor != expected_cursor {
            return Err(bad(format!(
                "checkpoint cursor {} does not match implicit position {expected_cursor}",
                omega.cursor
            )));
        }

        // Replay from `j*K` to `offset`, then reveal `length` symbols. The core
        // window starts at `j*K`; the local cursor is rebased to 0 so `reveal`
        // indexes the window correctly.
        let core = self.read_core(expected_cursor, end)?;
        let steps =
            usize::try_from(offset - expected_cursor).map_err(|_| EkbloomError::Overflow {
                what: "checkpoint replay",
            })?;
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        let mut local = Snapshot::new(omega.state, 0);
        for _ in 0..steps {
            machine.reveal(&core, &mut local)?;
        }
        if let Ok(n) = usize::try_from(length) {
            out.reserve(n);
        }
        for _ in 0..length {
            out.push(machine.reveal(&core, &mut local)?);
        }
        Ok(true)
    }

    /// Path 3: the full history. Only the `u32` states for the requested
    /// positions, plus the core window `[offset, end)`, are read.
    fn read_via_history(&self, offset: u64, end: u64, out: &mut Vec<u8>) -> Result<bool> {
        let Some(history) = &self.history else {
            return Ok(false);
        };
        if history.logical_len != self.logical_len {
            return Ok(false);
        }
        // Only a cursor==position machine can rebase the state-only projection.
        if self.machine_id != MACHINE_NL2 {
            return Ok(false);
        }
        let count = history
            .logical_len
            .checked_add(1)
            .ok_or(EkbloomError::Overflow {
                what: "history count",
            })?;
        if end > count {
            return Ok(false);
        }
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        let core = self.read_core(offset, end)?;

        let span = end - offset;
        let state_bytes = span.checked_mul(4).ok_or(EkbloomError::Overflow {
            what: "history states",
        })?;
        let state_off = history
            .payload_offset
            .checked_add(HISTORY_HEADER_LEN as u64)
            .and_then(|o| o.checked_add(offset.checked_mul(4)?))
            .ok_or(EkbloomError::Overflow {
                what: "history states",
            })?;
        let states = read_vec_at(&self.file, state_off, state_bytes, self.physical_len)?;

        if let Ok(n) = usize::try_from(span) {
            out.reserve(n);
        }
        for i in 0..span {
            let s = usize::try_from(i).map_err(|_| EkbloomError::Overflow {
                what: "history index",
            })?;
            let state = u32::from_le_bytes([
                states[s * 4],
                states[s * 4 + 1],
                states[s * 4 + 2],
                states[s * 4 + 3],
            ]);
            // Rebase the cursor onto the window starting at `offset`.
            let mut omega = Snapshot::new(state, i);
            out.push(machine.reveal(&core, &mut omega)?);
        }
        Ok(true)
    }

    /// Path 4: authoritative replay from `Omega_0` over the core window `[0, end)`.
    fn read_via_machine(&self, offset: u64, end: u64, out: &mut Vec<u8>) -> Result<()> {
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        if self.machine_id == MACHINE_NL2 && self.logical_len > self.core_len {
            return Err(EkbloomError::CoreTooShort {
                needed: self.logical_len,
                available: self.core_len,
            });
        }
        // Machine 1 consumes one core byte per symbol, so `[0, end)` suffices.
        // A cursor-divergent machine may need bytes past `end`, so read the whole
        // core (the read is still bounded by the file size).
        let core = if self.machine_id == MACHINE_NL2 {
            self.read_core(0, end)?
        } else {
            self.read_core(0, self.core_len)?
        };
        if let Ok(n) = usize::try_from(end - offset) {
            out.reserve(n);
        }
        let mut omega = machine.init(&core)?;
        let mut pos = 0u64;
        while pos < offset {
            machine.reveal(&core, &mut omega)?;
            pos += 1;
        }
        while pos < end {
            out.push(machine.reveal(&core, &mut omega)?);
            pos += 1;
        }
        Ok(())
    }
}

/// Forward-scan a file by streaming: validate the header and every commit block,
/// returning the valid prefix and any torn tail. Mirrors [`format::scan`] without
/// loading the file into memory.
fn scan_file(
    file: &File,
    total: u64,
) -> Result<(
    format::GenesisHeader,
    Vec<format::ScannedCommit>,
    Option<EkbloomError>,
)> {
    let mut header_buf = [0u8; format::HEADER_LEN];
    read_exact_at_len(file, &mut header_buf, 0, total)?;
    let header = format::GenesisHeader::decode(&header_buf)?;
    let core_end =
        format::CORE_OFFSET
            .checked_add(header.core_len)
            .ok_or(EkbloomError::Overflow {
                what: "core extent",
            })?;
    if total < core_end {
        return Err(EkbloomError::Truncated {
            needed: core_end,
            available: total,
        });
    }

    let mut commits: Vec<format::ScannedCommit> = Vec::new();
    let mut prev_generation = 0u64;
    let mut torn_error = None;
    let mut pos = format::align8(core_end);
    while pos < total {
        match scan_one_commit(file, total, pos, prev_generation) {
            Ok(commit) => {
                prev_generation = commit.generation;
                pos = commit.end;
                commits.push(commit);
            }
            Err(e) => {
                torn_error = Some(e);
                break;
            }
        }
    }
    Ok((header, commits, torn_error))
}

/// Parse and validate a single commit block starting at `start`, streaming every
/// section payload through a bounded checksum buffer.
fn scan_one_commit(
    file: &File,
    total: u64,
    start: u64,
    expected_prev: u64,
) -> Result<format::ScannedCommit> {
    let mut sections: Vec<format::ScannedSection> = Vec::new();
    let mut cur = start;
    let footer_start;

    loop {
        let header_end =
            cur.checked_add(format::SECTION_HEADER_LEN as u64)
                .ok_or(EkbloomError::Overflow {
                    what: "section header extent",
                })?;
        if total < header_end {
            return Err(EkbloomError::Truncated {
                needed: header_end,
                available: total,
            });
        }
        let mut hbuf = [0u8; format::SECTION_HEADER_LEN];
        file.read_exact_at(&mut hbuf, cur)?;
        let header = format::SectionHeader::decode(&hbuf)?;
        let expected_offset = header_end;
        if header.offset != expected_offset {
            return Err(bad(format!(
                "section offset {} does not follow its header at {cur}",
                header.offset
            )));
        }
        let payload_end =
            expected_offset
                .checked_add(header.length)
                .ok_or(EkbloomError::Overflow {
                    what: "section payload extent",
                })?;
        if total < payload_end {
            return Err(EkbloomError::Truncated {
                needed: payload_end,
                available: total,
            });
        }
        let actual = crc32_range(file, expected_offset, header.length, total)?;
        if actual != header.checksum {
            return Err(EkbloomError::ChecksumMismatch {
                section: header.kind.name(),
            });
        }
        sections.push(format::ScannedSection {
            kind: header.kind,
            generation: header.generation,
            header_offset: cur,
            payload_offset: expected_offset,
            payload_len: header.length,
        });
        cur = format::align8(payload_end);
        if header.kind == SectionKind::Manifest {
            // Optional trailing AUTH section, immediately before the footer.
            if let Some((auth, next)) = scan_auth_opt_file(file, cur, total)? {
                sections.push(auth);
                cur = next;
            }
            footer_start = cur;
            break;
        }
    }

    let footer_end =
        footer_start
            .checked_add(format::FOOTER_LEN as u64)
            .ok_or(EkbloomError::Overflow {
                what: "footer extent",
            })?;
    if total < footer_end {
        return Err(EkbloomError::Truncated {
            needed: footer_end,
            available: total,
        });
    }
    let mut fbuf = [0u8; format::FOOTER_LEN];
    file.read_exact_at(&mut fbuf, footer_start)?;
    let footer = format::CommitFooter::decode(&fbuf)?;

    let manifest_idx = sections
        .iter()
        .rposition(|s| s.kind == SectionKind::Manifest)
        .ok_or_else(|| bad("commit has no manifest section"))?;
    // Only the optional trailing AUTH section may follow the manifest.
    if sections[manifest_idx + 1..]
        .iter()
        .any(|s| s.kind != SectionKind::Auth)
    {
        return Err(bad(
            "commit does not end with its manifest (and optional AUTH)",
        ));
    }
    let manifest = sections[manifest_idx];
    if footer.manifest_offset != manifest.header_offset
        || footer.manifest_len != manifest.payload_len
    {
        return Err(bad(format!(
            "footer manifest pointer ({}, {}) disagrees with section ({}, {})",
            footer.manifest_offset,
            footer.manifest_len,
            manifest.header_offset,
            manifest.payload_len
        )));
    }
    if footer.prev_generation != expected_prev {
        return Err(bad(format!(
            "footer prev_generation {} does not link to {expected_prev}",
            footer.prev_generation
        )));
    }
    let manifest_payload = read_vec_at(file, manifest.payload_offset, manifest.payload_len, total)?;
    let actual_hash = *blake3::hash(&manifest_payload).as_bytes();
    let mut expected_hash = [0u8; 16];
    expected_hash.copy_from_slice(&actual_hash[..16]);
    if footer.manifest_hash != expected_hash {
        return Err(EkbloomError::ChecksumMismatch {
            section: "manifest",
        });
    }
    let record = format::ManifestRecord::decode(&manifest_payload)?;
    if record.generation != footer.generation {
        return Err(bad(format!(
            "manifest generation {} disagrees with footer {}",
            record.generation, footer.generation
        )));
    }

    Ok(format::ScannedCommit {
        generation: footer.generation,
        prev_generation: footer.prev_generation,
        manifest_offset: footer.manifest_offset,
        manifest_len: footer.manifest_len,
        manifest_hash: footer.manifest_hash,
        start,
        end: footer_end,
        sections,
    })
}

/// Decode the `MANIFEST` record of a scanned commit.
fn manifest_record(
    file: &File,
    commit: &format::ScannedCommit,
    total: u64,
) -> Result<format::ManifestRecord> {
    let section = commit
        .sections
        .iter()
        .rev()
        .find(|s| s.kind == SectionKind::Manifest)
        .ok_or_else(|| bad("commit has no manifest section"))?;
    let payload = read_vec_at(file, section.payload_offset, section.payload_len, total)?;
    format::ManifestRecord::decode(&payload)
}

/// If a well-formed [`SectionKind::Auth`] section begins at absolute `offset` in
/// `file`, return its descriptor and the aligned offset just past it; otherwise
/// `Ok(None)`. Structural mismatches are not errors: a malformed candidate is
/// left for the footer parser to reject (fail closed).
fn scan_auth_opt_file(
    file: &File,
    offset: u64,
    total: u64,
) -> Result<Option<(format::ScannedSection, u64)>> {
    let header_end = match offset.checked_add(format::SECTION_HEADER_LEN as u64) {
        Some(v) => v,
        None => return Ok(None),
    };
    if header_end > total {
        return Ok(None);
    }
    let mut hbuf = [0u8; format::SECTION_HEADER_LEN];
    if file.read_exact_at(&mut hbuf, offset).is_err() {
        return Ok(None);
    }
    let header = match format::SectionHeader::decode(&hbuf) {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    if header.kind != SectionKind::Auth
        || header.offset != header_end
        || header.length != format::AUTH_MAC_LEN as u64
    {
        return Ok(None);
    }
    let payload_end = match header_end.checked_add(header.length) {
        Some(v) => v,
        None => return Ok(None),
    };
    if payload_end > total
        || crc32_range(file, header_end, header.length, total)? != header.checksum
    {
        return Ok(None);
    }
    let section = format::ScannedSection {
        kind: SectionKind::Auth,
        generation: header.generation,
        header_offset: offset,
        payload_offset: header_end,
        payload_len: header.length,
    };
    Ok(Some((section, format::align8(payload_end))))
}

/// Parse a `BloomShell` header; the construction payload is never loaded.
fn parse_bloom(
    file: &File,
    payload_offset: u64,
    payload_len: u64,
    total: u64,
) -> Result<FileBloom> {
    if payload_len < SHELL_HEADER_LEN as u64 {
        return Err(EkbloomError::Truncated {
            needed: SHELL_HEADER_LEN as u64,
            available: payload_len,
        });
    }
    let mut shell = [0u8; SHELL_HEADER_LEN];
    read_exact_at_len(file, &mut shell, payload_offset, total)?;
    let kind = BloomKind::from_u32(u32::from_le_bytes([shell[0], shell[1], shell[2], shell[3]]))
        .ok_or_else(|| bad("unknown bloom shell kind"))?;
    let depth = u16::from_le_bytes([shell[4], shell[5]]);
    let logical_len = le_u64(&shell, 14);
    let entries = le_u64(&shell, 22);
    let status = Certification::from_u32(u32::from_le_bytes([
        shell[30], shell[31], shell[32], shell[33],
    ]))
    .ok_or_else(|| bad("unknown bloom shell status"))?;
    let mut core_hash = [0u8; 32];
    core_hash.copy_from_slice(&shell[34..66]);
    let declared_payload = le_u64(&shell, 66);
    if declared_payload != payload_len - SHELL_HEADER_LEN as u64 {
        return Err(bad(
            "bloom shell payload length does not match available bytes",
        ));
    }
    Ok(FileBloom {
        kind,
        depth,
        logical_len,
        entries,
        status,
        core_hash,
        payload_offset,
        payload_len,
    })
}

/// Parse a checkpoint-set header and validate every resume point's cursor via
/// bounded batch reads (never materialising the set).
fn parse_checkpoint(
    file: &File,
    payload_offset: u64,
    payload_len: u64,
    total: u64,
    machine: &'static dyn crate::machine::OrMachine,
) -> Result<FileCheckpoint> {
    if payload_len < CHECKPOINT_HEADER_LEN as u64 {
        return Err(EkbloomError::Truncated {
            needed: CHECKPOINT_HEADER_LEN as u64,
            available: payload_len,
        });
    }
    let mut h = [0u8; CHECKPOINT_HEADER_LEN];
    read_exact_at_len(file, &mut h, payload_offset, total)?;
    let interval = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    if !CheckpointSet::supported_interval(interval) {
        return Err(EkbloomError::BadInterval { interval });
    }
    let count = le_u64(&h, 4);
    if count == 0 {
        return Err(bad("checkpoint set count is zero"));
    }
    let k = u64::from(interval);
    (count - 1).checked_mul(k).ok_or(EkbloomError::Overflow {
        what: "checkpoint span",
    })?;
    let need = count
        .checked_mul(Snapshot::PACKED_LEN as u64)
        .and_then(|n| n.checked_add(CHECKPOINT_HEADER_LEN as u64))
        .ok_or(EkbloomError::Overflow {
            what: "checkpoint byte length",
        })?;
    if payload_len < need {
        return Err(EkbloomError::Truncated {
            needed: need,
            available: payload_len,
        });
    }
    validate_checkpoint_entries(file, payload_offset, count, k, total, machine)?;
    Ok(FileCheckpoint {
        interval,
        count,
        payload_offset,
    })
}

/// Validate every resume point's implicit cursor using bounded batch reads.
///
/// The `cursor == j*K` invariant is only enforced for machine 1; other machines
/// store the true backing cursor verbatim.
fn validate_checkpoint_entries(
    file: &File,
    payload_offset: u64,
    count: u64,
    k: u64,
    total: u64,
    machine: &'static dyn crate::machine::OrMachine,
) -> Result<()> {
    let enforce_cursor = machine.machine_id() == MACHINE_NL2;
    let batch = CHECKPOINT_VALIDATE_BATCH as usize;
    let mut buf = vec![0u8; batch.saturating_mul(Snapshot::PACKED_LEN)];
    let mut j = 0u64;
    while j < count {
        let n = (count - j).min(CHECKPOINT_VALIDATE_BATCH);
        let bytes = (n as usize) * Snapshot::PACKED_LEN;
        let off = payload_offset
            .checked_add(CHECKPOINT_HEADER_LEN as u64)
            .and_then(|o| o.checked_add(j.checked_mul(Snapshot::PACKED_LEN as u64)?))
            .ok_or(EkbloomError::Overflow {
                what: "checkpoint entry",
            })?;
        read_exact_at_len(file, &mut buf[..bytes], off, total)?;
        for e in 0..n {
            let lo = (e as usize) * Snapshot::PACKED_LEN;
            let s = Snapshot::unpack(&buf[lo..lo + Snapshot::PACKED_LEN])?;
            if enforce_cursor {
                let expected = (j + e).checked_mul(k).ok_or(EkbloomError::Overflow {
                    what: "checkpoint cursor",
                })?;
                if s.cursor != expected {
                    return Err(bad(format!(
                        "checkpoint {} cursor {} does not match implicit position {expected}",
                        j + e,
                        s.cursor
                    )));
                }
            }
        }
        j += n;
    }
    Ok(())
}

/// Parse a history header, validating its count and length without loading the
/// state projection.
fn parse_history(
    file: &File,
    payload_offset: u64,
    payload_len: u64,
    total: u64,
) -> Result<FileHistory> {
    if payload_len < HISTORY_HEADER_LEN as u64 {
        return Err(EkbloomError::Truncated {
            needed: HISTORY_HEADER_LEN as u64,
            available: payload_len,
        });
    }
    let mut h = [0u8; HISTORY_HEADER_LEN];
    read_exact_at_len(file, &mut h, payload_offset, total)?;
    let logical_len = le_u64(&h, 8);
    let mut core_hash = [0u8; 32];
    core_hash.copy_from_slice(&h[24..56]);
    let count = le_u64(&h, 56);
    let want_count = logical_len.checked_add(1).ok_or(EkbloomError::Overflow {
        what: "history logical length",
    })?;
    if count != want_count {
        return Err(bad(format!(
            "history count {count} inconsistent with logical_len {logical_len}"
        )));
    }
    let need = count
        .checked_mul(4)
        .and_then(|n| n.checked_add(HISTORY_HEADER_LEN as u64))
        .ok_or(EkbloomError::Overflow {
            what: "history byte length",
        })?;
    if payload_len < need {
        return Err(EkbloomError::Truncated {
            needed: need,
            available: payload_len,
        });
    }
    Ok(FileHistory {
        logical_len,
        core_hash,
        payload_offset,
    })
}

/// Parse a certificate into the fields `inspect` and linkage need.
fn parse_certificate(
    file: &File,
    payload_offset: u64,
    payload_len: u64,
    total: u64,
) -> Result<FileCertificate> {
    let payload = read_vec_at(file, payload_offset, payload_len, total)?;
    let cert = Certificate::from_bytes(&payload)?;
    Ok(FileCertificate {
        kind: cert.kind,
        core_hash: cert.core_hash,
        bytes: payload_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{OrMachine, NL2};
    use crate::manifest::create;
    use crate::ManifestationProfile;

    fn lcg(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push((s >> 33) as u8);
        }
        out
    }

    #[test]
    fn bounds_and_zero_length_reads() {
        let body = create(b"abc", ManifestationProfile::Root).unwrap();
        assert!(matches!(
            read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: 4
                }
            ),
            Err(EkbloomError::OutOfBounds { .. })
        ));
        assert!(matches!(
            read(
                &body,
                ReadRequest {
                    offset: 4,
                    length: 0
                }
            ),
            Err(EkbloomError::OutOfBounds { .. })
        ));
        assert!(matches!(
            read(
                &body,
                ReadRequest {
                    offset: u64::MAX,
                    length: 2
                }
            ),
            Err(EkbloomError::Overflow { .. })
        ));
        assert_eq!(
            read(
                &body,
                ReadRequest {
                    offset: 3,
                    length: 0
                }
            )
            .unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: 3
                }
            )
            .unwrap(),
            b"abc".to_vec()
        );
    }

    #[test]
    fn file_backed_open_roundtrip_and_bad_magic() {
        let dir = std::env::temp_dir().join(format!("ekbloom-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.ekb");
        let src = lcg(0x1234_5678, 300);
        let body = create(&src, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        crate::writer::write_body(&path, &body).unwrap();

        let opened = open(&path).unwrap();
        assert_eq!(opened.logical_len, src.len() as u64);
        assert!(crate::verify::verify(&opened).unwrap().ok);

        let mut bad = std::fs::read(&path).unwrap();
        bad[0] ^= 0xFF;
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(open(&path), Err(EkbloomError::BadMagic)));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_access_path_matches_the_machine() {
        let src = lcg(0xBEEF_1234, 1200);
        let core = NL2.encode(&src).unwrap();
        let reference = NL2.decode(&core, src.len() as u64).unwrap();

        let profiles = [
            ManifestationProfile::Root,
            ManifestationProfile::Sparse { interval: 1024 },
            ManifestationProfile::Full,
            ManifestationProfile::Path { depth: 4 },
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 512,
            },
            ManifestationProfile::MaxSpeed,
        ];
        for profile in profiles {
            let body = create(&src, profile).unwrap();
            // Debug agreement across every available path.
            let mut ranges = Vec::new();
            for &(off, len) in &[
                (0u64, 0u64),
                (0, src.len() as u64),
                (1, 63),
                (500, 700),
                (src.len() as u64 - 1, 1),
                (src.len() as u64, 0),
            ] {
                ranges.push((off, len));
            }
            for (off, len) in ranges {
                let got = read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: len,
                    },
                )
                .unwrap();
                assert_eq!(
                    got,
                    reference[off as usize..(off + len) as usize].to_vec(),
                    "profile {} range ({off},{len})",
                    body.profiles.last().unwrap()
                );
            }
        }
    }
}
