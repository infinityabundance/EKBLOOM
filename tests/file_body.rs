//! Integration tests: the fully-safe, file-backed [`FileBody`] is byte-exact
//! against the authoritative in-memory [`ekbloom::read`] / [`ekbloom::read_file`]
//! for every manifestation profile, and never panics on corrupt input.
//!
//! `FileBody` must be able to answer a read without holding the whole body or
//! history in RAM, so these tests deliberately exercise the full range of backing
//! layouts: `root` (machine replay), `sparse` (checkpoint restart), `full`
//! (history projection), the transition blooms (fallback), and the path blooms
//! (contiguous `hot`-array reads), including an object larger than a few MiB.

use ekbloom::{
    create, open, read, read_file, sweep_profiles, write_body, FileBody, ManifestationProfile,
    ReadRequest,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Deterministic corpus.
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

/// A freshly-created temp directory, removed on drop. Unique per call.
struct TempDir {
    path: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ekbloom-filebody-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Build and write a `.ekb` file for `src` under `dir`.
fn write_ekb(dir: &TempDir, name: &str, src: &[u8], profile: ManifestationProfile) -> PathBuf {
    let body = create(src, profile).expect("build body");
    let path = dir.join(name);
    write_body(&path, &body).expect("write body");
    path
}

/// In-bounds `(offset, length)` pairs, including edge cases and random interior
/// ranges.
fn ranges_for(n: u64, count: usize, seed: u64) -> Vec<(u64, u64)> {
    let mut ranges = vec![
        (0, 0),
        (0, n),
        (0, 1),
        (1, 17),
        (n / 3, 4096),
        (n.saturating_sub(4096), 4096.min(n)),
        (n.saturating_sub(1), 1.min(n)),
        (n, 0),
    ];
    let mut s = seed;
    for _ in 0..count {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let off = if n == 0 { 0 } else { s % (n + 1) };
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let max = n - off;
        let len = if max == 0 { 0 } else { s % (max + 1) };
        ranges.push((off, len));
    }
    ranges
}

fn req(offset: u64, length: u64) -> ReadRequest {
    ReadRequest { offset, length }
}

#[test]
fn file_body_read_matches_source_and_reader_for_every_profile() {
    let dir = TempDir::new("profiles");
    let src = lcg(0xABCD_1234_5678_9F01, 20_000);
    let n = src.len() as u64;
    let ranges = ranges_for(n, 128, 0xDEAD_BEEF_CAFE);

    for (i, profile) in sweep_profiles().into_iter().enumerate() {
        let path = write_ekb(&dir, &format!("p{i}.ekb"), &src, profile);
        let label = profile.label();

        let file_body = FileBody::open(&path).expect("open file body");
        assert_eq!(file_body.logical_len(), n, "logical_len for {label}");
        assert_eq!(
            file_body.physical_len(),
            std::fs::metadata(&path).unwrap().len(),
            "physical_len for {label}"
        );

        // The in-memory body is the authoritative reference for byte equality.
        let body = open(&path).expect("open body");

        for &(off, len) in &ranges {
            let got = file_body.read(req(off, len)).expect("file body read");
            assert_eq!(
                got,
                src[off as usize..(off + len) as usize].to_vec(),
                "source mismatch {label} ({off},{len})"
            );
            assert_eq!(
                got,
                read(&body, req(off, len)).unwrap(),
                "reader mismatch {label} ({off},{len})"
            );
            assert_eq!(
                got,
                read_file(&path, req(off, len)).unwrap(),
                "read_file mismatch {label} ({off},{len})"
            );
        }

        // Full read equals the source, and `read_into` appends without clearing.
        assert_eq!(file_body.read(req(0, n)).unwrap(), src, "full read {label}");
        let mut out = b"prefix".to_vec();
        file_body.read_into(req(0, 8), &mut out).unwrap();
        assert!(
            out.starts_with(b"prefix"),
            "read_into cleared output {label}"
        );
        assert_eq!(&out[6..], &src[..8], "read_into bytes {label}");

        // `inspect` equals the report the in-memory body would produce.
        assert_eq!(
            file_body.inspect().unwrap(),
            ekbloom::inspect(&body),
            "inspect mismatch {label}"
        );
    }
}

#[test]
fn file_body_bounds_match_reader() {
    let dir = TempDir::new("bounds");
    let src = lcg(7, 300);
    let n = src.len() as u64;
    let path = write_ekb(
        &dir,
        "b.ekb",
        &src,
        ManifestationProfile::Sparse { interval: 64 },
    );
    let file_body = FileBody::open(&path).unwrap();

    // Past-the-end, overflow, and zero-length edge cases mirror `reader::read`.
    assert!(file_body.read(req(n, 1)).is_err());
    assert!(file_body.read(req(n + 1, 0)).is_err());
    assert!(file_body.read(req(u64::MAX, 2)).is_err());
    assert_eq!(file_body.read(req(n, 0)).unwrap(), Vec::<u8>::new());
    assert_eq!(file_body.read(req(0, 0)).unwrap(), Vec::<u8>::new());
    assert_eq!(
        file_body.read(req(n - 1, 1)).unwrap(),
        vec![src[src.len() - 1]]
    );

    // The authoritative reader rejects exactly the same requests.
    let body = open(&path).unwrap();
    assert!(read(&body, req(n, 1)).is_err());
    assert!(read(&body, req(u64::MAX, 2)).is_err());
    assert_eq!(read(&body, req(n, 0)).unwrap(), Vec::<u8>::new());
}

#[test]
fn large_interior_range_reads_correctly() {
    let dir = TempDir::new("large");
    // Larger than a few MiB, and deliberately not a multiple of the block depth.
    let n = 4 * 1024 * 1024 + 123;
    let src = lcg(0x5151_2626_3737_4848, n);
    let path = write_ekb(
        &dir,
        "big.ekb",
        &src,
        ManifestationProfile::Path { depth: 4 },
    );
    let file_body = FileBody::open(&path).unwrap();
    assert_eq!(file_body.logical_len(), n as u64);

    let (off, len) = (2_000_003u64, 4096u64);
    let got = file_body.read(req(off, len)).unwrap();
    assert_eq!(got, src[off as usize..(off + len) as usize].to_vec());

    // Byte-for-byte equality with the authoritative file reader.
    assert_eq!(got, read_file(&path, req(off, len)).unwrap());

    // A range straddling the final partial block.
    let (off, len) = (n as u64 - 10_000, 10_000);
    assert_eq!(
        file_body.read(req(off, len)).unwrap(),
        src[off as usize..(off + len) as usize].to_vec()
    );
}

#[test]
fn corrupt_and_truncated_files_error_cleanly() {
    let dir = TempDir::new("corrupt");
    let src = lcg(99, 4096);
    let path = write_ekb(
        &dir,
        "c.ekb",
        &src,
        ManifestationProfile::Sparse { interval: 1024 },
    );
    let good = std::fs::read(&path).unwrap();

    // A well-formed file opens.
    assert!(FileBody::open(&path).is_ok());

    // Bad magic.
    let mut bad_magic = good.clone();
    bad_magic[0] ^= 0xFF;
    let p = dir.join("badmagic.ekb");
    std::fs::write(&p, &bad_magic).unwrap();
    assert!(FileBody::open(&p).is_err());

    // A single flipped core byte is caught by the core-hash linkage check.
    let mut flipped = good.clone();
    let core_off = ekbloom::format::CORE_OFFSET as usize;
    flipped[core_off] ^= 0x01;
    let p = dir.join("flip.ekb");
    std::fs::write(&p, &flipped).unwrap();
    assert!(FileBody::open(&p).is_err());

    // Truncated to half (removes the commit footer).
    let p = dir.join("trunc.ekb");
    std::fs::write(&p, &good[..good.len() / 2]).unwrap();
    assert!(FileBody::open(&p).is_err());

    // Empty file and a missing file.
    let p = dir.join("empty.ekb");
    std::fs::write(&p, b"").unwrap();
    assert!(FileBody::open(&p).is_err());
    assert!(FileBody::open(&dir.join("missing.ekb")).is_err());
}
