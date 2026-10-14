//! Integration tests for the online workload-adaptation controller
//! (`src/controller.rs`).
//!
//! These pin the control contract:
//!
//! * **additive / non-withdrawal** — a `step` only ever appends; earlier
//!   generations stay byte-identical and still verify;
//! * **budgets** — the committed body never exceeds `memory_budget`, and a single
//!   epoch never constructs more than the background-I/O budget admits;
//! * **hysteresis** — a change is not made within the dwell or inside the band;
//! * **determinism** — identical configuration + observation sequence ⇒ identical
//!   actions;
//! * **SLO accounting** — the miss counter is exact;
//! * **byte-exactness** — every read after adaptation equals the source.

use ekbloom::{
    create, manifest, read, verify_deep, ControlConfig, Controller, EkbloomBody,
    ManifestationProfile, ReadRequest,
};

const REGION: u64 = 4096;

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

/// The base committed codec body: `Root` plus a bounded cold-restart checkpoint set.
fn base_body(src: &[u8]) -> EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    body
}

fn config(body: &EkbloomBody, allowance: u64, hysteresis: u32, bg_io: u64) -> ControlConfig {
    ControlConfig {
        memory_budget: body.physical_bytes().saturating_add(allowance),
        slo_p99_ns: 500,
        hysteresis,
        max_bg_io_bytes_per_s: bg_io,
        construction_budget_ns: 1 << 40,
    }
}

fn observe_region(c: &mut Controller, region: u64, reads: u64, latency: u64) {
    for _ in 0..reads {
        c.observe(
            ReadRequest {
                offset: region * REGION,
                length: REGION,
            },
            latency,
        );
    }
}

#[test]
fn controller_is_additive_and_reads_byte_exactly() {
    let src = lcg_bytes(0xC011_7011_0000_0001, 256 * 1024);
    let mut body = base_body(&src);
    let core_hash = body.core_hash;
    let gen0 = body.generation;
    let checkpoints_before = body.checkpoints.len();
    let blooms_before = body.blooms.len();

    let cfg = config(&body, 256 * 1024, 1, 1 << 30);
    let mut c = Controller::new(cfg, &body).unwrap();

    // Manifest a hot neighbourhood in the first epoch.
    for _ in 0..4 {
        for &r in &[3u64, 4, 5] {
            observe_region(&mut c, r, 16, 100);
        }
    }
    let a1 = c.step(&mut body).unwrap();
    assert!(a1.changed);
    assert!(body.generation > gen0);
    assert_eq!(body.core_hash, core_hash, "core was rewritten");
    assert_eq!(body.checkpoints.len(), checkpoints_before);
    assert_eq!(body.blooms.len(), blooms_before);
    assert_eq!(body.tiles.len(), 1, "one generation appended");
    let first_tile_bytes = body.tiles[0].to_bytes();
    assert!(body.tiles.iter().all(|t| t.verify()));
    assert!(verify_deep(&body).unwrap().ok);

    for &off in &[
        0u64,
        REGION,
        3 * REGION,
        5 * REGION,
        body.logical_len - REGION,
    ] {
        let got = read(
            &body,
            ReadRequest {
                offset: off,
                length: REGION,
            },
        )
        .unwrap();
        assert_eq!(&got, &src[off as usize..(off + REGION) as usize]);
    }

    // A second adaptation of a different region must not rewrite the first.
    for _ in 0..4 {
        observe_region(&mut c, 40, 16, 100);
    }
    let a2 = c.step(&mut body).unwrap();
    assert!(a2.changed);
    assert_eq!(body.tiles.len(), 2, "second generation appended");
    assert_eq!(
        body.tiles[0].to_bytes(),
        first_tile_bytes,
        "earlier generation rewritten"
    );
    assert!(body.tiles.iter().all(|t| t.verify()));
    assert!(verify_deep(&body).unwrap().ok);

    // A range spanning a tile boundary is still byte-exact.
    let got = read(
        &body,
        ReadRequest {
            offset: 3 * REGION,
            length: 2 * REGION,
        },
    )
    .unwrap();
    assert_eq!(&got, &src[(3 * REGION) as usize..(5 * REGION) as usize]);
}

#[test]
fn controller_respects_memory_budget() {
    let src = lcg_bytes(0xC011_7011_0000_0002, 256 * 1024);
    let mut body = base_body(&src);
    let allowance = 8 * REGION + 8192;
    let memory_budget = body.physical_bytes().saturating_add(allowance);
    let cfg = config(&body, allowance, 1, 1 << 30);
    assert_eq!(cfg.memory_budget, memory_budget);
    let mut c = Controller::new(cfg, &body).unwrap();

    let mut charged = 0u64;
    for _ in 0..6 {
        for &r in &[0u64, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
            observe_region(&mut c, r, 16, 100);
        }
        let a = c.step(&mut body).unwrap();
        assert!(
            body.physical_bytes() <= memory_budget,
            "committed {} exceeds memory budget {}",
            body.physical_bytes(),
            memory_budget
        );
        assert!(a.bg_io_bytes_charged <= cfg.max_bg_io_bytes_per_s);
        charged = charged.saturating_add(a.bg_io_bytes_charged);
    }
    let tiles: u64 = body.tiles.iter().map(|t| t.present_tiles()).sum();
    assert!(
        tiles * REGION <= allowance + 8192,
        "tiles exceeded allowance"
    );
    assert!(charged <= allowance + 8192);
    assert!(verify_deep(&body).unwrap().ok);
}

