//! The rigorous performance court (directive §12).
//!
//! Runs the [`ekbloom::court`] harness over a ladder of working-set sizes that
//! sit around real cache transitions, with a raw-byte direct-access baseline at
//! identical residency for every workload, interleaved/seed-randomised repetitions
//! and bootstrap 95% confidence intervals. Writes `evidence/bench/court.json`.
//!
//! Run in the pinned, memory-capped container (and `taskset`-pinned) with
//! `sh scripts/run-court.sh`.

use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

use ekbloom::baseline::{Baseline, BaselineKind};
use ekbloom::court::Workload;
use ekbloom::reader::FileBody;
use ekbloom::writer::write_body;
use ekbloom::{
    collect_environment, create, lcg_bytes, manifest, read_shell, write_court_json, AccessPlan,
    AccessTrace, Candidate, Court, CourtConfig, DirectoryKind, Dist, ManifestationBudget,
    ManifestationProfile, Planner, ReadRequest, Row, ShellChoice, TileSet,
};

/// Working sets actually run (a bounded subset of
/// [`ekbloom::default_working_sets`], chosen to straddle L1/L2/LLC/DRAM). The
/// full configurable ladder (including 80/96/128 MiB and 1 GiB) is available via
/// `ekbloom::default_working_sets()`.
const WORKING_SETS: [usize; 7] = [
    32 * 1024,
    256 * 1024,
    1024 * 1024,
    2 * 1024 * 1024,
    16 * 1024 * 1024,
    64 * 1024 * 1024,
    256 * 1024 * 1024,
];

/// Seed for the deterministic corpora.
const SRC_SEED: u64 = 0xEB10_0000_C0DE_7001;

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

/// Build the three in-memory candidates over `source`: the raw-byte baseline, a
/// depth-32 path bloom, and a planner over a tiles+checkpoint body.
fn candidates_for(source: &[u8], cfg: &CourtConfig) -> ekbloom::Result<Vec<Candidate>> {
    let raw = Candidate::raw_vec("raw-vec", source.to_vec());

    let path_body = create(source, ManifestationProfile::Path { depth: 32 })?;
    let path = Candidate::body("ekbloom-path32", path_body);

    // A planner body: exact byte tiles over the hot prefix plus a cold checkpoint
    // set (no whole-object path bloom), so the plan must mix HotBytes and
    // PackedRestart by region.
    let mut pbody = create(source, ManifestationProfile::Root)?;
    let n = source.len() as u64;
    let hot_len = (n / 16).max(4096).min(n);
    let trace = AccessTrace {
        regions: vec![(0, hot_len)],
        counts: vec![1000],
    };
    TileSet::manifest_hot_tiles(
        &mut pbody,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )?;
    manifest(&mut pbody, ManifestationProfile::Sparse { interval: 64 })?;
    let plan = AccessPlan::plan(&pbody, cfg)?;
    let planned = Candidate::planned("planner-hotcold", Planner::new(plan), pbody);

    Ok(vec![raw, path, planned])
}

fn main() -> ekbloom::Result<()> {
    let cfg = CourtConfig {
        reps: 3,
        interleave: true,
        seed: 0xE8B1_0000_C09B_7D15,
    };
    let mut court = Court::new(cfg);
    println!(
        "court: clock estimate {:.2} GHz, loop overhead {:.3} ns/iter, reps={}, interleave={}",
        court.clock_ghz(),
        court.report_loop_overhead(),
        cfg.reps,
        cfg.interleave
    );

    let workloads = [
        Workload::Sequential,
        Workload::Random1b,
        Workload::Random4k,
        Workload::Random64k,
        Workload::HotCold90_10,
        Workload::HotCold99_1,
        Workload::MovingWindow,
    ];

    for &w in &WORKING_SETS {
        let source = lcg_bytes(SRC_SEED ^ (w as u64), w);
        let cands = candidates_for(&source, &cfg)?;
        let refs: Vec<&Candidate> = cands.iter().collect();
        for wl in workloads {
            court.measure(w as u64, wl, &refs);
        }
        println!("  measured working set {w} B ({} candidates)", refs.len());
    }

    // Concurrent reads while manifestation runs (2 MiB working set).
    {
        let w = 2 * 1024 * 1024usize;
        let source = lcg_bytes(SRC_SEED ^ (w as u64) ^ 0xC0C0, w);
        let cands = candidates_for(&source, &cfg)?;
        let refs: Vec<&Candidate> = cands.iter().collect();
        court.measure_concurrent(&source, &refs);
        println!("  measured concurrent-manifestation at {w} B");
    }

    // File-backed cold single-byte reads (4 MiB object): one `pread` per byte for
    // both the raw-file baseline and the depth-32 ekbloom file.
    measure_file_backed(&mut court, 4 * 1024 * 1024)?;

    let report = court.report(collect_environment());
    let dir = Path::new("evidence/bench");
    write_court_json(&dir.join("court.json"), &report)?;
    print_summary(&report);

    measure_planner_evidence(dir)?;

    println!(
        "wrote evidence/bench/court.json ({} rows)",
        report.rows.len()
    );
    Ok(())
}

