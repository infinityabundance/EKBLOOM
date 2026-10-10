//! Run the manifestation sweep over a deterministic corpus and write evidence.
//!
//! Writes:
//! - `evidence/bench/sweep.json` — the measured speed-vs-manifested-space
//!   frontier (one point per profile),
//! - `evidence/bench/cpu.json` — the detected CPU features and a content hash of
//!   the exact source used, so the numbers are attributable.
//!
//! Run it in the pinned container with `sh scripts/run-sweep.sh`.

use ekbloom::{cpu_features, manifestation_sweep, write_evidence_json, SweepConfig};
use std::fmt::Write as _;
use std::path::Path;

/// Deterministic corpus size (~2 MiB).
const SRC_LEN: usize = 2 * 1024 * 1024;

/// Seed for the deterministic corpus.
const SRC_SEED: u64 = 0xEB10_0000_1A2B_3C4D;

fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
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

fn main() -> ekbloom::Result<()> {
    let source = lcg_bytes(SRC_SEED, SRC_LEN);
    let source_hash = blake3::hash(&source);
    let cfg = SweepConfig::default();

    let report = manifestation_sweep(&source, &cfg)?;

    let dir = Path::new("evidence/bench");
    write_evidence_json(&dir.join("sweep.json"), &report)?;

    // CPU + provenance record, written by hand (no serde dependency).
    let cpu = cpu_features();
    let mut meta = String::new();
    meta.push_str("{\n");
    let _ = writeln!(
        meta,
        "  \"cpu\": {{ \"sse2\": {}, \"avx2\": {}, \"avx512f\": {} }},",
        cpu.sse2, cpu.avx2, cpu.avx512f
    );
    let _ = writeln!(meta, "  \"source_len\": {},", source.len());
    let _ = writeln!(meta, "  \"source_seed\": {},", SRC_SEED);
    let _ = writeln!(meta, "  \"iterations\": {},", cfg.iterations);
    let _ = writeln!(meta, "  \"sweep_seed\": {},", cfg.seed);
    let _ = writeln!(meta, "  \"source_blake3\": \"{}\"", source_hash.to_hex());
    meta.push_str("}\n");
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("cpu.json"), meta)?;

    println!(
        "sweep: {} points, source_len={} bytes, blake3={}",
        report.points.len(),
        source.len(),
        source_hash.to_hex()
    );
    Ok(())
}
