//! Kernel + background-planner tests (directive §6 and §4).
//!
//! Pins, for every kernel, **byte-exactness** against the authoritative machine
//! (all lengths, thread counts and boundary sizes); Kernel B == sequential decode;
//! and, for the background planner, that it selects only certified regions, is
//! additive, bounds by budget, does not spike read tails under concurrent load,
//! exposes a benefit heuristic monotone in the expected direction, and is
//! deterministic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ekbloom::machine::{OrMachine, NL2};
use ekbloom::{
    create, kernel_a_scalar, kernel_a_scalar_windows, kernel_a_simd_windows, kernel_a_table,
    kernel_a_unrolled, kernel_b_simd, kernel_b_threads, manifest, plan_benefits, region_benefit,
    AccessObservation, AccessObserver, AccessPath, BackgroundPlanner, BackgroundPlannerConfig,
    BenefitModel, DirectoryKind, EkbloomBody, ManifestationBudget, ManifestationProfile,
    PlannerReader, Publication, ReadRequest, ResidencyCache,
};

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

fn oracle(x0: u16, r: &[u8]) -> Vec<u8> {
    let mut x = x0;
    let mut out = Vec::with_capacity(r.len());
    for &b in r {
        let h = (x >> 8) as u8;
        let l = (x & 0xFF) as u8;
        let s = (h ^ (h << 1) ^ (h << 3)) ^ l ^ b;
        out.push(s);
        x = (u16::from(l) << 8) | u16::from(s);
    }
    out
}

// ---------------------------------------------------------------------------
// Kernel A
// ---------------------------------------------------------------------------

#[test]
fn kernel_a_variants_are_byte_exact_for_all_lengths() {
    for len in [
        0usize, 1, 2, 3, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 255,
    ] {
        let r = lcg(0xA0A0 ^ len as u64, len);
        for &x0 in &[0u16, 1, 0x00FF, 0x1234, 0x8000, 0xFFFF, 0xABCD] {
            let want = oracle(x0, &r);
            let mut a = Vec::new();
            kernel_a_scalar(x0, &r, &mut a);
            let mut b = Vec::new();
            kernel_a_unrolled(x0, &r, &mut b);
            let mut c = Vec::new();
            kernel_a_table(x0, &r, &mut c);
            assert_eq!(a, want, "scalar len={len} x0={x0:#06x}");
            assert_eq!(b, want, "unrolled len={len} x0={x0:#06x}");
            assert_eq!(c, want, "table len={len} x0={x0:#06x}");
        }
    }
}

#[test]
fn kernel_a_window_batches_match_scalar_and_machine() {
    let core = lcg(0xB0B0, 4096);
    for &window_len in &[1usize, 7, 16, 32, 64] {
        for &windows in &[0usize, 1, 15, 16, 17, 48] {
            if windows == 0 {
                continue;
            }
            let padded = windows.div_ceil(ekbloom::NL2_WINDOW_LANES)
                * ekbloom::NL2_WINDOW_LANES
                * window_len;
            let total = windows * window_len;
            assert!(padded >= total);
            // Authoritative: decode the region directly from genesis.
            let region = &core[..total];
            let truth = NL2.decode(region, total as u64).unwrap();

            let starts = ekbloom::window_start_states(region, window_len).unwrap();
            let starts: Vec<u16> = starts[..windows].to_vec();

            // Group-major step-major layout.
            let mut cols = vec![0u8; padded];
            for (w, x) in starts.iter().enumerate() {
                let g = w / ekbloom::NL2_WINDOW_LANES;
                let lane = w % ekbloom::NL2_WINDOW_LANES;
                let window = &region[w * window_len..(w + 1) * window_len];
                for (i, &byte) in window.iter().enumerate() {
                    cols[(g * window_len + i) * ekbloom::NL2_WINDOW_LANES + lane] = byte;
                }
                let _ = x;
            }

            let mut st_scalar = starts.clone();
            let mut out_scalar = vec![0u8; padded];
            kernel_a_scalar_windows(&mut st_scalar, &cols, window_len, &mut out_scalar).unwrap();
            let mut st_simd = starts.clone();
            let mut out_simd = vec![0u8; padded];
            kernel_a_simd_windows(&mut st_simd, &cols, window_len, &mut out_simd).unwrap();
            assert_eq!(out_simd, out_scalar, "windows={windows} len={window_len}");

            // Reassemble the outputs in logical order and compare to the decode.
            let mut flat = Vec::with_capacity(total);
            for w in 0..windows {
                let g = w / ekbloom::NL2_WINDOW_LANES;
                let lane = w % ekbloom::NL2_WINDOW_LANES;
                for i in 0..window_len {
                    flat.push(out_simd[(g * window_len + i) * ekbloom::NL2_WINDOW_LANES + lane]);
                }
            }
            assert_eq!(flat, truth, "reassembled windows != decode");
        }
    }
}

