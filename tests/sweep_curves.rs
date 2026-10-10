//! Integration tests for the extended manifestation sweep: the budget frontier
//! and the region-size / depth / checkpoint-interval curves.
//!
//! All tests run on a small (64 KiB) deterministic corpus and clamp the sweep
//! iteration budget so the whole file stays fast.

use ekbloom::sweep::{
    budget_frontier, cache_resident_seq_bytes_per_s, checkpoint_interval_curve, cold_start_ns,
    depth_curve, region_size_curve, BudgetPoint, BUDGET_MIN_FLOOR_BYTES,
};
use ekbloom::{ManifestationProfile, SweepConfig};

/// A small deterministic corpus for every test in this file.
const CORPUS: usize = 64 * 1024;

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

fn cfg() -> SweepConfig {
    SweepConfig {
        iterations: 4,
        seed: 0x1234_5678_9ABC_DEF0,
    }
}

/// Mirror of the module's budget rule: `max(visible * ratio, floor)`.
fn budget(visible: u64, ratio: f64) -> u64 {
    let scaled = visible as f64 * ratio;
    let cap = if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    };
    cap.max(BUDGET_MIN_FLOOR_BYTES)
}

#[test]
fn budget_frontier_is_monotone_and_within_budget() {
    let src = lcg(0x11, CORPUS);
    let visible = src.len() as u64;
    let ratios = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0];
    let rows = budget_frontier(&src, &ratios, &cfg()).unwrap();
    assert_eq!(rows.len(), ratios.len());

    let mut prev = 0u64;
    let mut chosen_any = false;
    for (row, &ratio) in rows.iter().zip(ratios.iter()) {
        assert_eq!(row.target_ratio, ratio);
        // The chosen footprint never exceeds the budget for its ratio.
        assert!(
            row.physical_bytes <= budget(visible, ratio),
            "ratio {ratio}: physical {} exceeds budget {}",
            row.physical_bytes,
            budget(visible, ratio)
        );
        // More budget never selects a strictly smaller footprint.
        assert!(
            row.physical_bytes >= prev,
            "ratio {ratio}: physical {} smaller than previous {}",
            row.physical_bytes,
            prev
        );
        prev = row.physical_bytes;

        if row.physical_bytes > 0 {
            chosen_any = true;
            assert!(!row.chosen_profile.is_empty());
            let expect = row.physical_bytes as f64 / visible as f64;
            assert!(
                (row.achieved_ratio - expect).abs() < 1e-9,
                "ratio {ratio}: achieved {} != {}",
                row.achieved_ratio,
                expect
            );
            assert!(row.seq_bytes_per_s.is_finite() && row.seq_bytes_per_s > 0.0);
            assert!(row.random_1b_ns_p50.is_finite());
            assert!(row.random_4k_ns_p50.is_finite());
        } else {
            assert!(row.chosen_profile.is_empty());
            assert_eq!(row.achieved_ratio, 0.0);
        }
    }
    assert!(chosen_any, "no ratio chose a profile on a 64 KiB corpus");
}

#[test]
fn ratio_one_selects_the_root_object() {
    let src = lcg(0x22, CORPUS);
    let rows = budget_frontier(&src, &[1.0], &cfg()).unwrap();
    assert_eq!(rows.len(), 1);
    // Exactly the root object fits `N` bytes; any manifestation adds bytes.
    assert_eq!(rows[0].chosen_profile, "root");
    assert_eq!(rows[0].physical_bytes, src.len() as u64);
}

#[test]
fn each_curve_has_one_point_per_label() {
    let src = lcg(0x33, CORPUS);
    let cfg = cfg();

    let region_sizes = [1024u32, 4096, 16384];
    let c = region_size_curve(&src, &cfg, &region_sizes).unwrap();
    assert_eq!(c.axis, "region-size");
    assert_eq!(c.labels.len(), region_sizes.len());
    assert_eq!(c.points.len(), region_sizes.len());
    for (i, p) in c.points.iter().enumerate() {
        assert_eq!(c.labels[i], format!("R={}", region_sizes[i]));
        assert!(
            p.physical_bytes > 0,
            "region point {i} has no physical bytes"
        );
    }

    let depths = [1u16, 4, 32];
    let c = depth_curve(&src, &cfg, &depths).unwrap();
    assert_eq!(c.axis, "path-depth");
    assert_eq!(c.labels.len(), depths.len());
    assert_eq!(c.points.len(), depths.len());
    for (i, p) in c.points.iter().enumerate() {
        assert_eq!(c.labels[i], format!("D={}", depths[i]));
        assert!(
            p.physical_bytes > 0,
            "depth point {i} has no physical bytes"
        );
    }

    let intervals = [1024u32, 64, 16];
    let c = checkpoint_interval_curve(&src, &cfg, &intervals).unwrap();
    assert_eq!(c.axis, "checkpoint-interval");
    assert_eq!(c.labels.len(), intervals.len());
    assert_eq!(c.points.len(), intervals.len());
    for (i, p) in c.points.iter().enumerate() {
        assert_eq!(c.labels[i], format!("K={}", intervals[i]));
        assert!(
            p.physical_bytes > 0,
            "interval point {i} has no physical bytes"
        );
    }
}

#[test]
fn cold_start_is_non_negative_and_finite() {
    let src = lcg(0x44, CORPUS);
    let cfg = cfg();
    for profile in [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Full,
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ] {
        let ns = cold_start_ns(&src, profile, &cfg).unwrap();
        assert!(ns >= 0.0, "cold start was negative: {ns}");
        assert!(ns.is_finite());
    }
}

#[test]
fn cache_resident_throughput_is_positive() {
    let bytes_per_s = cache_resident_seq_bytes_per_s(64 * 1024, &cfg()).unwrap();
    assert!(bytes_per_s.is_finite() && bytes_per_s > 0.0);
}

#[test]
fn budget_frontier_is_deterministic_for_a_fixed_seed() {
    let src = lcg(0x55, CORPUS);
    let cfg = cfg();
    let ratios = [1.0, 2.0, 4.0, 8.0, 16.0];
    let a = budget_frontier(&src, &ratios, &cfg).unwrap();
    let b = budget_frontier(&src, &ratios, &cfg).unwrap();

    let key = |rows: &[BudgetPoint]| -> Vec<(String, u64)> {
        rows.iter()
            .map(|p| (p.chosen_profile.clone(), p.physical_bytes))
            .collect()
    };
    assert_eq!(key(&a), key(&b), "same seed chose different profiles");
}