/// Measure the certified planner against each fixed path on a hot/cold workload
/// and write `evidence/bench/planner.json`.
fn measure_planner_evidence(dir: &Path) -> ekbloom::Result<()> {
    const W: usize = 4 * 1024 * 1024;
    let source = lcg_bytes(SRC_SEED ^ 0x9117_0000, W);
    let n = W as u64;

    // Hot prefix tiled exactly; a cold checkpoint set bounds the rest. No
    // whole-object path bloom and no history, so the region planner must mix
    // HotBytes (hot) with PackedRestart (cold).
    let mut body = create(&source, ManifestationProfile::Root)?;
    let hot_len = n / 4;
    let trace = AccessTrace {
        regions: vec![(0, hot_len)],
        counts: vec![1000],
    };
    let tiles = TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )?;
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 })?;

    let cfg = CourtConfig {
        reps: 3,
        interleave: true,
        seed: 0x91A7_0000_0000_0001,
    };
    let plan = AccessPlan::plan(&body, &cfg)?;
    let distribution: Vec<(String, usize)> = plan
        .distribution()
        .into_iter()
        .map(|(p, c)| (p.as_str().to_string(), c))
        .collect();
    let certified = plan.all_certified();
    let region_bytes = plan.region_bytes();
    let planner = Planner::new(plan);

    // Byte-exactness spot check across the hot/cold boundary.
    let probe = ReadRequest {
        offset: hot_len - 3,
        length: 7,
    };
    assert_eq!(planner.read(&body, probe)?, ekbloom::read(&body, probe)?);

    let hot_offsets = sample_offsets(0x4017_0000, 0, hot_len, 200);
    let cold_offsets = sample_offsets(0xC017_0000, n / 2, n, 64);
    let overhead = ekbloom::loop_overhead_ns(2_000_000);

    let hot_planner = batched(&hot_offsets, 64, overhead, 1, |off| {
        black_box(
            planner
                .read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: 1,
                    },
                )
                .unwrap(),
        );
    });
    let cold_planner = batched(&cold_offsets, 1, overhead, 2, |off| {
        black_box(
            planner
                .read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: 1,
                    },
                )
                .unwrap(),
        );
    });
    let hot_tiles = batched(&hot_offsets, 64, overhead, 3, |off| {
        black_box(body.tiles[0].read(&body.core, off, 1).unwrap());
    });
    let cold_tiles = batched(&cold_offsets, 1, overhead, 4, |off| {
        black_box(body.tiles[0].read(&body.core, off, 1).unwrap());
    });
    let hot_restart = batched(&hot_offsets, 64, overhead, 5, |off| {
        black_box(read_shell(&body, ShellChoice::Checkpoint, off, 1).unwrap());
    });
    let cold_restart = batched(&cold_offsets, 1, overhead, 6, |off| {
        black_box(read_shell(&body, ShellChoice::Checkpoint, off, 1).unwrap());
    });
    let cold_replay = batched(&cold_offsets, 1, overhead, 7, |off| {
        black_box(read_shell(&body, ShellChoice::Machine, off, 1).unwrap());
    });
    let hot_raw = batched(&hot_offsets, 64, overhead, 8, |off| {
        black_box(source[off as usize]);
    });
    let cold_raw = batched(&cold_offsets, 1, overhead, 9, |off| {
        black_box(source[off as usize]);
    });

    let rows = [
        ("planner (per-region)", &hot_planner, &cold_planner),
        ("fixed hot-bytes (tiles)", &hot_tiles, &cold_tiles),
        ("fixed packed-restart", &hot_restart, &cold_restart),
        ("fixed core-replay", &cold_replay, &cold_replay),
        ("raw-vec baseline", &hot_raw, &cold_raw),
    ];
    let latency = rows
        .iter()
        .map(|(name, hot, cold)| row_json(name, hot, cold))
        .collect::<Vec<_>>()
        .join(",\n");

    let dist_json = distribution
        .iter()
        .map(|(p, c)| format!("    {{ \"path\": \"{p}\", \"regions\": {c} }}"))
        .collect::<Vec<_>>()
        .join(",\n");

    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/court.rs (sh scripts/run-court.sh)\","
    );
    let _ = writeln!(js, "  \"logical_len\": {n},");
    let _ = writeln!(js, "  \"physical_bytes\": {},", body.physical_bytes());
    let _ = writeln!(js, "  \"tiles_selected\": {},", tiles.tiles_selected);
    let _ = writeln!(js, "  \"hot_region_bytes\": {hot_len},");
    let _ = writeln!(js, "  \"region_bytes\": {region_bytes},");
    let _ = writeln!(js, "  \"certified_paths_only\": {certified},");
    let _ = writeln!(js, "  \"request_length_bytes\": 1,");
    let _ = writeln!(js, "  \"chosen_path_distribution\": [\n{dist_json}\n  ],");
    let _ = writeln!(js, "  \"latency\": [\n{latency}\n  ],");
    let _ = writeln!(
        js,
        "  \"note\": \"hot reads are served by exact byte tiles (HotBytes); cold reads by \
         bounded checkpoint restart (PackedRestart); the planner matches the best fixed path on \
         each region while every fixed path is slow somewhere.\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("planner.json"), js)?;
    println!("wrote evidence/bench/planner.json");
    Ok(())
}

