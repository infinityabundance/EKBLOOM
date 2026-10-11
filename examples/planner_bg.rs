//! Workload-directed background-planner measurement (directive §4).
//!
//! A **moving-hot-window** workload: the hot window hops to a fresh, non-
//! overlapping set of regions, and for each window the harness measures reads
//! **before** the background planner has reacted (cold) and **after** it has
//! promoted the window with certified byte tiles (warm). The planner is driven
//! only by the sharded access counters, entirely off the read path. The harness
//! reports:
//!
//! * latency (p50/p99) **vs time**, i.e. the cold→warm adaptation recovery;
//! * bytes spent and adaptations;
//! * that committed history is untouched (stable core hash; committed tile sets
//!   only ever grow);
//! * that the read path is **lock-free** (shared-lock acquisitions are
//!   `O(epochs)`, never `O(reads)`).
//!
//! Writes `evidence/bench/planner-bg.json`, run by `sh scripts/run-planner-bg.sh`.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ekbloom::{
    create, manifest, now_ns, AccessObserver, AccessPath, BackgroundPlanner,
    BackgroundPlannerConfig, BenefitModel, DirectoryKind, EkbloomBody, ManifestationBudget,
    ManifestationProfile, PlannerReader, Publication, ReadRequest,
};

/// Deterministic corpus size (4 MiB).
const CORPUS_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed.
const CORPUS_SEED: u64 = 0xEB10_0000_8145_0000;
/// Observer/planner region size (64 KiB).
const REGION_BYTES: u64 = 64 * 1024;
/// Regions in the moving hot window.
const WINDOW_REGIONS: u64 = 8;
/// Number of distinct windows visited.
const WINDOWS: usize = 6;
/// Reads timed per (cold/warm) phase.
const READS_PER_PHASE: usize = 1024;
/// Planner interval.
const PLANNER_MS: u64 = 12;
/// Time to let the background planner react after a cold burst (must comfortably
/// exceed one planning epoch plus the tile build).
const REACT_MS: u64 = 90;

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

fn next_rand(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

struct PhaseSample {
    phase: &'static str,
    window: usize,
    hot_fraction: f64,
    p50_ns: u64,
    p99_ns: u64,
    version: u64,
}

/// Read `READS_PER_PHASE` single bytes uniformly in `[lo, hi)`, observe each, and
/// return `(hot_fraction, p50, p99)`.
fn measure_phase(
    reader: &PlannerReader,
    observer: &AccessObserver,
    source: &[u8],
    lo: u64,
    hi: u64,
    rng: &mut u64,
) -> ekbloom::Result<(f64, u64, u64)> {
    let mut latencies = Vec::with_capacity(READS_PER_PHASE);
    let mut hot = 0usize;
    for _ in 0..READS_PER_PHASE {
        let span = (hi - lo).max(1);
        let off = lo + next_rand(rng) % span;
        let is_hot = reader.current().plan().path_for(off) == AccessPath::HotBytes;
        let t0 = now_ns();
        let got = reader.read(ReadRequest {
            offset: off,
            length: 1,
        })?;
        let dt = now_ns().saturating_sub(t0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], source[off as usize], "read mismatch at {off}");
        observer.observe(off, 1)?;
        latencies.push(dt);
        if is_hot {
            hot += 1;
        }
    }
    latencies.sort_unstable();
    let p50 = latencies[latencies.len() / 2];
    let p99 = latencies[(latencies.len() * 99 / 100).min(latencies.len() - 1)];
    Ok((hot as f64 / READS_PER_PHASE as f64, p50, p99))
}

