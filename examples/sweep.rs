//! Run the manifestation sweep over a deterministic corpus and write evidence.
//!
//! Writes:
//! - `evidence/bench/sweep.json` — the measured speed-vs-manifested-space
//!   frontier (one point per profile),
//! - `evidence/bench/sweep-large.json` — the extended curves, budget frontier,
//!   cold starts and cache-resident throughput over a ~4 MiB corpus,
//! - `evidence/bench/depth-metrics.json` — the per-depth path-bloom metrics
//!   table (`D` in `{1,2,4,8,16,32}`): construction time, physical bytes,
//!   sequential throughput and random-1b p50,
//! - `evidence/bench/memory-tiers.json` — the L1/L2/LLC/DRAM memory-tier curve
//!   (O9),
//! - `evidence/bench/adaptive.json` — the adaptive-shell demonstration (O7),
//! - `evidence/bench/cpu.json` — the detected CPU features and a content hash of
//!   the exact source used, so the numbers are attributable.
//!
//! Run it in the pinned container with `sh scripts/run-sweep.sh`.

use ekbloom::sweep::{
    depth_metrics, large_sweep, memory_tiers, write_depth_metrics_json, write_large_evidence_json,
    write_memory_tiers_json, DEFAULT_MEMORY_TIERS,
};
use ekbloom::{
    adapt_to_workload, cpu_features, create, manifestation_sweep, write_evidence_json,
    ManifestationProfile, SweepConfig, Workload, WorkloadKind,
};
use std::fmt::Write as _;
use std::path::Path;

/// Deterministic corpus size (~2 MiB) for the classic frontier.
const SRC_LEN: usize = 2 * 1024 * 1024;

/// Seed for the deterministic corpus.
const SRC_SEED: u64 = 0xEB10_0000_1A2B_3C4D;

/// Deterministic corpus size (~4 MiB) for the extended sweep.
const LARGE_SRC_LEN: usize = 4 * 1024 * 1024;

/// Seed for the extended-sweep corpus.
const LARGE_SRC_SEED: u64 = 0xEB10_0000_DEAD_BEEF;

/// Cache-resident corpus size (~64 KiB, fits private caches).
const CACHE_SRC_LEN: usize = 64 * 1024;

/// Adaptive demonstration corpus (~1 MiB).
const ADAPT_SRC_LEN: usize = 1024 * 1024;

