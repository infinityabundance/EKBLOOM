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

use crate::bloom::{BloomKind, BloomShell, Certificate, Certification};
use crate::bloom_path::PathBloom;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::format::{self, SectionKind};
use crate::history::History;
use crate::machine::machine_by_id;
use crate::manifest::{EkbloomBody, ManifestationReport};
use crate::ReadRequest;
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

    let mut history: Option<History> = None;
    let mut checkpoints: Vec<CheckpointSet> = Vec::new();
    let mut blooms: Vec<BloomShell> = Vec::new();
    let mut certificates: Vec<Certificate> = Vec::new();

    for commit in &scan.commits {
        for section in &commit.sections {
            let payload = format::section_payload(bytes, section);
            match section.kind {
                SectionKind::HistoryFull => history = Some(History::from_bytes(payload)?),
                SectionKind::Checkpoints => checkpoints.push(CheckpointSet::from_bytes(payload)?),
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
                SectionKind::Manifest => {}
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
    if *blake3::hash(&core).as_bytes() != manifest.core_hash {
        return Err(corrupt(selected.generation, "core hash mismatch"));
    }
    if let Some(h) = &history {
        if h.core_hash != manifest.core_hash || h.logical_len != manifest.logical_len {
            return Err(corrupt(
                selected.generation,
                "history core linkage mismatch",
            ));
        }
    }
    for shell in &blooms {
        if shell.core_hash != manifest.core_hash {
            return Err(corrupt(selected.generation, "bloom core linkage mismatch"));
        }
    }
    for cert in &certificates {
        if cert.core_hash != manifest.core_hash {
            return Err(corrupt(
                selected.generation,
                "certificate core linkage mismatch",
            ));
        }
    }
    let expected_components =
        1 + usize::from(history.is_some()) + checkpoints.len() + blooms.len() + certificates.len();
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
    let mut omega = cp.restore(&body.core, offset)?;
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
    let mut omega = machine.init();
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
