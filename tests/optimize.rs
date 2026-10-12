//! The physical-budget-aware joint planner (`src/optimize.rs`).
//!
//! These tests pin the contract the joint optimiser must satisfy:
//!
//! * the chosen plan's **committed** bytes respect the budget and only reference
//!   **certified** paths;
//! * the built body reads **byte-exactly** against the authoritative
//!   [`ekbloom::read`];
//! * the optimiser is **deterministic** for a fixed seed/trace;
//! * a **receipt** is produced for every candidate considered;
//! * evaluation uses the **held-out** trace, never the evaluation trace
//!   (structural: the plan records the held-out trace's fingerprint);
//! * the plan is **additive** — [`OptimizedPlan::grow_into`] never removes an
//!   earlier generation's structures.

use ekbloom::{
    create_machine, evaluate, trace_fingerprint, AccessTrace, CourtConfig, EkbloomBody,
    ManifestationBudget, ManifestationProfile, OptimizedPlan, ReadRequest,
};

/// A heterogeneous corpus: a low-entropy skewed half followed by a uniform half.
fn heterogeneous(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let half = len / 2;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let byte = (s >> 33) as u8;
        out.push(if i < half { byte & 0x07 } else { byte });
    }
    out
}

/// A skewed held-out/evaluation trace: a uniform spread of small regions plus one
/// heavily-accessed hot region in the far (uniform) half.
fn hot_trace(n: u64, salt: u64) -> AccessTrace {
    let region = 16 * 1024u64;
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    let mut pos = 0u64;
    while pos + region <= n {
        regions.push((pos, region));
        counts.push(4 + (pos.wrapping_add(salt) % 9));
        pos += region;
    }
    let hot = (n / 4) * 3;
    let hot = hot.min(n.saturating_sub(region));
    regions.push((hot, region.min(n - hot)));
    counts.push(4000);
    AccessTrace { regions, counts }
}

fn cfg() -> CourtConfig {
    CourtConfig {
        reps: 3,
        interleave: true,
        seed: 0x0E71_0000_0000_0001,
    }
}

fn budget(bytes: u64) -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: bytes,
    }
}

const SRC_LEN: usize = 128 * 1024;

fn budgets(n: u64) -> [u64; 2] {
    [n + n / 8, n + n]
}

#[test]
fn joint_plan_respects_budget_is_certified_and_reads_byte_exactly() {
    let src = heterogeneous(0x0E71_0000_0000_0010, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 7);

    for limit in budgets(n) {
        let (plan, receipts) =
            OptimizedPlan::optimize(&src, &trace, &budget(limit), &cfg()).unwrap();
        assert!(plan.committed_bytes() <= limit, "committed over budget");
        assert!(!receipts.is_empty());

        let body = plan.build(&src).unwrap();
        assert_eq!(body.physical_bytes(), plan.committed_bytes());
        assert!(body.physical_bytes() <= limit);
        assert!(
            ekbloom::verify_deep(&body).unwrap().ok,
            "planned body must pass deep verification"
        );

        let engine = plan.engine(&body).unwrap();
        assert!(
            engine.plan_ref().all_certified(),
            "plan must reference certified paths only"
        );
        assert_eq!(engine.plan_ref().committed_bytes(), plan.committed_bytes());

        // Byte-exact against the authoritative reader over the whole object.
        for &(start, len) in &[(0u64, 1u64), (1, 4095), (n / 2, 777), (n - 1, 1), (0, n)] {
            let req = ReadRequest {
                offset: start,
                length: len,
            };
            let got = engine.read(&body, req).unwrap();
            let want = ekbloom::read(&body, req).unwrap();
            assert_eq!(got, want, "engine differs from reader at {start}+{len}");
            assert_eq!(&got, &src[start as usize..(start + len) as usize]);
        }
    }
}

#[test]
fn optimiser_is_deterministic_for_a_fixed_seed_and_trace() {
    let src = heterogeneous(0x0E71_0000_0000_0020, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 11);
    let b = budget(n + n / 4);

    let (a, ra) = OptimizedPlan::optimize(&src, &trace, &b, &cfg()).unwrap();
    let (c, rc) = OptimizedPlan::optimize(&src, &trace, &b, &cfg()).unwrap();
    assert_eq!(a.summary(), c.summary());
    assert_eq!(a.committed_bytes(), c.committed_bytes());
    assert_eq!(a.hot_regions(), c.hot_regions());
    assert_eq!(ra.len(), rc.len());
    assert_eq!(
        a.build(&src).unwrap(),
        c.build(&src).unwrap(),
        "build must be byte-identical across runs"
    );
}

