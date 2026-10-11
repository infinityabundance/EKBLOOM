//! Integration tests for the persistent, bounded-memory [`HistoryStore`].
//!
//! These exercise streaming construction across many lengths (empty, tiny,
//! segment-boundary and non-multiple-of-segment-size cases, and a few MiB),
//! byte-exact agreement with the in-RAM [`ekbloom::History`] for **all** logical
//! positions on small cases and a sample on larger ones, `snapshot_at` cursor
//! semantics, sampled recompute-and-compare verification, clean errors on
//! corrupt/truncated segments, bounded peak memory, and the `HISTORY_SEGMENTED`
//! `.ekb` embedding/read path.

use ekbloom::{
    current_rss_bytes, open, peak_rss_bytes, read, write_history_segmented, FileBody, History,
    HistoryStore, OrMachine, ReadRequest, StoreConfig, NL2,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const CHILD_ENV: &str = "EKBLOOM_HS_MEM_CHILD";

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ekbloom-hs-it-{}-{}-{}",
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

/// A real (non-procedural) core adapter over a concrete byte slice.
fn real_core(src: &[u8]) -> Vec<u8> {
    NL2.encode(src).expect("encode source")
}

#[test]
fn matches_history_for_all_positions_many_lengths() {
    let lengths = [
        0usize, 1, 2, 3, 7, 15, 16, 17, 31, 32, 33, 100, 255, 256, 257, 1000,
    ];
    // `segment_bytes` values force multi-segment and ragged final segments.
    for &seg in &[4u32, 12, 64, 100, 1024] {
        for &n in &lengths {
            let src = lcg(0x51F0_0000 ^ n as u64, n);
            let core = real_core(&src);
            let hist = History::build(&core, n as u64, 1).unwrap();

            let dir = TempDir::new("all");
            let store = HistoryStore::build(
                &dir.join("segs"),
                |p| core[p as usize],
                n as u64,
                StoreConfig { segment_bytes: seg },
                4,
            )
            .unwrap();

            assert_eq!(store.logical_len(), n as u64);
            assert_eq!(store.state_count(), n as u64 + 1);
            assert_eq!(store.physical_bytes(), 4 * (n as u64 + 1));
            for i in 0..=(n as u64) {
                assert_eq!(
                    store.state_at(i).unwrap(),
                    hist.states[i as usize],
                    "seg={seg} n={n} i={i}"
                );
                // `snapshot_at` cursor is the logical position itself.
                assert_eq!(store.snapshot_at(i).unwrap().cursor, i);
            }
            // Full sampled verification passes, one position at a time.
            assert_eq!(
                store.verify_sample(|p| core[p as usize], 1).unwrap(),
                n as u64 + 1
            );
        }
    }
}

#[test]
fn few_mib_sampled_positions_and_checkpoints() {
    let n = 3 * 1024 * 1024 + 123; // ~3 MiB, not a segment multiple
    let src = lcg(0xDEAD_BEEF_0BAD_F00D, n);
    let core = real_core(&src);
    let dir = TempDir::new("fewmib");
    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        n as u64,
        StoreConfig {
            segment_bytes: 1024 * 1024,
        },
        65536,
    )
    .unwrap();

    assert_eq!(store.physical_bytes(), 4 * (n as u64 + 1));
    assert!(store.segment_count() > 3);

    // Sample the projection across the whole range.
    let mut i = 0u64;
    while i <= n as u64 {
        assert_eq!(store.state_at(i).unwrap(), expected_state(&src, i));
        i += 9973;
    }
    assert_eq!(
        store.state_at(n as u64).unwrap(),
        expected_state(&src, n as u64)
    );

    // Checkpoints restore exactly to `snapshot_at`, and replay is bounded.
    assert_eq!(store.checkpoints().max_replay_steps(), 65535);
    for &i in &[0u64, 1, 65535, 65536, 65537, (n as u64) / 2, n as u64] {
        assert_eq!(
            store.checkpoints().restore(&core, i).unwrap(),
            store.snapshot_at(i).unwrap()
        );
    }

    // Sampled recompute-and-compare passes and checks the expected count
    // (the terminal position is always checked, plus the arithmetic grid).
    let stride = 1 << 16;
    let count = store.verify_sample(|p| core[p as usize], stride).unwrap();
    let mut expected = n as u64 / stride + 1;
    if n as u64 % stride != 0 {
        expected += 1;
    }
    assert_eq!(count, expected);
}