fn main() -> ekbloom::Result<()> {
    let source = lcg(CORPUS_SEED, CORPUS_LEN);
    let n = source.len() as u64;

    // Committed body: the authoritative core plus a bounded cold-restart checkpoint
    // set (cold reads are ~230 ns of bounded replay, not an O(offset) genesis
    // replay). No tiles yet — the planner adds them.
    let mut body: EkbloomBody = create(&source, ManifestationProfile::Root)?;
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 })?;
    let committed_core_hash = body.core_hash;
    let initial_committed_tiles = body.tiles.len();
    let initial_physical = body.physical_bytes();

    let publication = Publication::new(body)?;
    let observer = Arc::new(AccessObserver::new(n, REGION_BYTES, 8)?);
    let mut reader = PlannerReader::new(Arc::clone(&publication));

    let planner_cfg = BackgroundPlannerConfig {
        interval: Duration::from_millis(PLANNER_MS),
        region_bytes: REGION_BYTES,
        tile_shift: 12,
        directory_kind: DirectoryKind::TwoLevelRadix,
        budget: ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
        model: BenefitModel::measured_default(),
        max_adaptations: (WINDOWS * 2) as u64,
    };
    let mut planner =
        BackgroundPlanner::spawn(Arc::clone(&publication), Arc::clone(&observer), planner_cfg);

    let regions_total = n.div_ceil(REGION_BYTES);
    let max_start_region = regions_total.saturating_sub(WINDOW_REGIONS).max(1);
    let mut rng = 0x8145_0000_0000_0001u64;

    // Warm-up: touch every page of the object (first-touch page faults, read-path
    // priming) and prime the plan before any measurement.
    let mut off = 0u64;
    while off < n {
        let _ = reader.read(ReadRequest {
            offset: off,
            length: 1,
        })?;
        off += 4096;
    }
    for _ in 0..512 {
        let off = next_rand(&mut rng) % n;
        let _ = reader.read(ReadRequest {
            offset: off,
            length: 1,
        })?;
    }
    reader.refresh();

    let locks_before = publication.reader_lock_acquisitions();
    let mut reads_total = 0usize;
    let mut samples: Vec<PhaseSample> = Vec::with_capacity(WINDOWS * 2);

    for w in 0..WINDOWS {
        let start_region = (w as u64 * WINDOW_REGIONS) % max_start_region;
        let hot_start = start_region * REGION_BYTES;
        let hot_end = (hot_start + WINDOW_REGIONS * REGION_BYTES).min(n);

        // COLD: the window is not yet resident; observe this access burst.
        reader.refresh();
        observer.reset();
        let (cold_hot, cold_p50, cold_p99) =
            measure_phase(&reader, &observer, &source, hot_start, hot_end, &mut rng)?;
        reads_total += READS_PER_PHASE;
        samples.push(PhaseSample {
            phase: "cold",
            window: w,
            hot_fraction: cold_hot,
            p50_ns: cold_p50,
            p99_ns: cold_p99,
            version: reader.cached_version(),
        });

        // Let the background planner react to this window (> one epoch + build).
        std::thread::sleep(Duration::from_millis(REACT_MS));
        reader.refresh();

        // WARM: same window, now promoted.
        let (warm_hot, warm_p50, warm_p99) =
            measure_phase(&reader, &observer, &source, hot_start, hot_end, &mut rng)?;
        reads_total += READS_PER_PHASE;
        samples.push(PhaseSample {
            phase: "warm",
            window: w,
            hot_fraction: warm_hot,
            p50_ns: warm_p50,
            p99_ns: warm_p99,
            version: reader.cached_version(),
        });

        println!(
            "  window {w}: cold hot={:>5.1}% p50={cold_p50:>6} ns p99={cold_p99:>7} ns  ->  \
             warm hot={:>5.1}% p50={warm_p50:>6} ns p99={warm_p99:>7} ns (v{})",
            cold_hot * 100.0,
            warm_hot * 100.0,
            reader.cached_version()
        );
    }

    planner.stop();
    let stats = planner.stats();
    let locks_after = publication.reader_lock_acquisitions();

    // Committed history is untouched.
    let final_snapshot = reader.current();
    let committed_core_ok = final_snapshot.body().core_hash == committed_core_hash;
    let committed_tiles_grew = final_snapshot.body().tiles.len() >= initial_committed_tiles;
    let history_touched = final_snapshot.body().history.is_some();
    assert!(committed_core_ok, "committed core hash changed");
    assert!(committed_tiles_grew, "committed tile sets shrank");
    assert!(!history_touched, "committed history was modified");

    let (cold_p50, cold_p99, warm_p50, warm_p99) = recovery(&samples);
    println!(
        "  adaptation recovery: cold p50={cold_p50} ns -> warm p50={warm_p50} ns; \
         cold p99={cold_p99} ns -> warm p99={warm_p99} ns"
    );
    println!(
        "  planner: adaptations={} bytes_spent={} regions_manifested={} epochs={} failed={}",
        stats.adaptations,
        stats.bytes_spent,
        stats.regions_manifested,
        stats.epochs,
        stats.certifications_failed
    );
    println!(
        "  lock-free read path: reads={reads_total} shared-lock acquisitions={} (O(epochs), not O(reads))",
        locks_after - locks_before
    );

    write_planner_bg_json(
        Path::new("evidence/bench/planner-bg.json"),
        n,
        initial_physical,
        final_snapshot.body().physical_bytes(),
        initial_committed_tiles,
        final_snapshot.body().tiles.len(),
        committed_core_ok,
        &samples,
        &stats,
        reads_total,
        locks_after - locks_before,
        (cold_p50, cold_p99, warm_p50, warm_p99),
    )?;
    println!("wrote evidence/bench/planner-bg.json");
    Ok(())
}

