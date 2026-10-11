//! Conventional direct-access baselines vs EKBLOOM under identical workloads.
//!
//! Run inside the pinned container with `sh scripts/run-baselines.sh`. Measures
//! the four conventional baselines (`RawVec`, `RawFile`, `Predecoded`,
//! `IndexedArray`) and the EKBLOOM profiles `Root`, `Path{D=4}` and `Full` over
//! one deterministic corpus with the same seeds, and writes
//! `evidence/bench/baselines.json`.

use ekbloom::{
    cpu_features, create, measure_baseline, measure_body, now_ns, write_baselines_json, Baseline,
    BaselineKind, BaselinePoint, BaselineReport, ManifestationProfile, SweepConfig,
};
use std::path::Path;

/// Deterministic corpus size (~1 MiB) — bounded, fits comfortably in the
/// memory-capped container, and large enough that `Root` replay is measurable.
const SRC_LEN: usize = 1024 * 1024;

/// Deterministic corpus seed.
const SRC_SEED: u64 = 0xEB10_0000_BA5E_1001;

/// Deterministic LCG corpus (identical generator to `examples/sweep.rs`).
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

/// A compact capability tag, e.g. `h---x` (`h` history, `r` restart,
/// `p` per-position, `s` shells, `x` concealment; `-` = absent).
fn cap_str(p: &BaselinePoint) -> String {
    let c = p.capabilities;
    let mut s = String::with_capacity(5);
    s.push(if c.verifiable_encoded_history {
        'h'
    } else {
        '-'
    });
    s.push(if c.exact_checkpoint_restart { 'r' } else { '-' });
    s.push(if c.per_position_state { 'p' } else { '-' });
    s.push(if c.adaptive_shells { 's' } else { '-' });
    s.push(if c.concealment { 'x' } else { '-' });
    s
}

fn print_row(p: &BaselinePoint) {
    println!(
        "{:<22} {:<9} {:>11} {:>10} {:>10.1} {:>9.0} {:>9.0} {:>9.0}  {}",
        p.name,
        p.class,
        p.physical_bytes,
        p.construction_ns,
        p.seq_bytes_per_s / 1e6,
        p.random_1b_ns_p50,
        p.random_1b_ns_p99,
        p.random_4k_ns_p50,
        cap_str(p),
    );
}

fn main() -> ekbloom::Result<()> {
    let source = lcg_bytes(SRC_SEED, SRC_LEN);
    let cfg = SweepConfig::default();
    let dir = Path::new("target/baseline-tmp");

    let mut points: Vec<BaselinePoint> = Vec::new();

    println!(
        "baselines vs ekbloom: source_len={} bytes, iterations={}, seed={:#x}",
        source.len(),
        cfg.iterations,
        cfg.seed
    );
    println!(
        "{:<22} {:<9} {:>11} {:>10} {:>10} {:>9} {:>9} {:>9}  caps",
        "name", "class", "bytes", "build ns", "seq MB/s", "1b p50", "1b p99", "4k p50"
    );

    for kind in BaselineKind::all() {
        let b = Baseline::build(kind, &source, dir)?;
        let p = measure_baseline(&b, &cfg)?;
        print_row(&p);
        points.push(p);
    }

    let profiles: [(&str, ManifestationProfile); 3] = [
        ("Root", ManifestationProfile::Root),
        ("Path{D=4}", ManifestationProfile::Path { depth: 4 }),
        ("Full", ManifestationProfile::Full),
    ];
    for (label, profile) in profiles {
        let start = now_ns();
        let body = create(&source, profile)?;
        let build_ns = now_ns().saturating_sub(start);
        let p = measure_body(&body, label, build_ns, &cfg)?;
        print_row(&p);
        points.push(p);
    }

    let report = BaselineReport {
        cpu: cpu_features(),
        source_len: source.len() as u64,
        source_seed: SRC_SEED,
        iterations: cfg.iterations,
        sweep_seed: cfg.seed,
        points,
    };
    let out = Path::new("evidence/bench/baselines.json");
    write_baselines_json(out, &report)?;
    println!("wrote {} ({} rows)", out.display(), report.points.len());
    Ok(())
}
