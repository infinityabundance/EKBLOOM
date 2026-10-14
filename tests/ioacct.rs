//! Independent validation of the central I/O accounting (`src/ioacct.rs`).
//!
//! The reader exposes `FileBody::fetched_bytes()` / `fetched_reads()` from one
//! [`ekbloom::ReadLedger`] that **every** physical read path charges. These tests
//! pin that the tile path, the bounded encoded-window path, the decoder model
//! ("dictionary") load and the section/metadata reads are all counted; that the
//! ledger agrees with an independently observed `/proc/self/io` trace; and that a
//! read path which bypassed the ledger would be caught.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ekbloom::blocks::BlockIndex;
use ekbloom::{
    create, create_machine, write_body, AccessTrace, DirectoryKind, FileBody, LoggedFile,
    ManifestationBudget, ManifestationProfile, ReadClass, ReadLedger, ReadRequest, TileSet,
    MACHINE_RANS0,
};

/// Our tests read process-wide `/proc/self/io`; serialise every test in this
/// binary so no sibling test's I/O pollutes the deltas we measure.
static SERIAL: Mutex<()> = Mutex::new(());

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("ekbloom-ioacct-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// A skewed source machine 2 compresses (`V < N`).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(match r {
            0..=69 => b'a',
            70..=91 => b'b',
            _ => b'c',
        });
    }
    out
}

/// A `.ekb` with a persisted hot tile set (4 KiB tiles) over the first 8 KiB
/// (two tiles), so a boundary-crossing read stays tile-served.
fn write_tiled(dir: &Path, name: &str, src: &[u8]) -> PathBuf {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 8192)],
        counts: vec![1000],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::Flat,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();
    let path = dir.join(name);
    write_body(&path, &body).unwrap();
    path
}

/// A file-backed block-indexed `.ekb` for machine 2 (cursor-divergent).
fn write_blocks(dir: &Path, name: &str, src: &[u8], block_bytes: u32) -> PathBuf {
    let machine = ekbloom::machine::machine_by_id(MACHINE_RANS0).unwrap();
    let core = machine.encode(src).unwrap();
    let index =
        BlockIndex::build_machine(&core, src.len() as u64, block_bytes, MACHINE_RANS0).unwrap();
    let mut body = create_machine(src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    body.blocks.push(index);
    let path = dir.join(name);
    write_body(&path, &body).unwrap();
    path
}

fn read(fb: &FileBody, offset: u64, length: u64) -> Vec<u8> {
    fb.read(ReadRequest { offset, length }).expect("read")
}

// ---------------------------------------------------------------------------
// /proc/self/io
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProcIo {
    /// Read syscalls issued by the process.
    syscr: u64,
    /// Bytes passed to `read`/`pread` by the process (page-cache independent).
    rchar: u64,
    /// Bytes actually fetched from storage (readahead-dependent).
    read_bytes: u64,
}

fn proc_io() -> Option<ProcIo> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    let mut io = ProcIo::default();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if let Ok(n) = value.trim().parse::<u64>() {
                match key {
                    "syscr" => io.syscr = n,
                    "rchar" => io.rchar = n,
                    "read_bytes" => io.read_bytes = n,
                    _ => {}
                }
            }
        }
    }
    Some(io)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Regression: a read served from a persisted tile set MUST be counted. Before
