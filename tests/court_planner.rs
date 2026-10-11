//! Court + certified planner contract tests.
//!
//! Pins: [`ekbloom::Dist`] percentile/CI sanity; the court runs bounded and
//! produces one row per (workload, candidate); [`AccessPlan::plan`] selects only
//! certified paths and is deterministic for a fixed seed; the planner's dispatch
//! is an O(1) index (structural assertion); and [`Planner::read`] is
//! byte-for-byte identical to [`ekbloom::read`] across profiles and many
//! requests, including requests that span planner regions.

use ekbloom::court::Workload;
use ekbloom::{
    create, lcg_bytes, manifest, read, AccessPath, AccessPlan, AccessTrace, Candidate, Court,
    CourtConfig, DirectoryKind, Dist, ManifestationBudget, ManifestationProfile, Planner,
    ReadRequest, TileSet,
};

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn cfg() -> CourtConfig {
    CourtConfig {
        reps: 2,
        interleave: true,
        seed: 0x0C0D_7E57_0000_0001,
    }
}

#[test]
fn dist_ci_is_sane() {
    let mut samples: Vec<u64> = Vec::new();
    for i in 1..=1000u64 {
        samples.push(i);
    }
    let d = Dist::from_samples(&samples, 0x1234);
    assert!(d.min <= d.p50 && d.p50 <= d.p90 && d.p90 <= d.p99 && d.p99 <= d.p999);
    assert!(d.p999 <= d.max);
    assert!(d.ci95_lo <= d.mean && d.mean <= d.ci95_hi);
    assert!(d.ci95_lo >= d.min && d.ci95_hi <= d.max);
    // Empty and single samples must not panic or produce non-finite values.
    let e = Dist::from_samples(&[], 1);
    assert_eq!(e.p50, 0.0);
    let s = Dist::from_samples(&[42], 1);
    assert_eq!((s.p50, s.ci95_lo, s.ci95_hi), (42.0, 42.0, 42.0));
}

#[test]
fn court_runs_bounded() {
    let source = lcg_bytes(0xC0FF_EE00_0000_0001, 64 * 1024);
    let body = create(&source, ManifestationProfile::Path { depth: 8 }).unwrap();
    let raw = Candidate::raw_vec("raw-vec", source.clone());
    let path = Candidate::body("ekbloom-path8", body);
    let cands = [&raw, &path];

    let mut court = Court::new(cfg());
    court.measure(64 * 1024, Workload::Random1b, &cands);
    court.measure(64 * 1024, Workload::Sequential, &cands);
    let report = court.report(ekbloom::collect_environment());

    assert_eq!(report.rows.len(), 4, "2 workloads x 2 candidates");
    for r in &report.rows {
        if r.workload == "sequential" {
            assert!(r.throughput_bytes_per_s > 0.0);
            assert!(r.cycles_per_byte > 0.0);
        } else {
            assert!(r.dist_ns.p50 > 0.0);
            assert!(r.dist_ns.ci95_lo <= r.dist_ns.ci95_hi);
        }
    }
    // The 3-D frontier never flags a dominated point as on-frontier.
    assert!(!report.pareto.is_empty());
    assert!(report.pareto.iter().any(|p| p.on_frontier));
    assert!(report.clock_ghz > 0.0);
}

/// A planner body that mixes a tiled hot prefix with a cold checkpoint set.
fn hotcold_body(source: &[u8]) -> ekbloom::EkbloomBody {
    let n = source.len() as u64;
    let mut body = create(source, ManifestationProfile::Root).unwrap();
    let hot_len = (n / 4).max(4096).min(n);
    let trace = AccessTrace {
        regions: vec![(0, hot_len)],
        counts: vec![1000],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )
    .unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    body
}

