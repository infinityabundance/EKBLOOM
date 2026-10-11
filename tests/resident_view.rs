//! Borrowed resident-view contract tests.
//!
//! Pins the [`ekbloom::ResidentView`] guarantees:
//!
//! * [`ResidentView::new`] resolves the engine's certified resident surface
//!   **once**, for a fully materialised tile set and for a certified whole-object
//!   path bloom;
//! * [`ResidentView::get`] / [`ResidentView::get_checked`] /
//!   [`ResidentView::copy_range`] are **byte-for-byte identical** to
//!   [`ekbloom::read`] at every offset;
//! * the tight `sum += view.get(i)` loop equals the raw `sum += raw[i]` loop;
//! * out-of-range access is total (`get_checked`/`copy_range`) or panics like
//!   slice indexing (`get`);
//! * a mixed-path plan and an empty object are handled explicitly.

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::{
    create, create_machine, read, AccessTrace, BlockIndex, DirectoryKind, EkbloomBody, Engine,
    ManifestationBudget, ManifestationProfile, ReadRequest, ResidentView, TileSet,
};

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn workload4k() -> Workload {
    Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, 4096)],
    }
}

/// Deterministic pseudorandom bytes from a small LCG (no dependencies).
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

/// A body whose whole logical output is materialised as one tile set (a uniform
/// `HotTile` plan, `worst_case_steps == 0`), plus its engine plan.
fn full_tile_body(src: &[u8], machine: u32) -> (EkbloomBody, Engine) {
    let mut body = create_machine(src, ManifestationProfile::Root, machine).unwrap();
    let n = body.logical_len;
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    (body, engine)
}

/// Assert a view is byte-exact against [`read`] for every offset and exercises
/// `get`, `get_checked` and `copy_range` on each.
fn assert_view_byte_exact(view: &ResidentView<'_>, body: &EkbloomBody, src: &[u8]) {
    let n = src.len() as u64;
    assert_eq!(view.len(), n, "view length must equal the logical length");
    assert_eq!(view.base(), 0);
    let mut buf = [0u8; 1];
    for off in 0..n {
        let want = read(
            body,
            ReadRequest {
                offset: off,
                length: 1,
            },
        )
        .unwrap();
        assert_eq!(want, vec![src[off as usize]], "reader mismatch at {off}");
        assert_eq!(view.get(off), src[off as usize], "get mismatch at {off}");
        assert_eq!(
            view.get_checked(off),
            Some(src[off as usize]),
            "get_checked mismatch at {off}"
        );
        assert!(view.copy_range(off, &mut buf), "copy_range rejected {off}");
        assert_eq!(buf[0], src[off as usize], "copy_range mismatch at {off}");
    }
    // Out of range is total for the checked / copy variants.
    assert!(view.get_checked(n).is_none());
    assert!(view.get_checked(u64::MAX).is_none());
    assert!(!view.copy_range(n, &mut buf));
    assert!(!view.copy_range(u64::MAX, &mut buf));
}

#[test]
fn resident_view_is_byte_exact_for_every_machine() {
    for machine in [1u32, 2, 3] {
        for len in [1usize, 4096, 4097, 20 * 1024] {
            let src = lcg(0x4E51_0000_0000_0001 ^ u64::from(machine) ^ len as u64, len);
            let (body, engine) = full_tile_body(&src, machine);
            assert_eq!(engine.worst_case_steps(), 0, "machine {machine} len {len}");
            let view = ResidentView::new(&engine, &body).unwrap();
            assert_eq!(
                Some(view.surface()),
                body.tiles.last().and_then(|ts| ts.hot_surface()),
                "the view must borrow the tile set's hot surface"
            );
            assert_view_byte_exact(&view, &body, &src);
        }
    }
}

#[test]
fn resident_view_rejects_a_mixed_path_plan() {
    // Hot tiles over the first half plus a block index over the whole object: the
    // plan is not uniformly one pure-copy path, so no single contiguous resident
    // slice covers the object.
    let src = lcg(0x4E52_0000_0000_0001, 64 * 1024);
    let n = src.len() as u64;
    let half = (n / 2) as usize;
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let machine = body.machine_id;
    let trace = AccessTrace {
        regions: vec![(0, n / 2)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    body.blocks
        .push(BlockIndex::build_machine(&body.core, n, 1024, machine).expect("block index builds"));
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert!(
        engine.resident_surface(&body).is_none(),
        "a mixed-path plan exposes no single surface"
    );
    assert!(ResidentView::new(&engine, &body).is_err());
    // The first half is genuinely covered by the tile set, so the plan is bounded.
    assert_eq!(
        engine.byte_at(&body, half as u64 - 1).unwrap(),
        src[half - 1]
    );
}

#[test]
fn resident_view_handles_a_certified_path_bloom() {
    // The exact path bloom `P_D` stores the whole decoded output in its `hot`
    // array, so the engine selects a uniform `DirectStorage` plan.
    let src = lcg(0x4E53_0000_0000_0001, 32 * 1024);
    let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(
        engine.plan_ref().path_for(0),
        ekbloom::EnginePath::DirectStorage
    );
    assert!(
        engine.resident_surface(&body).is_some(),
        "a direct-storage plan exposes the path bloom's hot surface"
    );
    let view = ResidentView::new(&engine, &body).unwrap();
    assert_view_byte_exact(&view, &body, &src);
}

#[test]
fn resident_view_handles_an_empty_object() {
    let body = create(&[], ManifestationProfile::Root).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    let view = ResidentView::new(&engine, &body).unwrap();
    assert_eq!(view.len(), 0);
    assert!(view.is_empty());
    assert_eq!(view.base(), 0);
    assert!(view.surface().is_empty());
    assert_eq!(view.get_checked(0), None);
    assert_eq!(view.get_checked(u64::MAX), None);
    // A zero-length copy into an empty destination is in range.
    assert!(view.copy_range(0, &mut []));
}

#[test]
fn resident_view_tight_sum_loop_equals_raw_indexing() {
    let src = lcg(0x4E54_0000_0000_0001, 32 * 1024);
    let (body, engine) = full_tile_body(&src, 3);
    let view = ResidentView::new(&engine, &body).unwrap();
    let n = src.len() as u64;
    let mut sum_raw = 0u64;
    let mut sum_view = 0u64;
    for i in 0..n {
        sum_raw = sum_raw.wrapping_add(u64::from(src[i as usize]));
        sum_view = sum_view.wrapping_add(u64::from(view.get(i)));
    }
    assert_eq!(sum_raw, sum_view);
    std::hint::black_box(sum_view);
}

#[test]
#[should_panic]
fn resident_view_get_panics_past_the_end() {
    let src = lcg(0x4E55_0000_0000_0001, 4096);
    let (body, engine) = full_tile_body(&src, 3);
    let view = ResidentView::new(&engine, &body).unwrap();
    let _ = view.get(src.len() as u64);
}