/// the central ledger this path bypassed the counters and reported 0/0.
#[test]
fn tile_reads_are_counted() {
    let _guard = SERIAL.lock().unwrap();
    let dir = temp_dir("tiles");
    let src = lcg(0x11AC_0001, 64 * 1024);
    let path = write_tiled(&dir, "a.ekb", &src);
    let fb = FileBody::open(&path).unwrap();
    assert!(fb.tiles_cover(0, 100), "the hot range must be materialised");

    // A 100-byte read entirely inside tile 0: one pread of exactly 100 bytes.
    let b0 = fb.fetched_bytes();
    let s0 = fb.fetched_reads();
    let got = read(&fb, 0, 100);
    assert_eq!(got, &src[0..100]);
    let bytes = fb.fetched_bytes() - b0;
    let syscalls = fb.fetched_reads() - s0;
    assert_ne!(bytes, 0, "the tile path bypassed the central ledger");
    assert_eq!(bytes, 100, "a tile read preads exactly the requested range");
    assert_eq!(syscalls, 1, "a range inside one tile is one pread");

    // A read crossing a tile boundary: one pread per tile, bytes == len.
    let b1 = fb.fetched_bytes();
    let s1 = fb.fetched_reads();
    let got = read(&fb, 4090, 20);
    assert_eq!(got, &src[4090..4110]);
    assert_eq!(fb.fetched_bytes() - b1, 20);
    assert_eq!(fb.fetched_reads() - s1, 2, "two tiles crossed, two preads");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A cold (non-tile) read served from a bounded encoded window is counted, and
/// the total ledger is at least the encoded window it contains.
#[test]
fn cold_encoded_window_reads_are_counted() {
    let _guard = SERIAL.lock().unwrap();
    let dir = temp_dir("window");
    let src = skewed(0x11AC_0002, 128 * 1024);
    let path = write_blocks(&dir, "w.ekb", &src, 4096);
    let fb = FileBody::open(&path).unwrap();

    let total_before = fb.fetched_bytes();
    let enc_before = fb.fetched_encoded_bytes();
    let reads_before = fb.fetched_reads();
    let got = read(&fb, 300, 4096);
    assert_eq!(got, &src[300..4396]);
    assert!(
        fb.fetched_encoded_bytes() > enc_before,
        "encoded window uncounted"
    );
    assert!(fb.fetched_reads() > reads_before, "read syscalls uncounted");
    assert!(fb.fetched_bytes() >= fb.fetched_encoded_bytes());
    // The restart record is read-path metadata the total charges on top of the
    // encoded window.
    let total = fb.fetched_bytes() - total_before;
    let enc = fb.fetched_encoded_bytes() - enc_before;
    assert_eq!(
        total - enc,
        12,
        "machine-2 restart record is a 12-byte read"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The decoder model ("dictionary") header is loaded at open and charged to the
/// ledger (as a `Dictionary` read), not silently.
#[test]
fn dictionary_load_is_counted() {
    let _guard = SERIAL.lock().unwrap();
    let dir = temp_dir("dict");
    let src = skewed(0x11AC_0003, 32 * 1024);
    let body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    let path = dir.join("d.ekb");
    write_body(&path, &body).unwrap();
    let fb = FileBody::open(&path).unwrap();
    // Machine 2 retains a prepared decoder context, so `open` read the model
    // header through the ledger.
    assert!(fb.fetched_bytes() > 0, "the model header load is uncounted");
    assert_eq!(fb.fetched_reads(), 1);
    // The header is a dictionary read, not an encoded core window.
    assert_eq!(fb.fetched_encoded_bytes(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Section/metadata reads on the read path are counted (the block restart record
/// and, via the segmented history, a section index + state read).
#[test]
fn section_and_metadata_reads_are_counted() {
    let _guard = SERIAL.lock().unwrap();
    let dir = temp_dir("meta");
    let src = skewed(0x11AC_0004, 64 * 1024);
    let path = write_blocks(&dir, "m.ekb", &src, 1024);
    let fb = FileBody::open(&path).unwrap();

    // Every block read performs one metadata pread (restart) plus one payload
    // pread (the encoded window), so the read-call count is two.
    let s0 = fb.fetched_reads();
    let b0 = fb.fetched_bytes();
    let _ = read(&fb, 500, 4096);
    assert_eq!(fb.fetched_reads() - s0, 2, "restart + window = two preads");
    assert!(fb.fetched_bytes() - b0 >= 12);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The ledger agrees with an independently observed `/proc/self/io` trace around
/// a read sequence. `syscr` and `rchar` are page-cache independent, so they track
/// the ledger's syscall and byte counts directly.
#[test]
fn ledger_agrees_with_proc_self_io() {
    let _guard = SERIAL.lock().unwrap();
    let Some(_) = proc_io() else {
        eprintln!("skipping: /proc/self/io is unavailable on this platform");
        return;
    };
    let dir = temp_dir("proc");
    let src = skewed(0x11AC_0005, 256 * 1024);
    let path = write_blocks(&dir, "p.ekb", &src, 4096);
    let fb = FileBody::open(&path).unwrap();

    // Warm nothing: read a set of distinct 4 KiB windows.
    let mut offsets = Vec::new();
    let mut off = 0u64;
    while off + 4096 <= src.len() as u64 {
        offsets.push(off);
        off += 8192;
    }

    let io0 = proc_io().unwrap();
    let b0 = fb.fetched_bytes();
    let s0 = fb.fetched_reads();
    for &o in &offsets {
        let got = read(&fb, o, 4096);
        assert_eq!(got, &src[o as usize..(o + 4096) as usize]);
    }
    let io1 = proc_io().unwrap();

    let ledger_bytes = fb.fetched_bytes() - b0;
    let ledger_syscalls = fb.fetched_reads() - s0;
    let proc_syscr = io1.syscr - io0.syscr;
    let proc_rchar = io1.rchar - io0.rchar;

    assert!(ledger_syscalls > 0);
    assert!(ledger_bytes > 0);

    // Tolerance: the process issues a handful of incidental reads (the harness,
    // stdio, opening files) inside the window. Our own preads are counted in
    // *both* the ledger and the kernel's `syscr`/`rchar`, so the kernel totals are
    // >= the ledger totals and exceed them by at most the incidental amount.
    const SYSCALL_SLACK: u64 = 8;
    const BYTE_SLACK: u64 = 4096;
    assert!(
        proc_syscr >= ledger_syscalls,
        "kernel syscr {proc_syscr} < ledger calls {ledger_syscalls}"
    );
    assert!(
        proc_syscr - ledger_syscalls <= SYSCALL_SLACK,
        "kernel syscr {proc_syscr} disagrees with ledger calls {ledger_syscalls} beyond slack"
    );
    assert!(
        proc_rchar >= ledger_bytes,
        "kernel rchar {proc_rchar} < ledger bytes {ledger_bytes}"
    );
    assert!(
        proc_rchar - ledger_bytes <= BYTE_SLACK,
        "kernel rchar {proc_rchar} disagrees with ledger bytes {ledger_bytes} beyond slack"
    );
    eprintln!(
        "ioacct agreement: ledger {ledger_bytes} B / {ledger_syscalls} syscalls; \
         proc rchar {proc_rchar} B / syscr {proc_syscr}; storage read_bytes delta {}",
        io1.read_bytes.saturating_sub(io0.read_bytes)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression: a read path that bypasses the ledger would be caught by the
/// independent trace. A raw, uncharged `pread` is visible to `/proc/self/io`
/// (`syscr` rises) while the ledger stays at zero — exactly the mismatch the
/// `/proc/self/io` agreement test detects.
#[test]
fn a_bypassed_read_is_caught_by_the_independent_trace() {
    let _guard = SERIAL.lock().unwrap();
    let dir = temp_dir("bypass");
    let src = lcg(0x11AC_0006, 4096);
    let path = dir.join("b.bin");
    std::fs::write(&path, &src).unwrap();
    let file = std::fs::File::open(&path).unwrap();

    let ledger = ReadLedger::new();
    let mut buf = [0u8; 64];
    let before = proc_io().map(|p| p.syscr);
    // A deliberately-uncounted physical read: raw `pread`, no ledger charge.
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(&mut buf, 0).unwrap();
    }
    assert_eq!(buf[..], src[..64]);
    let after = proc_io().map(|p| p.syscr);
    if let (Some(b), Some(a)) = (before, after) {
        assert!(
            a > b,
            "the independent trace must observe the raw read (syscr {b} -> {a})"
        );
    }
    assert_eq!(ledger.bytes(), 0, "a bypass leaves the ledger at zero");

    // Charging the same read through the ledger restores agreement.
    let logged = LoggedFile::new(&file, src.len() as u64, &ledger);
    logged
        .read_exact_at(&mut buf, 0, ReadClass::Payload)
        .unwrap();
    assert_eq!(ledger.bytes(), 64);
    assert_eq!(ledger.read_calls(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
