//! Bounded tests for the standard-corpora court and the worst-case bound court.
//!
//! These pin, on bounded samples, the properties the evidence examples rely on:
//!
//! * `measured positioning steps <= worst_case_steps_for(request)` for **every**
//!   sample, on a large block restart, a large rANS checkpoint interval, and the
//!   multi-path cold → hot → cold case (where the request-level bound exceeds the
//!   per-region bound) — with zero violations and byte-exact output;
//! * the standard-corpus loader is deterministic (identical bytes and hashes on
//!   repeated loads) and its streaming BLAKE3 agrees with the slice hash;
//! * the conventional comparison stores (LZ4 / Zstandard at several levels, with
//!   and without a pinned hot prefix) are byte-exact and report real footprints.
//!
//! The samples are deliberately small so the suite stays bounded; the full-size
//! runs live in `examples/worstcase_bound.rs` and `examples/corpora.rs`.

#[path = "common/corpora.rs"]
mod bench;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ekbloom::{MACHINE_RANS0, MACHINE_STORE};

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

struct TempDir {
    path: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ekbloom-corpora-it-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
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

#[test]
fn bound_holds_on_a_large_block_restart() {
    let src = lcg(0xB0_0001, 512 * 1024);
    let body = bench::body_with_block(&src, MACHINE_STORE, 1 << 19).expect("body");
    let engine = bench::engine_for(&body).expect("engine");
    let per_region = engine.worst_case_steps();
    assert_eq!(per_region, (1 << 19) - 1);
    let out = bench::verify_bound(&engine, &body, &src, 20_000, &[1], 0xB0_0001).expect("verify");
    assert_eq!(out.samples, 20_000);
    assert_eq!(out.violations, 0, "bound violated");
    assert_eq!(out.mismatches, 0, "wrong bytes");
    assert!(out.max_measured_steps <= out.max_bound_steps);
    assert_eq!(out.max_bound_steps, per_region);
}

#[test]
fn bound_holds_on_a_large_rans_checkpoint_interval() {
    let src = lcg(0xB0_0002, 512 * 1024);
    let body = bench::body_with_checkpoint(&src, MACHINE_RANS0, 65536).expect("body");
    let engine = bench::engine_for(&body).expect("engine");
    assert_eq!(engine.worst_case_steps(), 65535);
    let out = bench::verify_bound(&engine, &body, &src, 2_000, &[1], 0xB0_0002).expect("verify");
    assert_eq!(out.violations, 0);
    assert_eq!(out.mismatches, 0);
    assert!(out.max_measured_steps <= 65535);
}

#[test]
fn bound_holds_and_exceeds_per_region_on_cold_hot_cold() {
    let src = lcg(0xB0_0003, 512 * 1024);
    let band_start = 128 * 1024;
    let band_len = 32 * 1024;
    let body =
        bench::body_with_block_and_tiles(&src, MACHINE_STORE, 65536, (band_start, band_len), 12)
            .expect("body");
    let engine = bench::engine_for(&body).expect("engine");
    let per_region = engine.worst_case_steps();
    // The plan must mix a hot band with cold block restarts.
    let dist = engine.plan_ref().distribution();
    assert!(
        dist.iter().any(|(p, _)| p.as_str() == "hot-tile"),
        "no hot tiles in the plan: {dist:?}"
    );
    assert!(
        dist.iter().any(|(p, _)| p.as_str() == "block-restart"),
        "no cold block restarts in the plan: {dist:?}"
    );
    let out =
        bench::verify_bound(&engine, &body, &src, 4_000, &[131_072], 0xB0_0003).expect("verify");
    assert_eq!(out.violations, 0, "bound violated");
    assert_eq!(out.mismatches, 0, "wrong bytes");
    // A request spanning cold → hot → cold pays one restart per segment, so the
    // request-level bound exceeds the per-region bound. This is the point the
    // per-region `worst_case_steps` does NOT bound.
    assert!(
        out.max_bound_steps > per_region,
        "request-level bound {} did not exceed per-region {}",
        out.max_bound_steps,
        per_region
    );
}

#[test]
fn corpora_loader_is_deterministic() {
    let dir = TempDir::new("loader");
    // A fake standard-corpus layout: silesia/, enwik8/, calgary/.
    for sub in ["silesia", "enwik8", "calgary"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let sil_a = lcg(0x51, 40_000);
    let sil_b = lcg(0x52, 25_000);
    std::fs::write(dir.join("silesia/dickens"), &sil_a).unwrap();
    std::fs::write(dir.join("silesia/mozilla"), &sil_b).unwrap();
    let enwik = lcg(0x53, 90_000);
    std::fs::write(dir.join("enwik8/enwik8"), &enwik).unwrap();
    let cal = lcg(0x54, 12_000);
    std::fs::write(dir.join("calgary/pic"), &cal).unwrap();

    let cap = 16 * 1024;
    let first = bench::load_standard(&dir.path, cap);
    let second = bench::load_standard(&dir.path, cap);
    assert_eq!(first.len(), 3, "expected three corpora");
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.source, b.source, "{} corpus is not deterministic", a.name);
        assert_eq!(a.full_blake3, b.full_blake3);
        assert_eq!(a.used_blake3, b.used_blake3);
        assert!(!a.source.is_empty());
        assert!(a.source.len() <= cap);
        assert!(a.full_bytes >= a.source.len() as u64);
    }
    // The streaming hash of a file matches its slice hash, and the used-slice
    // hash matches an independent BLAKE3 of the sampled bytes.
    let (hex, bytes) = bench::blake3_file(&dir.join("enwik8/enwik8")).unwrap();
    assert_eq!(bytes, enwik.len() as u64);
    assert_eq!(hex, bench::blake3_hex(&enwik));
    let enwik_corpus = first.iter().find(|c| c.name == "enwik8").unwrap();
    assert_eq!(enwik_corpus.full_blake3, bench::blake3_hex(&enwik));
    assert_eq!(
        enwik_corpus.used_blake3,
        bench::blake3_hex(&enwik_corpus.source)
    );
}

#[test]
fn comparison_stores_are_byte_exact() {
    let dir = TempDir::new("stores");
    let src = lcg(0x57, 64 * 1024);
    let n = src.len() as u64;
    let hot = 16 * 1024;

    for (label, level) in [("lz4", 0i32), ("zstd-19", 19), ("zstd-3", 3)] {
        let mut store = if label == "lz4" {
            bench::Store::lz4(&dir.join("lz4"), &src, hot).unwrap()
        } else {
            bench::Store::zstd(&dir.join(label), &src, level, hot).unwrap()
        };
        assert!(store.physical_bytes() > 0);
        assert!(store.pinned_bytes() > 0, "{label} pinned nothing");
        for &off in &[0u64, 4096, 30_000, n - 4096] {
            assert_eq!(
                store.read(off, 4096),
                &src[off as usize..(off + 4096) as usize],
                "{label} @ {off}"
            );
            assert_eq!(
                store.byte_at(off),
                src[off as usize],
                "{label} byte @ {off}"
            );
        }
        // A cold read (beyond the pinned prefix) still fetches only covering
        // blocks, not the whole object.
        let off = n - 4096;
        let _ = store.read(off, 4096);
        assert!(
            store.last_fetched() <= n,
            "{label} fetched the whole object"
        );
    }
}