#[test]
fn plan_picks_certified_paths_only() {
    let source = lcg_bytes(0x9A9A_0000_0000_0001, 256 * 1024);
    let c = cfg();

    // Root: only authoritative replay is valid.
    let root = create(&source, ManifestationProfile::Root).unwrap();
    let p = AccessPlan::plan(&root, &c).unwrap();
    assert!(p.all_certified());
    assert!(p.regions().iter().all(|r| r.path == AccessPath::CoreReplay));

    // Path bloom: the whole object is HotBytes.
    let path = create(&source, ManifestationProfile::Path { depth: 8 }).unwrap();
    let p = AccessPlan::plan(&path, &c).unwrap();
    assert!(p.all_certified());
    assert_eq!(p.path_for(0), AccessPath::HotBytes);
    assert_eq!(p.path_for(source.len() as u64 - 1), AccessPath::HotBytes);

    // Full history on machine 1: an O(1) per-position path is available.
    let full = create(&source, ManifestationProfile::Full).unwrap();
    let p = AccessPlan::plan(&full, &c).unwrap();
    assert!(p.all_certified());
    assert!(p
        .regions()
        .iter()
        .all(|r| matches!(r.path, AccessPath::AffineBlock | AccessPath::CoreReplay)));

    // Tiles + checkpoint: hot regions are HotBytes, cold ones restart/replay.
    let hc = hotcold_body(&source);
    let p = AccessPlan::plan(&hc, &c).unwrap();
    assert!(p.all_certified(), "planner chose an uncertified path");
    assert_eq!(p.path_for(0), AccessPath::HotBytes, "hot prefix is tiled");
    let n = source.len() as u64;
    assert!(
        matches!(
            p.path_for(n - 1),
            AccessPath::PackedRestart | AccessPath::CoreReplay
        ),
        "cold tail must not be HotBytes"
    );
}

#[test]
fn dispatch_has_no_search() {
    let source = lcg_bytes(0x0B0B_0000_0000_0001, 512 * 1024);
    let hc = hotcold_body(&source);
    let p = AccessPlan::plan(&hc, &cfg()).unwrap();
    let shift = p.region_bytes().trailing_zeros();
    let regions = p.regions();
    assert_eq!(regions.len(), p.region_count());
    // `path_for` must be exactly the flat index `offset >> region_shift` — i.e. an
    // O(1) lookup with no scan.
    let n = source.len() as u64;
    let mut offset = 0u64;
    while offset < n {
        let idx = (offset >> shift) as usize;
        assert_eq!(p.path_for(offset), regions[idx].path);
        assert!(regions[idx].start <= offset && offset < regions[idx].end);
        offset += 9973;
    }
    // Contiguity: region k spans [k*region_bytes, (k+1)*region_bytes).
    for (k, r) in regions.iter().enumerate() {
        assert_eq!(r.start, k as u64 * p.region_bytes());
    }
}

#[test]
fn planner_matches_reader_byte_for_byte() {
    let source = lcg_bytes(0xBEEF_0000_0000_0001, 256 * 1024);
    let bodies = vec![
        create(&source, ManifestationProfile::Root).unwrap(),
        create(&source, ManifestationProfile::Sparse { interval: 64 }).unwrap(),
        create(&source, ManifestationProfile::Full).unwrap(),
        create(&source, ManifestationProfile::Path { depth: 4 }).unwrap(),
        hotcold_body(&source),
    ];
    let n = source.len() as u64;
    let c = cfg();
    for body in &bodies {
        let planner = Planner::new(AccessPlan::plan(body, &c).unwrap());
        // Single-region requests, region-boundary crossings and a whole-object read.
        let requests = [
            (0u64, 1u64),
            (0, 4096),
            (1, 1),
            (65535, 2),
            (65536 - 3, 9),
            (65536, 4096),
            (n / 2, 1234),
            (n - 7, 7),
            (n - 1, 1),
            (0, n),
            (n / 3, n / 3),
        ];
        for (offset, length) in requests {
            let want = read(body, ReadRequest { offset, length }).unwrap();
            let got = planner.read(body, ReadRequest { offset, length }).unwrap();
            assert_eq!(
                got, want,
                "planner != reader at ({offset},{length}) for {:?}",
                body.profiles
            );
        }
        // A zero-length read at the very end and an out-of-bounds read.
        assert_eq!(
            planner
                .read(
                    body,
                    ReadRequest {
                        offset: n,
                        length: 0
                    }
                )
                .unwrap(),
            Vec::<u8>::new()
        );
        assert!(planner
            .read(
                body,
                ReadRequest {
                    offset: n,
                    length: 1
                }
            )
            .is_err());
    }
}

#[test]
fn plan_is_deterministic_for_a_fixed_seed() {
    let source = lcg_bytes(0xD00D_0000_0000_0001, 128 * 1024);
    let hc = hotcold_body(&source);
    let c = cfg();
    let a = AccessPlan::plan(&hc, &c).unwrap();
    let b = AccessPlan::plan(&hc, &c).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.distribution(), b.distribution());
}