/// One planner.json latency row.
fn row_json(name: &str, hot: &Dist, cold: &Dist) -> String {
    format!(
        "    {{ \"path\": \"{name}\", \"hot_p50_ns\": {:.1}, \"hot_ci95\": [{:.1}, {:.1}], \
         \"cold_p50_ns\": {:.1}, \"cold_ci95\": [{:.1}, {:.1}] }}",
        hot.p50, hot.ci95_lo, hot.ci95_hi, cold.p50, cold.ci95_lo, cold.ci95_hi
    )
}

/// Batched per-operation latency distribution over `offsets`.
fn batched<F: FnMut(u64)>(
    offsets: &[u64],
    batch: usize,
    overhead: f64,
    seed: u64,
    mut op: F,
) -> Dist {
    let mut samples = Vec::with_capacity(offsets.len());
    for &off in offsets {
        let a = std::time::Instant::now();
        for _ in 0..batch {
            op(off);
        }
        let e = a.elapsed().as_nanos() as f64;
        let per = ((e - overhead * batch as f64) / batch as f64).max(0.0);
        samples.push(per as u64);
    }
    Dist::from_samples(&samples, seed)
}

/// Deterministic offsets in `[lo, hi)`.
fn sample_offsets(seed: u64, lo: u64, hi: u64, count: usize) -> Vec<u64> {
    let mut s = seed;
    let span = hi.saturating_sub(lo).max(1);
    let mut v = Vec::with_capacity(count);
    for _ in 0..count {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push(lo + (s >> 33) % span);
    }
    v
}