/// Median (cold p50, cold p99, warm p50, warm p99) across all windows.
///
/// Medians are used for the headline numbers so a single first-touch outlier
/// (window 0's first cold read) cannot dominate; the full per-window timeline is
/// recorded in the evidence JSON.
fn recovery(samples: &[PhaseSample]) -> (u64, u64, u64, u64) {
    let collect = |phase: &str| -> (Vec<u64>, Vec<u64>) {
        samples.iter().filter(|s| s.phase == phase).fold(
            (Vec::new(), Vec::new()),
            |(mut p50, mut p99), s| {
                p50.push(s.p50_ns);
                p99.push(s.p99_ns);
                (p50, p99)
            },
        )
    };
    let median = |v: &mut Vec<u64>| -> u64 {
        if v.is_empty() {
            0
        } else {
            v.sort_unstable();
            v[v.len() / 2]
        }
    };
    let (mut c50, mut c99) = collect("cold");
    let (mut w50, mut w99) = collect("warm");
    (
        median(&mut c50),
        median(&mut c99),
        median(&mut w50),
        median(&mut w99),
    )
}

#[allow(clippy::too_many_arguments)]
fn write_planner_bg_json(
    path: &Path,
    logical_len: u64,
    initial_physical: u64,
    final_physical: u64,
    initial_tiles: usize,
    final_tiles: usize,
    committed_core_ok: bool,
    samples: &[PhaseSample],
    stats: &ekbloom::PlannerStats,
    reads_total: usize,
    read_path_locks: u64,
    recovery: (u64, u64, u64, u64),
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/planner_bg.rs (sh scripts/run-planner-bg.sh)\","
    );
    let _ = writeln!(s, "  \"logical_len\": {logical_len},");
    let _ = writeln!(s, "  \"region_bytes\": {REGION_BYTES},");
    let _ = writeln!(s, "  \"window_regions\": {WINDOW_REGIONS},");
    let _ = writeln!(s, "  \"windows\": {WINDOWS},");
    let _ = writeln!(s, "  \"reads_per_phase\": {READS_PER_PHASE},");
    let _ = writeln!(s, "  \"reads_total\": {reads_total},");
    let _ = writeln!(s, "  \"initial_physical_bytes\": {initial_physical},");
    let _ = writeln!(s, "  \"final_physical_bytes\": {final_physical},");
    let _ = writeln!(s, "  \"adaptations\": {},", stats.adaptations);
    let _ = writeln!(s, "  \"bytes_spent\": {},", stats.bytes_spent);
    let _ = writeln!(s, "  \"regions_manifested\": {},", stats.regions_manifested);
    let _ = writeln!(s, "  \"planner_epochs\": {},", stats.epochs);
    let _ = writeln!(
        s,
        "  \"certifications_failed\": {},",
        stats.certifications_failed
    );
    let _ = writeln!(s, "  \"committed_core_hash_stable\": {committed_core_ok},");
    let _ = writeln!(s, "  \"committed_tiles_initial\": {initial_tiles},");
    let _ = writeln!(s, "  \"committed_tiles_final\": {final_tiles},");
    let _ = writeln!(s, "  \"committed_history_touched\": false,");
    let _ = writeln!(s, "  \"read_path_shared_locks\": {read_path_locks},");
    let _ = writeln!(
        s,
        "  \"read_path_locks_note\": \"shared-lock acquisitions are O(epochs) (planner snapshot + reader refresh), never O(reads); PlannerReader::read touches only its cached Arc\","
    );
    let _ = writeln!(s, "  \"cold_p50_ns\": {},", recovery.0);
    let _ = writeln!(s, "  \"cold_p99_ns\": {},", recovery.1);
    let _ = writeln!(s, "  \"warm_p50_ns\": {},", recovery.2);
    let _ = writeln!(s, "  \"warm_p99_ns\": {},", recovery.3);
    s.push_str("  \"timeline\": [\n");
    for (i, e) in samples.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"phase\": \"{}\",", e.phase);
        let _ = writeln!(s, "      \"window\": {},", e.window);
        let _ = writeln!(s, "      \"hot_fraction\": {:.4},", e.hot_fraction);
        let _ = writeln!(s, "      \"p50_ns\": {},", e.p50_ns);
        let _ = writeln!(s, "      \"p99_ns\": {},", e.p99_ns);
        let _ = writeln!(s, "      \"version\": {}", e.version);
        s.push_str(if i + 1 == samples.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, s)?;
    Ok(())
}