/// Independent reference for machine 1: `x_i` is the last two revealed symbols.
fn expected_state(src: &[u8], i: u64) -> u32 {
    match i {
        0 => 0,
        1 => u32::from(src[0]),
        _ => {
            let a = i as usize;
            (u32::from(src[a - 2]) << 8) | u32::from(src[a - 1])
        }
    }
}

#[test]
fn corrupt_and_truncated_segments_error_cleanly() {
    let src = lcg(5, 300);
    let core = real_core(&src);
    let dir = TempDir::new("corrupt");
    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        src.len() as u64,
        StoreConfig { segment_bytes: 64 },
        4,
    )
    .unwrap();

    let sps = store.states_per_segment();
    assert!(store.state_at(sps).is_ok());

    // Truncate segment 1 so its last state lies past the new end.
    let seg1 = dir.join("segs").join("seg-00000001.bin");
    let data = std::fs::read(&seg1).unwrap();
    std::fs::write(&seg1, &data[..data.len() - 4]).unwrap();
    assert!(store.state_at(sps * 2 - 1).is_err());

    // A missing referenced segment is a clean error.
    std::fs::remove_file(dir.join("segs").join("seg-00000000.bin")).unwrap();
    assert!(store.state_at(0).is_err());

    // Out-of-bounds positions are rejected, not panicked.
    assert!(store.state_at(store.logical_len() + 1).is_err());
}

#[test]
fn ekb_segmented_embedding_roundtrip() {
    let src = lcg(0x0E4B_0000_0000_0001, 4096);
    let core = real_core(&src);
    let n = src.len() as u64;
    let dir = TempDir::new("ekb");

    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        n,
        StoreConfig { segment_bytes: 128 },
        16,
    )
    .unwrap();

    let path = dir.join("seg.ekb");
    write_history_segmented(&path, &core, &store).unwrap();

    // File-backed, streaming access.
    let file_body = FileBody::open(&path).unwrap();
    assert_eq!(file_body.segmented_logical_len(), Some(n));
    assert_eq!(file_body.segmented_checkpoint_interval(), Some(16));
    for i in 0..=n {
        assert_eq!(
            file_body.segmented_state_at(i).unwrap(),
            store.state_at(i).unwrap(),
            "i={i}"
        );
    }
    assert!(file_body.segmented_state_at(n + 1).is_err());

    // The file remains a normal, machine-readable `.ekb`.
    let req = ReadRequest {
        offset: 0,
        length: n,
    };
    assert_eq!(file_body.read(req).unwrap(), src);
    let body = open(&path).unwrap();
    assert_eq!(body.logical_len, n);
    assert_eq!(read(&body, req).unwrap(), src);

    // A file with no segmented section reports its absence cleanly.
    let plain = dir.join("plain.ekb");
    let plain_body = ekbloom::create(&src, ekbloom::ManifestationProfile::Root).unwrap();
    ekbloom::write_body(&plain, &plain_body).unwrap();
    let fb = FileBody::open(&plain).unwrap();
    assert_eq!(fb.segmented_logical_len(), None);
    assert!(fb.segmented_state_at(0).is_err());
}

#[test]
fn streaming_build_keeps_peak_memory_bounded() {
    // Peak RSS is process-wide, so run the heavy build in a freshly exec'd child
    // (via re-invoking just the helper test) where parallel tests cannot pollute
    // it. The child asserts its own bound.
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let exe = std::env::current_exe().expect("current test binary");
    let status = std::process::Command::new(exe)
        .args(["--exact", "streaming_build_child_helper", "--nocapture"])
        .env(CHILD_ENV, "1")
        .status()
        .expect("spawn bounded-memory child");
    assert!(status.success(), "bounded-memory child failed");
}

#[test]
fn streaming_build_child_helper() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    const N: u64 = 16 * 1024 * 1024; // 64 MiB projection, far above the bound.
    let dir = TempDir::new("memchild");
    let before = current_rss_bytes();

    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| (p as u8) ^ 0x5A,
        N,
        StoreConfig {
            segment_bytes: 1024 * 1024,
        },
        65536,
    )
    .unwrap();

    let peak = peak_rss_bytes();
    assert_eq!(store.physical_bytes(), 4 * (N + 1));
    assert!(
        peak < 32 * 1024 * 1024,
        "peak RSS {peak} B exceeded 32 MiB for a {} B projection (before={before})",
        4 * (N + 1)
    );
    // A couple of exact spot checks.
    assert_eq!(store.state_at(0).unwrap(), 0);
    assert_eq!(store.snapshot_at(N).unwrap().cursor, N);
}
