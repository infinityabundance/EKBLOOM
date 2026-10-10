//! Transactional `.ekb` publication.
//!
//! Two entry points:
//!
//! - [`write_body`] creates a fresh, fully-committed file.
//! - [`append_generation`] appends a new commit block (its new sections, a
//!   `MANIFEST`, and a `CommitFooter`) to an existing file, returning the new
//!   generation.
//!
//! # Durability model
//!
//! Publication follows `docs/DESIGN.md` §16: write section payloads in order,
//! write the manifest, flush, then append a complete `CommitFooter` and flush.
//! [`write_body`] writes to a sibling `*.tmp` file, `fsync`s it, atomically
//! renames it over the destination, and `fsync`s the containing directory.
//! [`append_generation`] opens the file, truncates any uncommitted tail, appends
//! the new block, and `fsync`s.
//!
//! The durability contract is **append + fsync + validated footer**. A crash can
//! therefore leave a torn *final* block (a partial section, manifest, or footer),
//! but a torn block never invalidates an earlier committed generation: the
//! reader validates footers and falls back to the newest valid commit. This does
//! **not** assume that an append is atomic on the filesystem; correctness rests
//! on footer validation, not on atomicity.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::bloom::BloomKind;
use crate::error::{EkbloomError, Result};
use crate::format::{
    self, align8, CommitFooter, ManifestComponent, ManifestRecord, SectionHeader, SectionKind,
    CORE_OFFSET, FOOTER_LEN, HEADER_LEN, SECTION_HEADER_LEN,
};
use crate::manifest::{inspect, EkbloomBody};

/// Create a fresh, committed `.ekb` file at `path` from `body`.
///
/// Any existing file at `path` is replaced atomically. The file's first commit
/// uses `body.generation` and has `prev_generation = 0`.
pub fn write_body(path: &Path, body: &EkbloomBody) -> Result<()> {
    let header = crate::format::GenesisHeader {
        format_major: format::FORMAT_MAJOR,
        format_minor: format::FORMAT_MINOR,
        feature_flags: 0,
        object_id: body.object_id,
        logical_symbols: body.logical_len,
        logical_bytes: body.logical_len,
        core_len: body.core.len() as u64,
        core_machine_id: body.machine_id,
    };

    let sections = all_sections(body);
    let core_end = CORE_OFFSET + body.core.len() as u64;
    let base = align8(core_end);
    let commit = build_commit(base, body.generation, 0, &sections)?;

    let tmp = tmp_path(path);
    let result = (|| -> Result<()> {
        let file = File::create(&tmp)?;
        let mut w = BufWriter::new(file);
        w.write_all(&header.encode())?;
        w.write_all(&body.core)?;
        write_zeros(&mut w, base - core_end)?;
        w.write_all(&commit.bytes)?;
        w.flush()?;
        let file = w
            .into_inner()
            .map_err(|e| EkbloomError::Io(e.into_error()))?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, path)?;
    fsync_dir(path)?;
    Ok(())
}

/// Append a new committed generation to `path` from `body`, returning the new
/// generation number.
///
/// Only structures not already present in the file are written; earlier bytes are
/// never rewritten. `body.generation` must be strictly greater than the current
/// newest committed generation, and `body` must describe the same object (same
/// core, `object_id`, `logical_len`, `machine_id`, `core_hash`) as the file.
///
/// If the file has an uncommitted torn tail, it is truncated back to the newest
/// valid commit before appending, so the new commit's `prev_generation` links to
/// a complete predecessor.
pub fn append_generation(path: &Path, body: &EkbloomBody) -> Result<u64> {
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
    let file = OpenOptions::new().write(true).open(path)?;
    if (bytes.len() as u64) > clean_len {
        file.set_len(clean_len)?;
    }
    drop(file);

    let base = align8(clean_len);
    let commit = build_commit(base, body.generation, last.generation, &sections)?;

    let mut file = OpenOptions::new().append(true).open(path)?;
    if clean_len % 8 != 0 {
        write_zeros(&mut file, base - clean_len)?;
    }
    file.write_all(&commit.bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(body.generation)
}

/// A planned commit block: the exact bytes to place at `base`.
struct CommitPlan {
    bytes: Vec<u8>,
}

/// Serialize `sections` as an aligned commit block beginning at absolute offset
/// `base` (which must be 8-aligned), closing with a `CommitFooter`.
fn build_commit(
    base: u64,
    generation: u64,
    prev_generation: u64,
    sections: &[(SectionKind, Vec<u8>)],
) -> Result<CommitPlan> {
    let mut out: Vec<u8> = Vec::new();
    let mut off = base;
    let mut manifest: Option<(u64, u64, [u8; 16])> = None;

    for (kind, payload) in sections {
        write_zeros(&mut out, align8(off) - off)?;
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

    write_zeros(&mut out, align8(off) - off)?;

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

    Ok(CommitPlan { bytes: out })
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
                SectionKind::CoreMachine | SectionKind::Manifest => {}
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
    header: &format::GenesisHeader,
    bytes: &[u8],
    last: &format::ScannedCommit,
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

/// Write `n` zero bytes (padding).
fn write_zeros<W: Write>(w: &mut W, n: u64) -> Result<()> {
    if n == 0 {
        return Ok(());
    }
    let chunk = [0u8; 64];
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(chunk.len() as u64) as usize;
        w.write_all(&chunk[..take])?;
        remaining -= take as u64;
    }
    Ok(())
}

/// `fsync` the directory containing `path`, so the rename is durable.
fn fsync_dir(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let file = File::open(dir)?;
    file.sync_all()?;
    Ok(())
}

/// Fixed on-disk prefix sizes, exposed for tests and reporting.
pub const FILE_HEADER_LEN: usize = HEADER_LEN;
/// Fixed on-disk footer size.
pub const FILE_FOOTER_LEN: usize = FOOTER_LEN;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::create;
    use crate::{ManifestationProfile, ReadRequest};

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "ekbloom-writer-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_open_roundtrip() {
        let dir = temp_dir("rt");
        let path = dir.join("a.ekb");
        let src = b"writer roundtrip".to_vec();
        let body = create(&src, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        write_body(&path, &body).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], &format::MAGIC);
        assert_eq!(
            &bytes[bytes.len() - 16..bytes.len() - 8],
            &format::COMMIT_MARKER
        );

        let opened = crate::reader::open(&path).unwrap();
        assert!(crate::verify::verify(&opened).unwrap().ok);
        assert_eq!(
            crate::read_file(
                &path,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64
                }
            )
            .unwrap(),
            src
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_preserves_prefix() {
        let dir = temp_dir("app");
        let path = dir.join("b.ekb");
        let src = vec![0x5Au8; 300];
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        write_body(&path, &body).unwrap();
        let prefix = std::fs::read(&path).unwrap();

        crate::manifest::grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        let gen = append_generation(&path, &body).unwrap();
        assert_eq!(gen, body.generation);

        let after = std::fs::read(&path).unwrap();
        assert_eq!(&after[..prefix.len()], &prefix[..], "prefix rewritten");

        let opened = crate::reader::open(&path).unwrap();
        assert_eq!(opened.generation, gen);
        assert_eq!(opened.checkpoints.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