/// Seed for the adaptive demonstration corpus.
const ADAPT_SRC_SEED: u64 = 0xEB10_0000_AD49_7001;

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

    // --- extended sweep over a ~4 MiB corpus -----------------------------------
    let large_source = lcg_bytes(LARGE_SRC_SEED, LARGE_SRC_LEN);
    let large_hash = blake3::hash(&large_source);
    let large_cfg = SweepConfig {
        iterations: 32,
        seed: cfg.seed,
    };
    let large = large_sweep(&large_source, &large_cfg)?;
    write_large_evidence_json(&dir.join("sweep-large.json"), &large)?;

    println!(
        "large sweep: {} budget rows, {} curves, cold_start={} profiles, source_len={} bytes, blake3={}",
        large.budget_frontier.len(),
        large.curves.len(),
        large.cold_start.len(),
        large_source.len(),
        large_hash.to_hex()
    );
    println!("budget frontier:");
    for bp in &large.budget_frontier {
        println!(
            "  M/N target {:.2}: chosen={:<16} phys={:>10} B (achieved {:.3}), seq={:.1} MB/s, 1b p50={:.0} ns, 4k p50={:.0} ns",
            bp.target_ratio,
            if bp.chosen_profile.is_empty() {
                "(none)"
            } else {
                bp.chosen_profile.as_str()
            },
            bp.physical_bytes,
            bp.achieved_ratio,
            bp.seq_bytes_per_s / 1e6,
            bp.random_1b_ns_p50,
            bp.random_4k_ns_p50,
        );
    }
    println!(
        "cache-resident seq ({} KiB): {:.1} MB/s",
        CACHE_SRC_LEN / 1024,
        large.cache_resident_seq_bytes_per_s / 1e6
    );

    // --- per-depth metrics (§10) ----------------------------------------------
    let depth = depth_metrics(&large_source, &large_cfg)?;
    write_depth_metrics_json(&dir.join("depth-metrics.json"), &depth)?;
    println!(
        "depth metrics (path bloom, source_len={}):",
        depth.source_len
    );
    for p in &depth.points {
        println!(
            "  D={:<2} phys={:>10} B build={:>12} ns seq={:>10.1} MB/s 1b p50={:>8.0} ns | {}",
            p.depth,
            p.physical_bytes,
            p.build_ns,
            p.seq_bytes_per_s / 1e6,
            p.random_1b_ns_p50,
            p.cache_note
        );
    }

    // --- memory tiers (O9) -----------------------------------------------------
    let tiers = memory_tiers(&DEFAULT_MEMORY_TIERS, &cfg)?;
    write_memory_tiers_json(&dir.join("memory-tiers.json"), &tiers)?;
    println!("memory tiers (path D=32):");
    for t in &tiers.tiers {
        println!(
            "  {:<4} N={:>11} B phys={:>11} B seq={:>10.1} MB/s 1b p50={:>8.0} ns 4k p50={:>8.0} ns | {}",
            t.tier,
            t.corpus_bytes,
            t.physical_bytes,
            t.seq_bytes_per_s / 1e6,
            t.random_1b_ns_p50,
            t.random_4k_ns_p50,
            t.cache_note
        );
    }

    // --- adaptive shell demonstration (O7) ------------------------------------
    let adapt_source = lcg_bytes(ADAPT_SRC_SEED, ADAPT_SRC_LEN);
    let hot_prefix = 64 * 1024u64;
    let sample: Vec<(u64, u64)> = {
        let mut s = 0xAD47_0000_0000_0001u64;
        (0..512)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s % hot_prefix, 1u64)
            })
            .collect()
    };
    let workload = Workload {
        kind: WorkloadKind::HotSet,
        hot_prefix: Some(hot_prefix),
        sample,
    };
    let mut adapt_body = create(&adapt_source, ManifestationProfile::Root)?;
    let adapt = adapt_to_workload(&mut adapt_body, &workload, &cfg)?;

    let mut aj = String::new();
    aj.push_str("{\n");
    let _ = writeln!(aj, "  \"kind\": \"{}\",", workload.kind.as_str());
    let _ = writeln!(aj, "  \"hot_prefix\": {},", hot_prefix);
    let _ = writeln!(aj, "  \"source_len\": {},", adapt_source.len());
    let _ = writeln!(aj, "  \"built\": \"{}\",", adapt.built);
    let _ = writeln!(
        aj,
        "  \"added_physical_bytes\": {},",
        adapt.added_physical_bytes
    );
    let _ = writeln!(aj, "  \"before_generation\": {},", adapt.before_generation);
    let _ = writeln!(aj, "  \"new_generation\": {},", adapt.new_generation);
    let _ = writeln!(
        aj,
        "  \"before_ns_per_read\": {},",
        adapt.before_ns_per_read
    );
    let _ = writeln!(aj, "  \"after_ns_per_read\": {},", adapt.after_ns_per_read);
    let _ = writeln!(
        aj,
        "  \"before_bytes_per_s\": {},",
        adapt.before_bytes_per_s
    );
    let _ = writeln!(aj, "  \"after_bytes_per_s\": {},", adapt.after_bytes_per_s);
    let _ = writeln!(aj, "  \"improved\": {},", adapt.improved);
    let _ = writeln!(aj, "  \"observed_reads\": {},", adapt.observed_reads);
    let _ = writeln!(
        aj,
        "  \"observed_working_set_bytes\": {},",
        adapt.observed_working_set_bytes
    );
    let _ = writeln!(aj, "  \"note\": \"{}\"", adapt.note);
    aj.push_str("}\n");
    std::fs::write(dir.join("adaptive.json"), aj)?;

    println!(
        "adaptive (O7, {}): built={} added={} B gen {} -> {}; {:.0} ns/read -> {:.0} ns/read; {}",
        workload.kind.as_str(),
        adapt.built,
        adapt.added_physical_bytes,
        adapt.before_generation,
        adapt.new_generation,
        adapt.before_ns_per_read,
        adapt.after_ns_per_read,
        if adapt.improved {
            "IMPROVED"
        } else {
            "no change"
        }
    );
    Ok(())
}
