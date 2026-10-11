//! Tests for `src/mapped.rs`: the mapped read surface must be byte-exact against
//! its source, must fail cleanly on a missing/empty (truncated) file, must give
//! identical bytes in huge-page mode, must keep reading an old generation after
//! the path is atomically replaced or unlinked (retain-then-swap), and the
//! crate's `unsafe` must stay confined to exactly three sanctioned modules.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ekbloom::{MapMode, MappedSurface};

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
        "ekbloom-mmap-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn offsets(seed: u64, max_start: u64, n: usize) -> Vec<u64> {
    let mut s = seed;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(if max_start == 0 {
            0
        } else {
            (s >> 17) % max_start
        });
    }
    out
}

#[test]
fn reads_are_byte_exact_full_random_and_range() {
    let dir = temp_dir("exact");
    let src = lcg(0xA1B2_C3D4, 1 << 20);
    let path = write(&dir, "surface.bin", &src);
    let m = MappedSurface::open(&path).unwrap();

    assert_eq!(m.len(), src.len() as u64);
    assert!(!m.is_empty());
    assert!(m.physical_bytes() >= src.len() as u64);
    assert_eq!(m.mode(), MapMode::Plain);

    // Full read.
    assert_eq!(m.read(0, m.len()).unwrap(), src);

    // Random ranges (including boundary-crossing and single bytes).
    for &off in &offsets(9, (src.len() - 65536) as u64, 512) {
        for &len in &[1u64, 7, 4096, 65536] {
            let got = m.read(off, len).unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
        }
        // read_into appends without clearing.
        let mut out = vec![0xEEu8];
        m.read_into(off, 3, &mut out).unwrap();
        assert_eq!(out[0], 0xEE);
        assert_eq!(&out[1..], &src[off as usize..off as usize + 3]);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn boundary_and_zero_length_reads() {
    let dir = temp_dir("boundary");
    let src = lcg(0x0F0F_0F0F, 100_000);
    let path = write(&dir, "surface.bin", &src);
    let m = MappedSurface::open(&path).unwrap();
    let n = m.len();

    // Zero-length reads succeed and append nothing, including at the end.
    assert!(m.read(0, 0).unwrap().is_empty());
    assert!(m.read(n, 0).unwrap().is_empty());
    let mut out = Vec::new();
    m.read_into(n, 0, &mut out).unwrap();
    assert!(out.is_empty());

    // Last valid byte and a range ending exactly at the end.
    assert_eq!(m.read(n - 1, 1).unwrap(), vec![src[src.len() - 1]]);
    assert_eq!(m.read(n - 100, 100).unwrap(), &src[src.len() - 100..]);

    // Past the end is a clean OutOfBounds, not a panic / over-read.
    let errors: Vec<ekbloom::EkbloomError> = vec![
        m.read(n, 1).unwrap_err(),
        m.read(n - 1, 2).unwrap_err(),
        m.read(n + 1, 0).unwrap_err(),
    ];
    for e in errors {
        assert!(
            matches!(e, ekbloom::EkbloomError::OutOfBounds { .. }),
            "got {e}"
        );
    }
    // An overflowing range is Overflow, not OutOfBounds.
    let e = m.read(u64::MAX, 2).unwrap_err();
    assert!(
        matches!(e, ekbloom::EkbloomError::Overflow { .. }),
        "got {e}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_and_truncated_files_error_cleanly() {
    let dir = temp_dir("bad");

    // Missing file.
    let missing = dir.join("does-not-exist.bin");
    assert!(MappedSurface::open(&missing).is_err());

    // Empty (truncated) file: no surface to map.
    let empty = write(&dir, "empty.bin", &[]);
    let e = MappedSurface::open(&empty).unwrap_err();
    assert!(
        matches!(e, ekbloom::EkbloomError::InvalidArgument { .. }),
        "got {e}"
    );

    // A directory is not a regular file.
    let subdir = dir.join("d");
    std::fs::create_dir_all(&subdir).unwrap();
    assert!(MappedSurface::open(&subdir).is_err());

    // A file truncated to zero *after* being written is rejected at open too.
    let p = write(&dir, "then-truncated.bin", &lcg(1, 8192));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(MappedSurface::open(&p).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn huge_page_mode_is_byte_identical_to_plain() {
    let dir = temp_dir("huge");
    let src = lcg(0xCAFE_BABE, 1 << 20);
    let path = write(&dir, "surface.bin", &src);

    let plain = MappedSurface::open_with(&path, MapMode::Plain).unwrap();
    let huge = MappedSurface::open_with(&path, MapMode::HugePage).unwrap();
    assert_eq!(huge.mode(), MapMode::HugePage);
    assert_eq!(plain.len(), huge.len());

    for &off in &offsets(3, (src.len() - 65536) as u64, 256) {
        for &len in &[1u64, 4096, 65536] {
            assert_eq!(
                huge.read(off, len).unwrap(),
                plain.read(off, len).unwrap(),
                "huge-page bytes differ at {off}+{len}"
            );
            assert_eq!(
                huge.read(off, len).unwrap(),
                &src[off as usize..(off + len) as usize]
            );
        }
    }
    // The huge-page report is always well-formed (may report zero delivery).
    let r = huge.huge_page_report();
    assert!(r.huge_bytes() <= huge.physical_bytes());

    // Prefault variants agree too.
    for mode in [MapMode::Populate, MapMode::PopulateRead] {
        let m = MappedSurface::open_with(&path, mode).unwrap();
        assert_eq!(m.read(0, 65536).unwrap(), &src[..65536]);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn old_generation_is_retained_while_a_mapping_is_alive() {
    let dir = temp_dir("retain");
    let gen1 = lcg(0x1111_1111, 256 * 1024);
    let gen2 = lcg(0x2222_2222, 512 * 1024);
    let path = write(&dir, "generation.bin", &gen1);

    // Map generation 1, then publish generation 2 by rename-then-retain: write a
    // *new* file and atomically rename it over the path. The mapped inode is
    // never truncated or rewritten.
    let m1 = MappedSurface::open(&path).unwrap();
    let tmp = write(&dir, "generation.next", &gen2);
    std::fs::rename(&tmp, &path).unwrap();

    // The mapping still reads the old generation, byte-for-byte.
    assert_eq!(m1.len(), gen1.len() as u64);
    assert_eq!(m1.read(0, m1.len()).unwrap(), gen1);

    // A fresh surface over the same path sees the new generation.
    let m2 = MappedSurface::open(&path).unwrap();
    assert_eq!(m2.read(0, m2.len()).unwrap(), gen2);

    // Unlinking the path does not invalidate a live mapping either.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(m1.read(0, 4096).unwrap(), &gen1[..4096]);
    assert_eq!(m2.read(0, 4096).unwrap(), &gen2[..4096]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn concurrent_readers_are_never_torn_across_generation_publication() {
    let dir = temp_dir("stress");
    let gen1 = lcg(0xDEAD_BEEF, 1 << 20);
    let path = write(&dir, "live.bin", &gen1);
    let surface = Arc::new(MappedSurface::open(&path).unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let s = Arc::clone(&surface);
        let expected = gen1.clone();
        let stop = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            let mut checks = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let off = (checks * 7919) % (expected.len() as u64 - 4096);
                let got = s.read(off, 4096).unwrap();
                assert_eq!(got, &expected[off as usize..off as usize + 4096]);
                checks += 1;
            }
            checks
        }));
    }

    // Publish many new generations by rename-then-retain; never touch the mapped
    // inode. Readers must never observe the new bytes through the old mapping.
    for i in 0..64u64 {
        let fresh = lcg(0xABCD_0000 + i, 1 << 20);
        let tmp = write(&dir, "live.next", &fresh);
        std::fs::rename(&tmp, &path).unwrap();
    }
    stop.store(true, Ordering::Relaxed);

    let total: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0, "readers made no progress");

    // The original mapping still yields the original bytes.
    assert_eq!(surface.read(0, 1 << 20).unwrap(), gen1);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// The crate's `unsafe` must live in exactly the three sanctioned modules:
/// `simd.rs`, `async_io.rs`, and the new `mapped.rs`.
#[test]
fn unsafe_is_confined_to_three_sanctioned_modules() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found: Vec<String> = Vec::new();
    visit(&src, &mut found);
    found.sort();
    found.dedup();
    assert_eq!(
        found,
        vec![
            "async_io.rs".to_string(),
            "mapped.rs".to_string(),
            "simd.rs".to_string()
        ],
        "unsafe code found in unexpected module(s): {found:?}"
    );
}

fn visit(dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            visit(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            if has_unsafe_code(&text) {
                out.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
}

/// True when `text` contains the `unsafe` token on a non-comment line.
fn has_unsafe_code(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if let Some(idx) = trimmed.find("unsafe") {
            let before_ok = trimmed[..idx]
                .chars()
                .last()
                .map(|c| !c.is_alphanumeric() && c != '_')
                .unwrap_or(true);
            let after = &trimmed[idx + "unsafe".len()..];
            let after_ok = after
                .chars()
                .next()
                .map(|c| !c.is_alphanumeric() && c != '_')
                .unwrap_or(true);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}