// ---------------------------------------------------------------------------
// Kernel B
// ---------------------------------------------------------------------------

#[test]
fn kernel_b_matches_sequential_decode_for_all_threads_and_boundaries() {
    for n in [0usize, 1, 15, 16, 17, 63, 64, 65, 255, 256, 257, 1000, 4097] {
        let core = lcg(0xB0B0 ^ n as u64, n);
        let truth = NL2.decode(&core, n as u64).unwrap();
        for &block in &[1usize, 7, 16, 64, 256, 1024] {
            for &threads in &[1usize, 2, 3, 4, 8, 16] {
                let got = kernel_b_threads(&core, block, threads).unwrap();
                assert_eq!(got, truth, "threads n={n} block={block} threads={threads}");
                let got = kernel_b_simd(&core, block, threads).unwrap();
                assert_eq!(got, truth, "simd n={n} block={block} threads={threads}");
            }
        }
    }
    // Zero is clamped to one worker and still exact.
    let core = lcg(0xB0B1, 300);
    let truth = NL2.decode(&core, 300).unwrap();
    assert_eq!(kernel_b_threads(&core, 64, 0).unwrap(), truth);
    assert_eq!(kernel_b_simd(&core, 64, 0).unwrap(), truth);
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

#[test]
fn observer_counts_are_additive_and_bounded() {
    let n = 1 << 16;
    let obs = AccessObserver::new(n, 4096, 4).unwrap();
    assert_eq!(obs.region_count(), 16);
    assert_eq!(obs.region_bytes(), 4096);
    for _ in 0..5 {
        obs.observe(0, 1).unwrap(); // region 0
    }
    obs.observe(4095, 2).unwrap(); // spans regions 0 and 1 (2 bytes: 4095, 4096)
    obs.observe(n - 1, 1).unwrap(); // region 15
    let snapshot = obs.snapshot();
    assert_eq!(snapshot.counts[0], 6);
    assert_eq!(snapshot.counts[1], 1);
    assert_eq!(snapshot.counts[15], 1);
    assert_eq!(snapshot.total_requests(), 8);

    // Concurrent observes land in shards and sum correctly.
    obs.reset();
    let obs = Arc::new(obs);
    let threads = 8usize;
    let per = 1000usize;
    let mut handles = Vec::new();
    for _ in 0..threads {
        let obs = Arc::clone(&obs);
        handles.push(std::thread::spawn(move || {
            for _ in 0..per {
                obs.observe(100, 1).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(obs.snapshot().counts[0], (threads * per) as u64);

    // Out-of-bounds and overflow are rejected without partial accounting.
    obs.reset();
    assert!(obs.observe(n, 1).is_err());
    assert!(obs.observe(u64::MAX, 2).is_err());
    assert_eq!(obs.snapshot().total_requests(), 0);
    // A zero-length read is a no-op.
    obs.observe(0, 0).unwrap();
    assert_eq!(obs.snapshot().total_requests(), 0);
}

// ---------------------------------------------------------------------------
// Benefit heuristic
// ---------------------------------------------------------------------------

fn synthetic_obs(counts: Vec<u64>) -> AccessObservation {
    AccessObservation {
        region_bytes: 4096,
        logical_len: counts.len() as u64 * 4096,
        counts,
    }
}

#[test]
fn benefit_is_monotone_in_the_expected_direction() {
    let model = BenefitModel::measured_default();
    let base = region_benefit(&model, 100, 4096);

    // More requests -> strictly more benefit.
    assert!(region_benefit(&model, 200, 4096) > base);
    // Costlier build -> strictly less benefit.
    assert!(region_benefit(&model, 100, 8192) < base);

    // A larger latency saving (lower L_hot) raises benefit.
    let mut faster = model;
    faster.l_hot_ns = model.l_hot_ns / 2.0;
    assert!(region_benefit(&faster, 100, 4096) > base);

    // A longer amortisation horizon lowers the amortised cost and raises benefit.
    let mut long = model;
    long.horizon_windows = model.horizon_windows * 4.0;
    assert!(region_benefit(&long, 100, 4096) > base);

    // Too few requests and the build is not amortised: benefit is negative.
    assert!(region_benefit(&model, 0, 4096) < 0.0);
}

#[test]
fn plan_selects_only_positive_benefit_within_budget_and_is_deterministic() {
    let model = BenefitModel::measured_default();
    // Two very hot regions, one cold, one moderate.
    let obs = synthetic_obs(vec![1000, 900, 0, 40]);
    let budget = 3 * 4096;
    let plan = plan_benefits(&obs, 4096, &model, budget);

    // Only positive-benefit regions are eligible.
    for e in &plan.entries {
        if e.benefit <= 0.0 {
            assert!(!e.selected, "non-positive benefit selected");
        }
    }
    // Respects the byte budget.
    assert!(plan.total_bytes <= budget);
    assert_eq!(
        plan.total_bytes,
        plan.entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.bytes)
            .sum::<u64>()
    );
    // Ranked by benefit (highest first).
    let mut prev = f64::INFINITY;
    for &r in &plan.selected_regions {
        let b = plan.entries[r as usize].benefit;
        assert!(b <= prev, "selection not benefit-ordered");
        prev = b;
    }
    // Deterministic.
    let again = plan_benefits(&obs, 4096, &model, budget);
    assert_eq!(plan, again);

    // Budget too small for even one region selects nothing.
    let empty = plan_benefits(&obs, 4096, &model, 1);
    assert!(empty.is_empty());
}

// ---------------------------------------------------------------------------
// Atomic publication
// ---------------------------------------------------------------------------

fn root_with_checkpoints(src: &[u8]) -> EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    body
}

fn manifest_region(body: &mut EkbloomBody, start: u64, len: u64) {
    let trace = ekbloom::AccessTrace {
        regions: vec![(start, len)],
        counts: vec![1000],
    };
    ekbloom::TileSet::manifest_hot_tiles(
        body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();
}

#[test]
fn publication_is_additive_committed_history_untouched_and_reads_never_partial() {
    let src = lcg(0x9E37_0000_0000_0001, 256 * 1024);
    let n = src.len() as u64;
    let baseline = root_with_checkpoints(&src);
    let committed_hash = baseline.core_hash;
    let committed_tiles = baseline.tiles.len();
    let publication = Publication::new(baseline).unwrap();

    // A reader caches the initial generation.
    let mut reader = PlannerReader::new(Arc::clone(&publication));
    assert_eq!(reader.cached_version(), 0);
    let old = Arc::clone(&publication.load());
    let locks_before = publication.reader_lock_acquisitions();

    // Publish a new generation that adds tiles over the first region.
    let mut next = reader.current().body().clone();
    assert_eq!(next.tiles.len(), committed_tiles);
    manifest_region(&mut next, 0, 64 * 1024);
    let version = publication
        .publish(
            next,
            ResidencyCache {
                hot_regions: vec![0],
                resident_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(publication.version(), 1);

    // The old snapshot is completely unchanged (RCU non-withdrawal).
    assert_eq!(old.version(), 0);
    assert_eq!(old.body().core_hash, committed_hash);
    assert_eq!(old.body().tiles.len(), committed_tiles);
    assert!(old.body().history.is_none());

    // The reader adopts the new generation on refresh; committed history is intact.
    reader.refresh();
    assert_eq!(reader.cached_version(), 1);
    assert_eq!(reader.current().body().core_hash, committed_hash);
    assert!(reader.current().body().tiles.len() > committed_tiles);
    assert!(reader.current().body().history.is_none());

    // Reads are byte-exact across the materialised boundary and never partial.
    for (off, len) in [(0u64, 1u64), (64 * 1024 - 3, 7), (64 * 1024, 5), (n - 7, 7)] {
        let got = reader
            .read(ReadRequest {
                offset: off,
                length: len,
            })
            .unwrap();
        assert_eq!(got, &src[off as usize..(off + len) as usize]);
    }

    // The read path itself takes no publication lock: only the two `load`s above
    // (one from `Publication::new`'s reader + one explicit) were counted.
    let locks_after = publication.reader_lock_acquisitions();
    assert_eq!(
        locks_after - locks_before,
        1,
        "unexpected read-path locking"
    );
}

// ---------------------------------------------------------------------------
// Background planner
// ---------------------------------------------------------------------------

#[test]
fn background_planner_adapts_additively_within_budget_under_concurrent_load() {
    let src = lcg(0x4242_0000_0000_0001, 512 * 1024);
    let n = src.len() as u64;
    let baseline = root_with_checkpoints(&src);
    let committed_hash = baseline.core_hash;
    let committed_tiles = baseline.tiles.len();
    let initial_physical = baseline.physical_bytes();
    let budget = initial_physical + 512 * 1024;

    let publication = Publication::new(baseline).unwrap();
    let observer = Arc::new(AccessObserver::new(n, 64 * 1024, 8).unwrap());
    // Seed the hot region (the first 64 KiB) with many observations.
    for _ in 0..2000 {
        observer.observe(1234, 1).unwrap();
    }

    let cfg = BackgroundPlannerConfig {
        interval: Duration::from_millis(5),
        region_bytes: 64 * 1024,
        tile_shift: 12,
        directory_kind: DirectoryKind::TwoLevelRadix,
        budget: ManifestationBudget {
            maximum_physical_bytes: budget,
        },
        model: BenefitModel::measured_default(),
        max_adaptations: 2,
    };
    let mut planner =
        BackgroundPlanner::spawn(Arc::clone(&publication), Arc::clone(&observer), cfg);

    // Concurrent readers hammer a hot and a cold region while the planner adapts.
    let src_shared = Arc::new(src.clone());
    let errors = Arc::new(AtomicU64::new(0));
    let tail = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let publication = Arc::clone(&publication);
        let errors = Arc::clone(&errors);
        let tail = Arc::clone(&tail);
        let src_shared = Arc::clone(&src_shared);
        handles.push(std::thread::spawn(move || {
            let mut reader = PlannerReader::new(publication);
            let deadline = Instant::now() + Duration::from_millis(400);
            while Instant::now() < deadline {
                for off in [100u64, 300_000u64, 400_000u64, 1] {
                    reader.refresh();
                    let t0 = Instant::now();
                    match reader.read(ReadRequest {
                        offset: off,
                        length: 1,
                    }) {
                        Ok(bytes) => {
                            if bytes != [src_shared[off as usize]] {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let dt = t0.elapsed().as_nanos() as u64;
                    tail.fetch_max(dt, Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    planner.stop();
    let stats = planner.stats();

    assert_eq!(
        errors.load(Ordering::Relaxed),
        0,
        "a read failed or was wrong"
    );
    assert!(stats.adaptations >= 1, "planner never adapted: {stats:?}");
    assert!(stats.bytes_spent > 0);
    assert!(stats.regions_manifested > 0);

    let final_body = publication.load();
    assert_eq!(
        final_body.body().core_hash,
        committed_hash,
        "committed core changed"
    );
    assert!(
        final_body.body().tiles.len() >= committed_tiles,
        "committed tiles shrank"
    );
    assert!(
        final_body.body().history.is_none(),
        "committed history touched"
    );
    assert!(
        final_body.body().physical_bytes() <= budget,
        "planner exceeded the budget"
    );
    // Every materialised tile is certified against the core.
    for ts in &final_body.body().tiles {
        assert!(ts.verify_against(&final_body.body().core).unwrap());
    }
    // The published plan still dispatches only to certified paths.
    assert!(final_body.plan().all_certified());
    // The hot region was promoted exactly as planned.
    assert!(final_body.body().tiles.len() > committed_tiles);
    assert_eq!(final_body.plan().path_for(1234), AccessPath::HotBytes);

    // Read-tail sanity: no read blocked for an unreasonable time under the planner.
    assert!(
        tail.load(Ordering::Relaxed) < 100_000_000,
        "a read tail spiked to {} ns",
        tail.load(Ordering::Relaxed)
    );
    assert!(stats.epochs >= stats.adaptations);
}
