//! Bounded, Silesia-derived CI court for the cross-platform matrix.
//!
//! This is the *bounded* corpora court the three CI jobs run
//! (`scripts/run-matrix.sh`): on the committed real Silesia/Calgary 4 KiB
//! prefixes in `fuzz/seeds/` it builds the `auto` body, adds sparse checkpoints,
//! and measures `byte_at` latency through the authoritative reader — asserting
//! every read is byte-exact — then prints canonical lines the runner parses:
//!
//! ```text
//! CIBENCH corpus=<name> n=<n> physical=<bytes> samples=<m> p50=<a> p99=<b> p99.9=<c> p99.99=<d>
//! CIBENCH-AGG corpora=<k> n=<N> physical=<P> e_v=<E> samples=<M> byte_at_p50_ns=… byte_at_p99_ns=… byte_at_p99_9_ns=… byte_at_p99_99_ns=…
//! ```
//!
//! It is deliberately small and allocation-bounded so it stays feasible under
//! QEMU `linux/arm64` emulation. Unlike the full standard-corpora court
//! (`examples/corpora.rs` via `scripts/run-corpora.sh`, which downloads multi-MiB
//! corpora and overwrites the committed `evidence/bench/corpora.json`), this
//! court is offline, deterministic and writes no evidence file — the matrix
//! runner owns the single `evidence/bench/ci-matrix.json`.
//!
//! On aarch64 there is no AVX2, so this court inherently exercises the scalar
//! fallbacks; with `EKB_NO_SIMD=1` it does so on x86_64 too.

use std::path::PathBuf;
use std::time::Instant;

use ekbloom::{create_auto, manifest, ManifestationProfile};

/// Random single-byte samples per corpus (bounded for QEMU).
const BYTE_SAMPLES: usize = 2000;

/// A tiny deterministic LCG step.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 17
    }
}

fn lcg(seed: u64, len: usize) -> Vec<u8> {
    let mut r = Rng(seed);
    (0..len).map(|_| (r.next() >> 16) as u8).collect()
}

fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut r = Rng(seed);
    (0..len).map(|_| b'a' + (r.next() % 4) as u8).collect()
}

/// The committed real Silesia/Calgary prefixes (sorted); a generated fallback
/// keeps the test total if the seeds dir is ever missing.
fn seeds() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/seeds");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("bin") {
                    p.file_stem().and_then(|s| s.to_str()).map(str::to_string)
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        for name in names {
            if let Ok(bytes) = std::fs::read(dir.join(format!("{name}.bin"))) {
                out.push((name, bytes));
            }
        }
    }
    if out.is_empty() {
        out.push(("generated-lcg".to_string(), lcg(0x0C1B_0001, 8192)));
        out.push(("generated-skew".to_string(), skewed(0x0C1B_0002, 8192)));
    }
    out
}

/// Nearest-rank percentile (matching `tests/common/corpora.rs`).
fn nearest_rank(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil();
    let idx = (rank as usize).saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

#[test]
fn bounded_silesia_court() {
    let corpora = seeds();
    let mut total_physical = 0u64;
    let mut total_n = 0u64;
    let mut pooled: Vec<u64> = Vec::with_capacity(corpora.len() * BYTE_SAMPLES);

    for (i, (name, src)) in corpora.iter().enumerate() {
        let n = src.len() as u64;
        assert!(n > 0, "{name}: empty corpus");
        let mut body = create_auto(src, ManifestationProfile::Root).expect("create_auto");
        manifest(&mut body, ManifestationProfile::Sparse { interval: 256 }).expect("manifest");
        let physical = body.physical_bytes();

        let mut rng = Rng(0x0C1B_0003u64 ^ ((i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
        let mut samples = Vec::with_capacity(BYTE_SAMPLES);
        for _ in 0..BYTE_SAMPLES {
            let off = rng.next() % n;
            let t0 = Instant::now();
            let got = ekbloom::byte_at(&body, off).expect("byte_at");
            samples.push(t0.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            assert_eq!(got, src[off as usize], "{name} byte mismatch at {off}");
        }
        samples.sort_unstable();
        println!(
            "CIBENCH corpus={name} n={n} physical={physical} samples={} p50={} p99={} p99.9={} p99.99={}",
            samples.len(),
            nearest_rank(&samples, 0.50),
            nearest_rank(&samples, 0.99),
            nearest_rank(&samples, 0.999),
            nearest_rank(&samples, 0.9999),
        );

        total_physical += physical;
        total_n += n;
        pooled.extend_from_slice(&samples);
    }

    pooled.sort_unstable();
    let e_v = total_physical as f64 / total_n.max(1) as f64;
    println!(
        "CIBENCH-AGG corpora={} n={} physical={} e_v={:.4} samples={} byte_at_p50_ns={} byte_at_p99_ns={} byte_at_p99_9_ns={} byte_at_p99_99_ns={}",
        corpora.len(),
        total_n,
        total_physical,
        e_v,
        pooled.len(),
        nearest_rank(&pooled, 0.50),
        nearest_rank(&pooled, 0.99),
        nearest_rank(&pooled, 0.999),
        nearest_rank(&pooled, 0.9999),
    );
}