#[test]
fn controller_respects_background_io_budget() {
    let src = lcg_bytes(0xC011_7011_0000_0003, 256 * 1024);
    let mut body = base_body(&src);
    // 8 KiB/s ⇒ a full bucket admits one 4 KiB tile (plus its directory) and no
    // more; the next epoch's refill cannot fund a second tile.
    let cfg = config(&body, 1 << 20, 1, 8192);
    let mut c = Controller::new(cfg, &body).unwrap();
    for &r in &[0u64, 1, 2, 3, 4, 5, 6, 7] {
        observe_region(&mut c, r, 16, 100);
    }
    let a = c.step(&mut body).unwrap();
    assert!(a.changed);
    assert_eq!(
        a.regions_manifested, 1,
        "background-I/O budget must cap the epoch at one tile"
    );
    assert!(a.bg_io_bytes_charged <= 8192);

    for &r in &[8u64, 9, 10, 11] {
        observe_region(&mut c, r, 16, 100);
    }
    let b = c.step(&mut body).unwrap();
    assert!(!b.changed, "refill must not fund a second tile this soon");
}

#[test]
fn hysteresis_dwell_prevents_immediate_rechange() {
    let src = lcg_bytes(0xC011_7011_0000_0004, 256 * 1024);
    let mut body = base_body(&src);
    let cfg = config(&body, 1 << 20, 4, 1 << 30);
    let mut c = Controller::new(cfg, &body).unwrap();

    for _ in 0..2 {
        observe_region(&mut c, 1, 16, 100);
    }
    assert!(c.step(&mut body).unwrap().changed);

    observe_region(&mut c, 2, 16, 100);
    assert!(
        !c.step(&mut body).unwrap().changed,
        "must not change within the dwell"
    );

    let mut changed_at = None;
    for epoch in 3..=7u64 {
        observe_region(&mut c, 2, 16, 100);
        if c.step(&mut body).unwrap().changed {
            changed_at = Some(epoch);
            break;
        }
    }
    assert_eq!(changed_at, Some(5), "change allowed once the dwell elapsed");
}

#[test]
fn hysteresis_band_blocks_marginal_shifts() {
    let src = lcg_bytes(0xC011_7011_0000_0005, 256 * 1024);
    let mut body = base_body(&src);
    // hysteresis 2 ⇒ dwell 2 epochs and a band of 3x.
    let cfg = config(&body, 1 << 20, 2, 1 << 30);
    let mut c = Controller::new(cfg, &body).unwrap();

    for _ in 0..2 {
        observe_region(&mut c, 1, 16, 100);
    }
    assert!(c.step(&mut body).unwrap().changed);

    // Dwell blocks the immediate attempt.
    observe_region(&mut c, 1, 10, 100);
    observe_region(&mut c, 2, 20, 100);
    assert!(!c.step(&mut body).unwrap().changed);

    // Dwell has elapsed, but region 2 (20) is within 3x of region 1 (10).
    observe_region(&mut c, 1, 10, 100);
    observe_region(&mut c, 2, 20, 100);
    assert!(
        !c.step(&mut body).unwrap().changed,
        "must not change inside the hysteresis band"
    );

    // Region 2 now far exceeds the band → adapt.
    observe_region(&mut c, 1, 2, 100);
    observe_region(&mut c, 2, 200, 100);
    assert!(
        c.step(&mut body).unwrap().changed,
        "must adapt outside the band"
    );
}

#[test]
fn controller_is_deterministic_for_fixed_inputs() {
    let src = lcg_bytes(0xC011_7011_0000_0006, 256 * 1024);
    let base = base_body(&src);

    let run = || -> (Vec<(bool, u64, u64)>, u64, u64) {
        let mut body = base.clone();
        let cfg = config(&body, 1 << 20, 2, 1 << 20);
        let mut c = Controller::new(cfg, &body).unwrap();
        let mut actions = Vec::new();
        for epoch in 0..12u64 {
            let region = (epoch * 7) % 40;
            for &r in &[region, region + 1, region + 2] {
                observe_region(&mut c, r % 64, 16, 100);
            }
            let a = c.step(&mut body).unwrap();
            actions.push((a.changed, a.added_physical_bytes, c.adaptations()));
        }
        (actions, body.generation, body.physical_bytes())
    };

    let first = run();
    let second = run();
    assert_eq!(first, second, "controller is not deterministic");
    let (actions, _, _) = &first;
    assert!(actions.iter().any(|&(changed, _, _)| changed));
}

#[test]
fn slo_miss_counter_is_accurate() {
    let src = lcg_bytes(0xC011_7011_0000_0007, 64 * 1024);
    let body = base_body(&src);
    let cfg = ControlConfig {
        memory_budget: body.physical_bytes() + (1 << 20),
        slo_p99_ns: 100,
        hysteresis: 1,
        max_bg_io_bytes_per_s: 1 << 30,
        construction_budget_ns: 1 << 40,
    };
    let mut c = Controller::new(cfg, &body).unwrap();
    for &lat in &[10u64, 100, 101, 50, 99, 1000, 100] {
        c.observe(
            ReadRequest {
                offset: 0,
                length: 1,
            },
            lat,
        );
    }
    assert_eq!(c.observed(), 7);
    assert_eq!(c.slo_misses(), 2, "only 101 and 1000 exceed the SLO");
}

#[test]
fn step_without_observation_is_a_noop() {
    let src = lcg_bytes(0xC011_7011_0000_0008, 64 * 1024);
    let mut body = base_body(&src);
    let generation = body.generation;
    let physical = body.physical_bytes();
    let cfg = config(&body, 1 << 20, 1, 1 << 30);
    let mut c = Controller::new(cfg, &body).unwrap();
    let a = c.step(&mut body).unwrap();
    assert!(!a.changed);
    assert_eq!(body.generation, generation);
    assert_eq!(body.physical_bytes(), physical);
    assert_eq!(c.adaptations(), 0);
    assert_eq!(c.bytes_spent(), 0);
    assert_eq!(c.slo_misses(), 0);
}