#[test]
fn receipts_are_produced_and_explain_the_choice() {
    let src = heterogeneous(0x0E71_0000_0000_0030, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 3);
    let (plan, receipts) =
        OptimizedPlan::optimize(&src, &trace, &budget(n + n / 2), &cfg()).unwrap();
    assert!(receipts.len() >= 2, "expected several candidate receipts");
    assert_eq!(receipts.len(), plan.receipts().len());
    assert!(
        receipts.iter().any(|r| r.reason.contains("chosen")),
        "a chosen receipt must be present"
    );
    assert!(
        receipts
            .iter()
            .any(|r| r.committed_bytes > 0 && r.expected_latency.requests > 0),
        "receipts must carry committed bytes and an expected latency"
    );
}

#[test]
fn held_out_evaluation_never_uses_the_evaluation_trace() {
    let src = heterogeneous(0x0E71_0000_0000_0040, SRC_LEN);
    let n = src.len() as u64;
    // The held-out trace drives the optimiser; the evaluation trace is *different*
    // and is only used to score the finished plan.
    let held_out = hot_trace(n, 5);
    let evaluation = hot_trace(n, 9001);
    assert_ne!(trace_fingerprint(&held_out), trace_fingerprint(&evaluation));

    let (plan, _) = OptimizedPlan::optimize(&src, &held_out, &budget(n + n / 2), &cfg()).unwrap();
    assert_eq!(plan.held_out_fingerprint(), trace_fingerprint(&held_out));
    assert_ne!(plan.held_out_fingerprint(), trace_fingerprint(&evaluation));

    // The recorded latency is exactly the held-out trace's evaluation, not the
    // evaluation trace's.
    let body = plan.build(&src).unwrap();
    let engine = plan.engine(&body).unwrap();
    let on_held_out = evaluate(
        &engine,
        &held_out,
        &ekbloom::CostModel::measured_default(),
        ekbloom::OPTIMIZER_READ_BYTES,
    );
    assert!((plan.expected_p99() - on_held_out.p99).abs() < 1e-9);
}

#[test]
fn grow_into_is_additive_and_preserves_earlier_generations() {
    let src = heterogeneous(0x0E71_0000_0000_0050, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 13);
    let (plan, _) = OptimizedPlan::optimize(&src, &trace, &budget(2 * n), &cfg()).unwrap();

    // An earlier generation: the plan's machine, core + one checkpoint.
    let mut earlier: EkbloomBody = create_machine(
        &src,
        ManifestationProfile::Sparse { interval: 64 },
        plan.machine_id(),
    )
    .unwrap();
    let core_hash = earlier.core_hash;
    let checkpoints_before = earlier.checkpoints.clone();
    let blooms_before = earlier.blooms.clone();
    let generation_before = earlier.generation;

    plan.grow_into(&mut earlier).unwrap();

    // Nothing earlier was removed or rewritten.
    assert_eq!(earlier.core_hash, core_hash);
    assert_eq!(
        &earlier.checkpoints[..checkpoints_before.len()],
        &checkpoints_before[..]
    );
    assert!(earlier.checkpoints.len() >= checkpoints_before.len());
    assert!(earlier.blooms.len() >= blooms_before.len());
    assert!(earlier.generation > generation_before);
    assert!(earlier.physical_bytes() <= plan.budget_bytes());
    assert!(ekbloom::verify_deep(&earlier).unwrap().ok);

    // Building the plan's fresh body twice is byte-identical (deterministic,
    // additive construction).
    assert_eq!(plan.build(&src).unwrap(), plan.build(&src).unwrap());
}

#[test]
fn hot_dominated_trace_selects_hot_tiles() {
    // When the held-out trace is dominated by one region, p99 is that region's
    // latency, so the joint optimiser must spend budget on its tiles.
    let src = heterogeneous(0x0E71_0000_0000_0070, SRC_LEN);
    let n = src.len() as u64;
    let trace = AccessTrace {
        regions: vec![(0, 4096), (n - 16 * 1024, 16 * 1024)],
        counts: vec![1_000_000, 1],
    };
    let (plan, _) = OptimizedPlan::optimize(&src, &trace, &budget(2 * n), &cfg()).unwrap();
    assert!(
        plan.tile_shift().is_some() && !plan.hot_regions().is_empty(),
        "a hot-dominated trace must select hot-region tiles: {}",
        plan.summary()
    );
    let body = plan.build(&src).unwrap();
    let engine = plan.engine(&body).unwrap();
    assert!(engine.plan_ref().all_certified());
    let req = ReadRequest {
        offset: 0,
        length: 4096,
    };
    assert_eq!(engine.read(&body, req).unwrap(), &src[..4096]);
}

#[test]
fn over_budget_source_is_rejected_transactionally() {
    let src = heterogeneous(0x0E71_0000_0000_0060, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 1);
    // A budget below even a raw core cannot admit any plan.
    let err = OptimizedPlan::optimize(&src, &trace, &budget(1024), &cfg()).unwrap_err();
    assert!(matches!(
        err,
        ekbloom::EkbloomError::AllocationTooLarge { .. }
    ));
}