/// Measure cold, file-backed, single-byte reads: the raw-file baseline
/// (`pread` of one byte) versus a depth-32 `.ekb` read through [`FileBody`].
fn measure_file_backed(court: &mut Court, w: usize) -> ekbloom::Result<()> {
    let dir = std::env::temp_dir().join(format!("ekbloom-court-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    let source = lcg_bytes(SRC_SEED ^ (w as u64) ^ 0xF11E_0000, w);
    let body = create(&source, ManifestationProfile::Path { depth: 32 })?;
    let ekb_path = dir.join("court-file.ekb");
    write_body(&ekb_path, &body)?;
    let fbody = FileBody::open(&ekb_path)?;
    let ekb_physical = fbody.physical_len();

    let rawfile = Baseline::build(BaselineKind::RawFile, &source, &dir)?;

    let n = source.len() as u64;
    let samples = 256usize;
    let mut rng = 0x51ED_C01D_0000_0001u64;
    let mut offsets = Vec::with_capacity(samples);
    for _ in 0..samples {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        offsets.push((rng >> 33) % n.max(1));
    }

    let mut out = Vec::new();
    let mut ekb_ns = Vec::with_capacity(samples);
    let mut raw_ns = Vec::with_capacity(samples);
    const BATCH: usize = 64;
    // interleave the two candidates to share any cache/OS drift
    for &off in &offsets {
        let a = std::time::Instant::now();
        for _ in 0..BATCH {
            out.clear();
            let _ = fbody.read_into(
                ReadRequest {
                    offset: off,
                    length: 1,
                },
                &mut out,
            );
        }
        ekb_ns.push((a.elapsed().as_nanos() / BATCH as u128) as u64);

        let a = std::time::Instant::now();
        for _ in 0..BATCH {
            out.clear();
            let _ = rawfile.read_into(
                ReadRequest {
                    offset: off,
                    length: 1,
                },
                &mut out,
            );
        }
        raw_ns.push((a.elapsed().as_nanos() / BATCH as u128) as u64);
    }

    court.push_row(Row {
        working_set: n,
        workload: Workload::FileBackedCold.label().to_string(),
        candidate: "ekbloom-file-path32".to_string(),
        residency: "file-backed".to_string(),
        physical_bytes: ekb_physical,
        dist_ns: Dist::from_samples(&ekb_ns, 0xF11E_0001),
        throughput_bytes_per_s: 0.0,
        cycles_per_byte: 0.0,
        reps: 1,
        samples: ekb_ns.len() as u64,
    });
    court.push_row(Row {
        working_set: n,
        workload: Workload::FileBackedCold.label().to_string(),
        candidate: "raw-file".to_string(),
        residency: "file-backed".to_string(),
        physical_bytes: rawfile.physical_bytes(),
        dist_ns: Dist::from_samples(&raw_ns, 0xF11E_0002),
        throughput_bytes_per_s: 0.0,
        cycles_per_byte: 0.0,
        reps: 1,
        samples: raw_ns.len() as u64,
    });

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Print a compact summary: a few representative rows with CIs and the frontier.
fn print_summary(report: &ekbloom::CourtReport) {
    println!("\n== representative rows (latency ns, 95% CI of the mean) ==");
    for r in report.rows.iter().filter(|r| {
        matches!(
            r.workload.as_str(),
            "random-1b" | "hot-cold-90-10" | "sequential" | "file-backed-cold"
        ) && matches!(
            r.working_set,
            1_048_576 | 67_108_864 | 268_435_456 | 4_194_304
        )
    }) {
        if r.throughput_bytes_per_s > 0.0 {
            println!(
                "  ws={:>10} {:<20} {:<22} {:>9.1} MB/s {:>7.3} cyc/B",
                r.working_set,
                r.workload,
                r.candidate,
                r.throughput_bytes_per_s / 1e6,
                r.cycles_per_byte
            );
        } else {
            println!(
                "  ws={:>10} {:<20} {:<22} p50={:>9.1} [{:>8.1},{:>8.1}] p99={:>9.1} ns",
                r.working_set,
                r.workload,
                r.candidate,
                r.dist_ns.p50,
                r.dist_ns.ci95_lo,
                r.dist_ns.ci95_hi,
                r.dist_ns.p99
            );
        }
    }

    println!("\n== 3-D Pareto frontier (per working set: latency / throughput / efficiency) ==");
    for p in report.pareto.iter().filter(|p| p.on_frontier) {
        println!(
            "  ws={:>10} {:<22} min_lat={:>10} ns max_thr={:>12.1} MB/s eff={:>10.3} phys={:>11} B",
            p.working_set,
            p.candidate,
            if p.min_latency_ns.is_finite() {
                format!("{:.1}", p.min_latency_ns)
            } else {
                "n/a".to_string()
            },
            p.max_throughput_bytes_per_s / 1e6,
            p.efficiency,
            p.physical_bytes
        );
    }
}
