//! Machine-4 (`OrANS-RANS0I`) first-class cold-engine integration tests.
//!
//! The unified [`Engine`] must plan and read over machine-4 bodies exactly as for
//! machines 1/2/3: the machine-4 block-restart path is enumerated and chosen for
//! cold/positional regions, every served region is certified, and the bytes equal
//! [`reader::read`]. The planner's [`AccessPath`] likewise includes and chooses the
//! machine-4 block path. The prepared machine-4 block decoder
//! (`BlockIndex::read_range_counted_rans_i`) is byte-exact against the unprepared
//! path, all lane counts are exact, and machines 1/2/3 are unaffected.

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::court::CourtConfig;
use ekbloom::machine_rans_i::{decode_measured, encode_measured, encode_with_k, RansIContext};
use ekbloom::{
    create_machine, machine_by_id, reader, AccessPath, AccessPlan, BlockIndex, EkbloomBody, Engine,
    EnginePath, ManifestationBudget, ManifestationProfile, Planner, ReadRequest, StructureRef,
};

const M1: u32 = 1;
const M2: u32 = 2;
const M3: u32 = 3;
const M4: u32 = 4;

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

/// A skewed 4-symbol corpus (machine 2/4 compress it: `V < N`).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(alpha[idx]);
    }
    out
}

/// A machine-4 body carrying a certified block index (`machine_id == 4`).
fn machine4_body(source: &[u8], block_bytes: u32) -> EkbloomBody {
    let mut body = create_machine(source, ManifestationProfile::Root, M4).unwrap();
    let idx = BlockIndex::build_machine(&body.core, body.logical_len, block_bytes, M4).unwrap();
    body.blocks.push(idx);
    body.generation += 1;
    body
}

#[test]
fn engine_reads_machine4_bodies_byte_exact_for_every_shape() {
    let source = skewed(0x4E64_0000_0000_0001, 64 * 1024);
    let n = source.len() as u64;
    for &block_bytes in &[256u32, 1024, 4096] {
        let body = machine4_body(&source, block_bytes);
        let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
        assert!(engine.plan_ref().all_certified());
        // The machine-4 cold path is the block restart (its only bounded structure).
        assert!(
            engine.plan_ref().regions().iter().all(|r| {
                r.path == EnginePath::BlockRestart
                    && matches!(r.structure, StructureRef::BlockIndex(_))
            }),
            "B={block_bytes}: every region must be the machine-4 block path"
        );
        assert_eq!(engine.worst_case_steps(), u64::from(block_bytes) - 1);

        let b = u64::from(block_bytes);
        let requests = [
            (0u64, 1u64),
            (0, 4096),
            (1, 1),
            (4093, 9),
            (b - 3, 9),
            (b, 4096),
            (n / 2, 1234),
            (n - 7, 7),
            (n - 1, 1),
            (0, n),
            (n / 3, n / 3),
            (40_000, 20_000),
        ];
        for (offset, length) in requests {
            if offset + length > n {
                continue;
            }
            let req = ReadRequest { offset, length };
            let want = reader::read(&body, req).unwrap();
            let got = engine.read(&body, req).unwrap();
            assert_eq!(got, want, "m4 B={block_bytes} ({offset},{length})");
            let (got2, steps) = engine.read_counted_steps(&body, req).unwrap();
            assert_eq!(got2, want, "counted m4 B={block_bytes} ({offset},{length})");
            // The request-level bound covers the actual positioning.
            assert!(steps.positioning <= engine.worst_case_steps_for(req));
            assert_eq!(steps.reveals, length);
        }

        // Zero-length at the end, and out-of-bounds mirror `reader::read`.
        assert_eq!(
            engine
                .read(
                    &body,
                    ReadRequest {
                        offset: n,
                        length: 0
                    }
                )
                .unwrap(),
            Vec::<u8>::new()
        );
        assert!(engine
            .read(
                &body,
                ReadRequest {
                    offset: n - 1,
                    length: 2
                }
            )
            .is_err());
    }
}

#[test]
fn planner_includes_and_chooses_the_machine4_block_path() {
    let source = skewed(0x4E64_0000_0000_0002, 256 * 1024);
    let n = source.len() as u64;
    let body = machine4_body(&source, 1024);
    let cfg = CourtConfig {
        reps: 1,
        interleave: false,
        seed: 0x4E64,
    };
    let plan = AccessPlan::plan(&body, &cfg).unwrap();
    assert!(plan.all_certified());
    // The cold tail is served by the machine-4 block restart, not genesis replay.
    assert_eq!(plan.path_for(n - 1), AccessPath::BlockRestart);
    assert!(plan
        .regions()
        .iter()
        .all(|r| matches!(r.path, AccessPath::BlockRestart | AccessPath::CoreReplay)));
    let planner = Planner::new(plan);
    for (offset, length) in [(0u64, 1u64), (0, 4096), (n / 2, 1234), (n - 7, 7), (0, n)] {
        assert_eq!(
            planner.read(&body, ReadRequest { offset, length }).unwrap(),
            reader::read(&body, ReadRequest { offset, length }).unwrap(),
            "planner m4 ({offset},{length})"
        );
    }
}

