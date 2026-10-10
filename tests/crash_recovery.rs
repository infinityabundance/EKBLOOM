//! Crash-recovery tests: torn footers, corrupted payloads, trailing garbage, and
//! fully-corrupt images. In every partial case the previous committed generation
//! must remain the newest valid one and stay readable, and `truncate_to_valid`
//! must restore a clean file.

use std::path::{Path, PathBuf};

use ekbloom::format::{self, SectionKind};
use ekbloom::{
    append_generation, create, grow, open, read_file, recover, scan_generations, truncate_to_valid,
    write_body, ManifestationProfile, ReadRequest,
};

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

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-crash-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a two-generation file: `Root` (gen 1) then `Sparse` (gen 2).
fn two_generations(dir: &Path, name: &str, src: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    append_generation(&path, &body).unwrap();
    path
}

fn assert_reads_source(path: &Path, src: &[u8]) {
    let opened = open(path).unwrap();
    assert!(ekbloom::verify(&opened).unwrap().ok);
    assert_eq!(
        read_file(
            path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );
}

#[test]
fn truncate_mid_footer_keeps_previous_generation() {
    let dir = temp_dir("footer");
    let src = lcg(0xC0FFEE, 300);
    let path = two_generations(&dir, "a.ekb", &src);

    let bytes = std::fs::read(&path).unwrap();
    let cut = bytes.len() - 20; // split the final footer
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(cut as u64)
        .unwrap();

    let report = recover(&path).unwrap();
    assert_eq!(report.newest_valid, Some(1));
    assert!(report.torn_tail_bytes > 0);
    assert!(report.generations.iter().any(|g| !g.valid));
    assert_reads_source(&path, &src);

    let repaired = truncate_to_valid(&path).unwrap();
    assert!(repaired.repaired);
    assert_eq!(repaired.newest_valid, Some(1));
    assert_eq!(repaired.torn_tail_bytes, 0);
    assert_eq!(scan_generations(&path).unwrap().newest_valid, Some(1));
    assert_reads_source(&path, &src);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn corrupt_newest_section_payload_keeps_previous_generation() {
    let dir = temp_dir("payload");
    let src = lcg(0xDEADBEEF, 256);
    let path = two_generations(&dir, "b.ekb", &src);

    let bytes = std::fs::read(&path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    assert_eq!(scan.commits.len(), 2);
    let newest = &scan.commits[1];
    // The newest generation adds a checkpoint section; corrupt its payload.
    let section = newest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Checkpoints)
        .unwrap();
    let mut corrupted = bytes.clone();
    corrupted[section.payload_offset as usize] ^= 0xFF;
    std::fs::write(&path, &corrupted).unwrap();

    let report = recover(&path).unwrap();
    assert_eq!(report.newest_valid, Some(1));
    assert!(report.torn_tail_bytes > 0);
    assert_reads_source(&path, &src);

    let repaired = truncate_to_valid(&path).unwrap();
    assert!(repaired.repaired);
    assert_eq!(repaired.newest_valid, Some(1));
    assert_eq!(scan_generations(&path).unwrap().torn_tail_bytes, 0);
    assert_reads_source(&path, &src);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn trailing_garbage_keeps_newest_generation_and_truncates() {
    let dir = temp_dir("garbage");
    let src = lcg(0x1234ABCD, 200);
    let path = two_generations(&dir, "c.ekb", &src);

    let clean_len = std::fs::read(&path).unwrap().len();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&[0u8; 10]);
    std::fs::write(&path, &bytes).unwrap();

    let report = recover(&path).unwrap();
    assert_eq!(report.newest_valid, Some(2));
    assert_eq!(report.torn_tail_bytes, 10);
    assert_reads_source(&path, &src);

    let repaired = truncate_to_valid(&path).unwrap();
    assert!(repaired.repaired);
    assert_eq!(repaired.newest_valid, Some(2));
    assert_eq!(repaired.torn_tail_bytes, 0);
    assert_eq!(std::fs::read(&path).unwrap().len(), clean_len);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fully_corrupt_commit_is_rejected_not_ignored() {
    let dir = temp_dir("allcorrupt");
    let path = dir.join("d.ekb");
    let src = lcg(0xFEEDFACE, 128);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    let manifest = scan.commits[0]
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Manifest)
        .unwrap();
    let mut corrupted = bytes;
    corrupted[manifest.payload_offset as usize] ^= 0x01;
    std::fs::write(&path, &corrupted).unwrap();

    // No valid commit remains: the reader must reject rather than silently read.
    assert!(open(&path).is_err());
    let report = recover(&path).unwrap();
    assert_eq!(report.newest_valid, None);
    assert!(report.generations.iter().all(|g| !g.valid));

    std::fs::remove_dir_all(&dir).unwrap();
}
