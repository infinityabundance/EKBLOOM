//! Crash recovery and torn-write detection for `.ekb` files.
//!
//! Publication appends a validated [`CommitFooter`](crate::format::CommitFooter)
//! after each commit's sections. Recovery exploits that: it forward-scans the
//! file, validating every section checksum, the `MANIFEST` record, and the footer
//! marker/checksum. Parsing stops at the first failure, so a torn *final* block
//! (a partially written section, manifest, or footer, a corrupted payload, or
//! trailing garbage) is reported as an invalid tail while every earlier committed
//! generation remains valid and readable.
//!
//! [`recover`] never mutates the file. [`truncate_to_valid`] removes a torn tail,
//! preserving all earlier commits.

use std::fs::OpenOptions;
use std::path::Path;

use crate::error::{EkbloomError, Result};
use crate::format;

/// Per-generation outcome of a recovery scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationRecord {
    /// The generation number (best-effort for a torn tail).
    pub generation: u64,
    /// Whether the generation validated completely.
    pub valid: bool,
    /// Human-readable explanation.
    pub note: String,
}

/// The aggregate result of a recovery scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Newest generation that validated completely, if any.
    pub newest_valid: Option<u64>,
    /// Every generation considered, oldest first (with any torn tail last).
    pub generations: Vec<GenerationRecord>,
    /// Number of trailing bytes that could not be validated.
    pub torn_tail_bytes: u64,
    /// Whether the file was modified to remove a torn tail.
    pub repaired: bool,
}

/// Walk every footer in `path` and validate each committed generation.
///
/// Does not mutate the file. A torn/corrupt tail is reported but never counted
/// as a valid generation.
pub fn scan_generations(path: &Path) -> Result<RecoveryReport> {
    let bytes = std::fs::read(path)?;
    build_report(&bytes, false)
}

/// Select the newest valid committed generation without mutating the file.
pub fn recover(path: &Path) -> Result<RecoveryReport> {
    let bytes = std::fs::read(path)?;
    build_report(&bytes, false)
}

/// Remove an unvalidatable tail, preserving every earlier committed generation.
///
/// If the file is already clean this is a no-op (`repaired == false`). If there
/// is a non-empty tail but no valid commit to preserve, this fails with
/// [`EkbloomError::RecoveryFailed`].
pub fn truncate_to_valid(path: &Path) -> Result<RecoveryReport> {
    let bytes = std::fs::read(path)?;
    let scan = format::scan(&bytes)?;
    if scan.torn_tail_bytes == 0 {
        return build_report(&bytes, false);
    }
    let last = scan
        .commits
        .last()
        .ok_or_else(|| EkbloomError::RecoveryFailed {
            reason: "no valid committed generation to preserve".to_string(),
        })?;
    let clean_len = last.end;
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(clean_len)?;
    file.sync_all()?;
    build_report(&bytes[..clean_len as usize], true)
}

/// Build a [`RecoveryReport`] from a file image.
fn build_report(bytes: &[u8], repaired: bool) -> Result<RecoveryReport> {
    let scan = format::scan(bytes)?;
    let mut generations: Vec<GenerationRecord> = scan
        .commits
        .iter()
        .map(|commit| GenerationRecord {
            generation: commit.generation,
            valid: true,
            note: format!("committed: {} section(s)", commit.sections.len()),
        })
        .collect();

    if scan.torn_tail_bytes > 0 {
        let generation = scan
            .commits
            .last()
            .map(|c| c.generation.saturating_add(1))
            .unwrap_or(0);
        let reason = scan
            .torn_note
            .unwrap_or_else(|| "unvalidatable tail".to_string());
        generations.push(GenerationRecord {
            generation,
            valid: false,
            note: format!("torn tail ({} bytes): {reason}", scan.torn_tail_bytes),
        });
    }

    Ok(RecoveryReport {
        newest_valid: scan.commits.last().map(|c| c.generation),
        generations,
        torn_tail_bytes: scan.torn_tail_bytes,
        repaired,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{create, grow};
    use crate::writer::{append_generation, write_body};
    use crate::ManifestationProfile;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-recovery-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clean_file_has_two_generations() {
        let dir = temp_dir("clean");
        let path = dir.join("a.ekb");
        let src = vec![0x11u8; 200];
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        write_body(&path, &body).unwrap();
        grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        append_generation(&path, &body).unwrap();

        let report = scan_generations(&path).unwrap();
        assert_eq!(report.newest_valid, Some(2));
        assert_eq!(report.generations.len(), 2);
        assert!(report.generations.iter().all(|g| g.valid));
        assert_eq!(report.torn_tail_bytes, 0);
        assert!(!report.repaired);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn torn_tail_falls_back_and_truncates() {
        let dir = temp_dir("torn");
        let path = dir.join("b.ekb");
        let src = vec![0x22u8; 300];
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        write_body(&path, &body).unwrap();
        grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        append_generation(&path, &body).unwrap();

        // Chop the final generation's footer.
        let bytes = std::fs::read(&path).unwrap();
        let cut = bytes.len() - 20;
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(cut as u64).unwrap();
        drop(file);

        let report = recover(&path).unwrap();
        assert_eq!(report.newest_valid, Some(1));
        assert!(report.torn_tail_bytes > 0);
        assert!(report.generations.iter().any(|g| !g.valid));

        // The older commit is still readable.
        let opened = crate::reader::open(&path).unwrap();
        assert_eq!(opened.generation, 1);
        assert_eq!(
            crate::read_file(
                &path,
                crate::ReadRequest {
                    offset: 0,
                    length: src.len() as u64
                }
            )
            .unwrap(),
            src
        );

        let repaired = truncate_to_valid(&path).unwrap();
        assert!(repaired.repaired);
        assert_eq!(repaired.newest_valid, Some(1));
        assert_eq!(repaired.torn_tail_bytes, 0);
        let clean = scan_generations(&path).unwrap();
        assert_eq!(clean.newest_valid, Some(1));
        assert_eq!(clean.torn_tail_bytes, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