#[test]
fn engine_prefers_the_machine4_block_path_over_a_coarse_checkpoint() {
    // A coarse machine-4 checkpoint (interval 65536) is a far worse cold restart
    // than a 256-symbol block, so the engine must choose `BlockRestart`.
    let source = skewed(0x4E64_0000_0000_0003, 64 * 1024);
    let n = source.len() as u64;
    let mut body = create_machine(&source, ManifestationProfile::Root, M4).unwrap();
    let cp = ekbloom::CheckpointSet::build_with(machine_by_id(M4).unwrap(), &body.core, n, 65536)
        .unwrap();
    body.checkpoints.push(cp);
    let idx = BlockIndex::build_machine(&body.core, n, 256, M4).unwrap();
    body.blocks.push(idx);
    body.generation += 2;

    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert!(engine.plan_ref().all_certified());
    assert!(engine
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::BlockRestart));
    assert_eq!(engine.worst_case_steps(), 255);
    assert_eq!(
        engine
            .read(
                &body,
                ReadRequest {
                    offset: n / 2,
                    length: 4096
                }
            )
            .unwrap(),
        reader::read(
            &body,
            ReadRequest {
                offset: n / 2,
                length: 4096
            }
        )
        .unwrap()
    );
}

#[test]
fn machine4_prepared_block_context_is_byte_exact_vs_unprepared() {
    let source = skewed(0x4E64_0000_0000_0004, 32 * 1024);
    let n = source.len() as u64;
    for k in [1usize, 2, 4, 8] {
        let core = encode_with_k(&source, k).unwrap();
        for &block in &[256u32, 1024, 4096] {
            let idx = BlockIndex::build_machine(&core, n, block, M4).unwrap();
            let ctx = RansIContext::from_core(&core).unwrap();
            let probes = [
                (0u64, 0u64),
                (0, 1),
                (1, 9),
                (255, 2),
                (u64::from(block), 500),
                (n - 5, 5),
                (0, n),
            ];
            for (offset, length) in probes {
                if offset + length > n {
                    continue;
                }
                let (unprepared, su) = idx.read_range_counted(&core, offset, length).unwrap();
                let (prepared, sp) = idx
                    .read_range_counted_rans_i(&core, &ctx, offset, length)
                    .unwrap();
                assert_eq!(unprepared, prepared, "k={k} B={block} ({offset},{length})");
                assert_eq!(su, sp);
                assert_eq!(
                    prepared,
                    &source[offset as usize..(offset + length) as usize],
                    "k={k} B={block} ({offset},{length}) not source"
                );
            }
            // A prepared read on a non-machine-4 index is rejected.
            let m2 = create_machine(&source, ManifestationProfile::Root, M2).unwrap();
            let idx2 = BlockIndex::build_machine(&m2.core, n, block, M2).unwrap();
            assert!(idx2
                .read_range_counted_rans_i(&m2.core, &ctx, 0, 1)
                .is_err());
        }
    }
}

#[test]
fn machine4_all_lane_counts_decode_exactly() {
    let source = skewed(0x4E64_0000_0000_0005, 8192);
    let n = source.len() as u64;
    // The stored path: every lane count `MAX_LANES` admits.
    for k in [1usize, 2, 4, 8] {
        let core = encode_with_k(&source, k).unwrap();
        assert_eq!(machine_by_id(M4).unwrap().decode(&core, n).unwrap(), source);
        let idx = BlockIndex::build_machine(&core, n, 1024, M4).unwrap();
        assert_eq!(idx.read_range(&core, 0, n).unwrap(), source);
    }
    // The measurement kernel: up to 16 lanes, and agreement with the stored path
    // wherever both apply.
    for k in [2usize, 4, 8, 16] {
        let core = encode_measured(&source, k).unwrap();
        let mut out = vec![0u8; source.len()];
        decode_measured(&core, k, &mut out).unwrap();
        assert_eq!(out, source, "measured k={k}");
        if k <= 8 {
            assert_eq!(
                core,
                encode_with_k(&source, k).unwrap(),
                "k={k} cores differ"
            );
        }
    }
}

#[test]
fn machines_one_two_three_engine_behaviour_unchanged() {
    let source = lcg(0x4E64_0000_0000_0006, 32 * 1024);
    let n = source.len() as u64;
    for &machine in &[M1, M2, M3] {
        let mut body = create_machine(&source, ManifestationProfile::Root, machine).unwrap();
        let idx = BlockIndex::build_machine(&body.core, n, 1024, machine).unwrap();
        body.blocks.push(idx);
        body.generation += 1;
        let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
        assert!(engine.plan_ref().all_certified());
        assert!(engine
            .plan_ref()
            .regions()
            .iter()
            .all(|r| r.path == EnginePath::BlockRestart));
        assert_eq!(engine.worst_case_steps(), 1023);
        for (offset, length) in [(0u64, 1u64), (1, 1), (1021, 9), (1024, 4096), (n - 7, 7)] {
            assert_eq!(
                engine.read(&body, ReadRequest { offset, length }).unwrap(),
                reader::read(&body, ReadRequest { offset, length }).unwrap(),
                "machine {machine} ({offset},{length})"
            );
        }
    }
}
