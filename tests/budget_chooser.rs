//! The byte-budget + target-p50 chooser (`ekbloom::choose_plan`).
//!
//! These tests pin the chooser's contract:
//!
//! * the chosen plan's **committed** bytes respect the byte budget and only
//!   reference **certified** paths;
//! * the built body reads **byte-exactly** against the authoritative
//!   [`ekbloom::read`];
//! * the chooser is **deterministic** for a fixed `source`/`trace`/budget/seed;
//! * meeting the target is done with the **fewest committed bytes**, and when the
//!   target is unattainable the **lowest-p50** plan within budget is returned;
//! * a **receipt** explains the choice.

use ekbloom::machine::MACHINE_STORE;
use ekbloom::{choose_plan, read as read_body, AccessTrace, CourtConfig, PlanReceipt, ReadRequest};

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

/// A uniform (incompressible) corpus.
fn uniform(seed: u64, len: usize) -> Vec<u8> {
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

/// A skewed held-out trace: small spread regions plus one hot far-half region.
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

/// The chosen receipt (the one whose reason says it was chosen).
fn chosen_receipt(receipts: &[PlanReceipt]) -> &PlanReceipt {
    receipts
        .iter()
        .find(|r| r.reason.contains("chosen"))
        .expect("a chosen receipt must be present")
}

const SRC_LEN: usize = 128 * 1024;

#[test]
fn chooser_respects_budget_is_certified_and_reads_byte_exactly() {
    let src = heterogeneous(0x0E71_0000_0000_1000, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 7);

    // A very large target (every admissible plan meets it) and an unattainable one.
    for (limit, target) in [(n + n / 8, u64::MAX), (2 * n, 0u64)] {
        let (plan, receipts) = choose_plan(&src, limit, target, &trace, &cfg()).unwrap();
        assert!(
            plan.committed_bytes() <= limit,
            "committed {} over budget {limit}",
            plan.committed_bytes()
        );
        assert!(!receipts.is_empty());
        assert_eq!(receipts.len(), plan.receipts().len());

        let body = plan.build(&src).unwrap();
        assert_eq!(body.physical_bytes(), plan.committed_bytes());
        assert!(body.physical_bytes() <= limit);
        assert!(ekbloom::verify_deep(&body).unwrap().ok);

        let engine = plan.engine(&body).unwrap();
        assert!(
            engine.plan_ref().all_certified(),
            "the chooser must reference certified paths only"
        );

        for &(start, len) in &[(0u64, 1u64), (1, 4095), (n / 2, 777), (n - 1, 1), (0, n)] {
            let req = ReadRequest {
                offset: start,
                length: len,
            };
            let got = engine.read(&body, req).unwrap();
            assert_eq!(got, read_body(&body, req).unwrap());
            assert_eq!(&got, &src[start as usize..(start + len) as usize]);
        }
    }
}

#[test]
fn chooser_is_deterministic_for_a_fixed_seed_and_trace() {
    let src = heterogeneous(0x0E71_0000_0000_2000, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 11);
    let limit = n + n / 4;

    let (a, ra) = choose_plan(&src, limit, 900, &trace, &cfg()).unwrap();
    let (c, rc) = choose_plan(&src, limit, 900, &trace, &cfg()).unwrap();
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
fn chooser_trades_bytes_for_target_and_minimises_p50_when_unattainable() {
    let src = heterogeneous(0x0E71_0000_0000_3000, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 3);
    let limit = n + n / 2;

    // Target 0 is unattainable (a real read costs > 0 ns), so this is the
    // lowest-p50 plan within budget. A huge target is met by every admissible
    // plan, so this is the cheapest plan within budget.
    let (lowlat, rl) = choose_plan(&src, limit, 0, &trace, &cfg()).unwrap();
    let (cheap, rc) = choose_plan(&src, limit, u64::MAX, &trace, &cfg()).unwrap();

    let lowlat_p50 = chosen_receipt(&rl).expected_latency.p50;
    let cheap_rec = chosen_receipt(&rc);

    // The lowest-p50 plan can never be slower at p50 than any other eligible plan.
    assert!(
        lowlat_p50 <= cheap_rec.expected_latency.p50,
        "lowest-p50 plan ({lowlat_p50} ns) must not exceed the cheapest target plan ({})",
        cheap_rec.expected_latency.p50
    );
    // The cheapest meeting-target plan can never cost more than the p50-minimiser.
    assert!(
        cheap.committed_bytes() <= lowlat.committed_bytes(),
        "cheapest plan ({}) must not exceed the lowest-p50 plan ({})",
        cheap.committed_bytes(),
        lowlat.committed_bytes()
    );
    // Both receipts explain themselves and identify a chosen candidate.
    assert!(chosen_receipt(&rl).reason.contains("chosen"));
    assert!(chosen_receipt(&rc).reason.contains("chosen"));
    assert!(
        cheap_rec.reason.contains("meets target")
            || cheap_rec.reason.contains("lowest held-out p50"),
        "unexpected chosen reason: {}",
        cheap_rec.reason
    );
}

#[test]
fn chooser_receipts_are_produced_and_explain_the_choice() {
    let src = heterogeneous(0x0E71_0000_0000_4000, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 3);
    let (plan, receipts) = choose_plan(&src, n + n / 2, 0, &trace, &cfg()).unwrap();
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

/// On incompressible data the cheapest eligible plan is the base representation
/// (verbatim machine 3, no structures): a huge target must select it.
#[test]
fn chooser_keeps_uniform_footprint_at_or_below_raw() {
    let src = uniform(0x0E71_0000_0000_5000, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 7);
    let (plan, _) = choose_plan(&src, n + n / 8, u64::MAX, &trace, &cfg()).unwrap();
    assert_eq!(
        plan.machine_id(),
        MACHINE_STORE,
        "uniform data must select the verbatim machine 3"
    );
    assert!(plan.committed_bytes() <= n);
    assert!(plan.tile_shift().is_none());
    assert!(plan.hot_regions().is_empty());

    let body = plan.build(&src).unwrap();
    assert!(ekbloom::verify_deep(&body).unwrap().ok);
    let engine = plan.engine(&body).unwrap();
    assert!(engine.plan_ref().all_certified());
    // Still a real, byte-exact plan.
    let req = ReadRequest {
        offset: 0,
        length: 4096,
    };
    assert_eq!(engine.read(&body, req).unwrap(), &src[..4096]);
}

#[test]
fn chooser_over_budget_source_is_rejected_transactionally() {
    let src = heterogeneous(0x0E71_0000_0000_6000, SRC_LEN);
    let n = src.len() as u64;
    let trace = hot_trace(n, 1);
    let err = choose_plan(&src, 1024, 0, &trace, &cfg()).unwrap_err();
    assert!(matches!(
        err,
        ekbloom::EkbloomError::AllocationTooLarge { .. }
    ));
}
