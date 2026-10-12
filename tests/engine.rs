//! Unified bounded-access engine contract tests.
//!
//! Pins: [`Engine::read`] is byte-for-byte identical to [`ekbloom::read`] on both
//! machines across many requests (including region-boundary crossings and a
//! whole-object read); only certified bounded paths are ever selected (a body
//! with no bounded structure is rejected rather than replayed from `Omega_0`);
//! the budget is enforced transactionally; [`Engine::worst_case_steps`] is a
//! genuine upper bound for adversarial offsets; the branch-lean hot tile surface
//! is the decoded output; and the competitor codecs (block-indexed LZ4, chunked
//! Zstandard) return byte-exact data.

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::{
    create, create_machine, machine_by_id, manifest, read, AccessTrace, BlockIndex, CheckpointSet,
    CostModel, DirectoryKind, EkbloomBody, EkbloomError, Engine, EnginePath, ManifestationBudget,
    ManifestationProfile, ReadRequest, StructureRef, TileSet,
};

const M1: u32 = 1;
const M2: u32 = 2;
const M3: u32 = 3;

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

/// A skewed 4-symbol corpus (order-0 compressible, so machine 2 attains `V < N`).
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

/// Materialise **every** tile of the object (`hot_surface` becomes available).
fn add_full_tiles(body: &mut EkbloomBody, shift: u8, budget: &ManifestationBudget) {
    let n = body.logical_len;
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(body, &trace, shift, DirectoryKind::Flat, budget).unwrap();
}

fn add_checkpoint(body: &mut EkbloomBody, interval: u32) {
    let machine = machine_by_id(body.machine_id).unwrap();
    let cp = CheckpointSet::build_with(machine, &body.core, body.logical_len, interval).unwrap();
    body.checkpoints.push(cp);
    body.generation += 1;
}

fn add_blocks(body: &mut EkbloomBody, block_bytes: u32) {
    let block =
        BlockIndex::build_machine(&body.core, body.logical_len, block_bytes, body.machine_id)
            .unwrap();
    body.blocks.push(block);
    body.generation += 1;
}

/// A "rich" body: full tiles, a `K = 64` checkpoint set and a `B = 1024` block
/// index (plus a machine-1 history).
fn rich_body(machine: u32) -> EkbloomBody {
    let source = skewed(0xE0E0_0000_0000_0001, 64 * 1024);
    let mut body = create_machine(&source, ManifestationProfile::Root, machine).unwrap();
    add_full_tiles(&mut body, 12, &generous());
    add_checkpoint(&mut body, 64);
    add_blocks(&mut body, 1024);
    if machine == M1 {
        manifest(&mut body, ManifestationProfile::Full).unwrap();
    }
    body
}

#[test]
fn engine_read_matches_reader_on_both_machines() {
    for machine in [M1, M2] {
        let body = rich_body(machine);
        let n = body.logical_len;
        let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
        let requests = [
            (0u64, 1u64),
            (0, 4096),
            (1, 1),
            (4093, 9),
            (4096 - 3, 9),
            (4096, 4096),
            (n / 2, 1234),
            (n - 7, 7),
            (n - 1, 1),
            (0, n),
            (n / 3, n / 3),
            (40000, 20000),
        ];
        for (offset, length) in requests {
            let want = read(&body, ReadRequest { offset, length }).unwrap();
            let got = engine.read(&body, ReadRequest { offset, length }).unwrap();
            assert_eq!(got, want, "machine {machine} at ({offset},{length})");
            let (got2, _) = engine
                .read_counted(&body, ReadRequest { offset, length })
                .unwrap();
            assert_eq!(
                got2, want,
                "counted machine {machine} at ({offset},{length})"
            );
        }
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
                    offset: n,
                    length: 1
                }
            )
            .is_err());
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
fn engine_direct_storage_matches_reader() {
    let source = lcg(0xD1D1_0000_0000_0001, 32 * 1024);
    let body = create(&source, ManifestationProfile::Path { depth: 4 }).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(engine.plan_ref().path_for(0), EnginePath::DirectStorage);
    assert!(engine.plan_ref().all_certified());
    let n = body.logical_len;
    for (offset, length) in [(0u64, 1u64), (n / 2, 4096), (n - 5, 5), (0, n)] {
        assert_eq!(
            engine.read(&body, ReadRequest { offset, length }).unwrap(),
            read(&body, ReadRequest { offset, length }).unwrap()
        );
    }
}

#[test]
fn byte_at_and_read_into_are_byte_exact_on_every_machine() {
    // A fully-resident body (hot tile surface) exercises the allocation-free hot
    // path; a path-profile body exercises the direct-storage hot path.
    for machine in [M1, M2, M3] {
        let body = rich_body(machine);
        let n = body.logical_len;
        let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();

        let requests = [
            (0u64, 1u64),
            (0, 4096),
            (1, 1),
            (4093, 9),
            (4096 - 3, 9),
            (4096, 4096),
            (n / 2, 1234),
            (n - 7, 7),
            (n - 1, 1),
            (0, n),
            (n / 3, n / 3),
            (40000, 20000),
        ];
        for (offset, length) in requests {
            let want = read(&body, ReadRequest { offset, length }).unwrap();
            let mut buf = vec![0u8; length as usize];
            engine
                .read_into(&body, ReadRequest { offset, length }, &mut buf)
                .unwrap();
            assert_eq!(
                buf, want,
                "read_into machine {machine} at ({offset},{length})"
            );
            if length == 1 {
                assert_eq!(
                    engine.byte_at(&body, offset).unwrap(),
                    want[0],
                    "byte_at machine {machine} at {offset}"
                );
            }
        }

        // Zero-length and boundary errors mirror reader::read.
        let mut empty: [u8; 0] = [];
        engine
            .read_into(
                &body,
                ReadRequest {
                    offset: n,
                    length: 0,
                },
                &mut empty,
            )
            .unwrap();
        assert!(engine.byte_at(&body, n).is_err());
        assert!(engine
            .read_into(
                &body,
                ReadRequest {
                    offset: n - 1,
                    length: 2
                },
                &mut [0u8; 2]
            )
            .is_err());
        // A destination of the wrong length is rejected for a non-zero request.
        assert!(engine
            .read_into(
                &body,
                ReadRequest {
                    offset: 0,
                    length: 4
                },
                &mut [0u8; 3]
            )
            .is_err());
    }
}

#[test]
fn byte_at_and_read_into_are_byte_exact_on_direct_storage() {
    let source = lcg(0xD1D2_0000_0000_0001, 32 * 1024);
    let body = create(&source, ManifestationProfile::Path { depth: 4 }).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(engine.plan_ref().path_for(0), EnginePath::DirectStorage);
    let n = body.logical_len;
    for (offset, length) in [(0u64, 1u64), (1, 1), (n / 2, 4096), (n - 5, 5), (0, n)] {
        let want = read(&body, ReadRequest { offset, length }).unwrap();
        let mut buf = vec![0u8; length as usize];
        engine
            .read_into(&body, ReadRequest { offset, length }, &mut buf)
            .unwrap();
        assert_eq!(buf, want);
        if length == 1 {
            assert_eq!(engine.byte_at(&body, offset).unwrap(), want[0]);
        }
    }
}

#[test]
fn engine_rejects_bodies_without_a_bounded_path() {
    let source = lcg(0x0BAD_0000_0000_0001, 16 * 1024);
    for machine in [M1, M2] {
        let body = create_machine(&source, ManifestationProfile::Root, machine).unwrap();
        let result = Engine::plan(&body, &generous(), &workload4k());
        assert!(
            result.is_err(),
            "a root body has no certified bounded path and must be rejected (machine {machine})"
        );
    }
}

#[test]
fn engine_selects_the_cheapest_certified_bounded_path() {
    let source = lcg(0x5E1E_0000_0000_0001, 64 * 1024);

    let mut tiles = create(&source, ManifestationProfile::Root).unwrap();
    add_full_tiles(&mut tiles, 12, &generous());
    let e = Engine::plan(&tiles, &generous(), &workload4k()).unwrap();
    assert!(e.plan_ref().all_certified());
    assert_eq!(e.worst_case_steps(), 0);
    assert!(e
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::HotTile && matches!(r.structure, StructureRef::TileSet(_))));

    let mut cp = create(&source, ManifestationProfile::Root).unwrap();
    add_checkpoint(&mut cp, 64);
    let e = Engine::plan(&cp, &generous(), &workload4k()).unwrap();
    assert_eq!(e.worst_case_steps(), 63);
    assert!(e
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::PackedRestart));

    let mut blk = create(&source, ManifestationProfile::Root).unwrap();
    add_blocks(&mut blk, 1024);
    let e = Engine::plan(&blk, &generous(), &workload4k()).unwrap();
    assert_eq!(e.worst_case_steps(), 1023);
    assert!(e
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::BlockRestart));

    let hist = create(&source, ManifestationProfile::Full).unwrap();
    let e = Engine::plan(&hist, &generous(), &workload4k()).unwrap();
    assert_eq!(e.worst_case_steps(), 0);
    assert!(e
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::AffineSummary));
}

#[test]
fn engine_budget_is_enforced_transactionally() {
    let source = lcg(0xB0D6_0000_0000_0001, 64 * 1024);
    let mut body = create(&source, ManifestationProfile::Root).unwrap();
    add_full_tiles(&mut body, 12, &generous());
    add_checkpoint(&mut body, 64);
    add_blocks(&mut body, 1024);
    let core = body.core.len() as u64;
    let committed = body.physical_bytes();
    assert!(committed > core, "the body carries committed structures");

    // Generous: the cheapest bounded path everywhere, on a committed body that fits.
    let e = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(e.worst_case_steps(), 0, "tiles win under a generous budget");
    assert_eq!(e.plan_ref().path_for(0), EnginePath::HotTile);
    assert_eq!(
        e.plan_ref().committed_bytes(),
        committed,
        "the committed footprint is the authoritative budget quantity"
    );

    // A core-only budget is below the *committed* footprint (which still retains the
    // unselected-for-some-regions structures) → transactional error.
    let err = Engine::plan(
        &body,
        &ManifestationBudget {
            maximum_physical_bytes: core,
        },
        &workload4k(),
    );
    assert!(
        matches!(err, Err(EkbloomError::AllocationTooLarge { requested, limit })
            if requested == committed && limit == core),
        "committed > budget must be rejected"
    );

    // Exactly the committed footprint is admitted; the active plan is smaller.
    let exact = ManifestationBudget {
        maximum_physical_bytes: committed,
    };
    let e = Engine::plan(&body, &exact, &workload4k()).unwrap();
    assert!(e.plan_ref().committed_bytes() <= exact.maximum_physical_bytes);
    assert!(e.plan_ref().retained_bytes() <= e.plan_ref().committed_bytes());
}

#[test]
fn engine_admits_the_committed_body_not_the_active_plan() {
    // A large committed structure the plan does **not** select: tiles materialised
    // for the first 16 KiB only, plus a small checkpoint covering the whole object.
    let source = lcg(0xC0FF_0000_0000_0001, 64 * 1024);
    let mut body = create(&source, ManifestationProfile::Root).unwrap();
    let n = body.logical_len;
    let trace = AccessTrace {
        regions: vec![(0, 16 * 1024)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    add_checkpoint(&mut body, 64);

    let committed = body.physical_bytes();

    // One region spanning the whole object, so the partial tile set covers nothing
    // and the covering checkpoint is selected instead.
    let e = Engine::plan_with(&body, &generous(), n, CostModel::measured_default()).unwrap();
    assert!(e
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::PackedRestart));
    let active = e.plan_ref().retained_bytes();
    assert!(
        active < committed,
        "active {active} should be below committed {committed}"
    );
    assert_eq!(e.plan_ref().committed_bytes(), committed);

    // A budget above the *active* plan but below the *committed* body is rejected.
    let between = active + 1;
    assert!(between < committed);
    let err = Engine::plan_with(
        &body,
        &ManifestationBudget {
            maximum_physical_bytes: between,
        },
        n,
        CostModel::measured_default(),
    );
    assert!(
        matches!(err, Err(EkbloomError::AllocationTooLarge { requested, limit })
            if requested == committed && limit == between),
        "a small active plan must not admit an over-committed body"
    );
}

#[test]
fn engine_rejects_an_over_budget_zero_region_body() {
    // `n == 0`: no regions, but the committed core plus a manifested zero-length
    // checkpoint still counts against the budget and must be checked.
    let mut body = create(&[], ManifestationProfile::Root).unwrap();
    assert_eq!(body.logical_len, 0);
    let cp = CheckpointSet::build(&body.core, 0, 64).unwrap();
    body.checkpoints.push(cp);
    body.generation += 1;
    let committed = body.physical_bytes();
    assert!(committed > 0);

    // A budget one byte below the committed footprint is rejected.
    let err = Engine::plan_with(
        &body,
        &ManifestationBudget {
            maximum_physical_bytes: committed - 1,
        },
        4096,
        CostModel::measured_default(),
    );
    assert!(
        matches!(err, Err(EkbloomError::AllocationTooLarge { requested, limit })
            if requested == committed && limit == committed - 1),
        "an over-budget zero-region core must be rejected"
    );

    // At exactly the committed footprint the empty plan is admitted.
    let e = Engine::plan_with(
        &body,
        &ManifestationBudget {
            maximum_physical_bytes: committed,
        },
        4096,
        CostModel::measured_default(),
    )
    .unwrap();
    assert_eq!(e.plan_ref().region_count(), 0);
    assert_eq!(e.plan_ref().committed_bytes(), committed);
}

#[test]
fn worst_case_steps_holds_for_adversarial_offsets() {
    let source = lcg(0xA0DE_0000_0000_0001, 64 * 1024);
    let n = source.len() as u64;

    let mut cp = create(&source, ManifestationProfile::Root).unwrap();
    add_checkpoint(&mut cp, 64);
    let e = Engine::plan(&cp, &generous(), &workload4k()).unwrap();
    assert_eq!(e.worst_case_steps(), 63);
    for base in (0..n).step_by(4093) {
        for delta in [0u64, 1, 62, 63] {
            let off = (base + delta).min(n - 1);
            let (_, steps) = e
                .read_counted(
                    &cp,
                    ReadRequest {
                        offset: off,
                        length: 1,
                    },
                )
                .unwrap();
            assert!(
                steps - 1 <= e.worst_case_steps(),
                "checkpoint off {off}: {} > {}",
                steps - 1,
                e.worst_case_steps()
            );
            assert_eq!(steps - 1, off % 64);
        }
    }
    let (_, s) = e
        .read_counted(
            &cp,
            ReadRequest {
                offset: 63,
                length: 1,
            },
        )
        .unwrap();
    assert_eq!(s - 1, 63, "the worst offset pays the full K-1 skip");

    let mut blk = create(&source, ManifestationProfile::Root).unwrap();
    add_blocks(&mut blk, 1024);
    let eb = Engine::plan(&blk, &generous(), &workload4k()).unwrap();
    assert_eq!(eb.worst_case_steps(), 1023);
    for off in [1023u64, 2047, n - 1] {
        let (_, steps) = eb
            .read_counted(
                &blk,
                ReadRequest {
                    offset: off,
                    length: 1,
                },
            )
            .unwrap();
        assert_eq!(steps - 1, off % 1024);
        assert!(steps - 1 <= eb.worst_case_steps());
    }
}

#[test]
fn hot_surface_is_the_decoded_output_and_read_hot_is_exact() {
    let source = skewed(0x0570_7000_0000_0001, 16 * 1024);
    let mut body = create(&source, ManifestationProfile::Root).unwrap();
    add_full_tiles(&mut body, 12, &generous());
    let ts = body.tiles.last().unwrap();
    let surf = ts
        .hot_surface()
        .expect("a fully-materialised set exposes a hot surface");
    assert_eq!(surf, &source[..], "the hot surface is the decoded output");
    for (offset, length) in [
        (0u64, 1u64),
        (4095, 2),
        (4096, 4096),
        (source.len() as u64 - 7, 7),
    ] {
        assert_eq!(
            ts.read_hot(offset, length).unwrap(),
            &source[offset as usize..(offset + length) as usize]
        );
        assert_eq!(
            ts.read(&body.core, offset, length).unwrap(),
            &source[offset as usize..(offset + length) as usize]
        );
    }
}

#[test]
fn engine_plan_is_deterministic() {
    let body = rich_body(M1);
    let a = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    let b = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(a.plan_ref(), b.plan_ref());
    assert_eq!(a.plan_ref().distribution(), b.plan_ref().distribution());
}

/// Block-indexed LZ4: a fixed 4 KiB block grid with per-block LZ4 blocks and an
/// implicit block index (block ordinal).
fn lz4_read(
    source: &[u8],
    blocks: &[Vec<u8>],
    block_size: usize,
    offset: usize,
    len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let end = offset + len;
    let mut pos = offset;
    while pos < end {
        let b = pos / block_size;
        let start = b * block_size;
        let seg_end = end.min(start + block_size);
        let full = (source.len() - start).min(block_size);
        let decoded = lz4_flex::decompress(&blocks[b], full).unwrap();
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// Chunked/seekable Zstandard: one independent frame per chunk, with an implicit
/// chunk index (chunk ordinal).
fn zstd_read(frames: &[Vec<u8>], chunk_size: usize, offset: usize, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let end = offset + len;
    let mut pos = offset;
    while pos < end {
        let c = pos / chunk_size;
        let start = c * chunk_size;
        let seg_end = end.min(start + chunk_size);
        let decoded = zstd::stream::decode_all(&frames[c][..]).unwrap();
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

#[test]
fn competitor_codecs_return_byte_exact_data() {
    let source = skewed(0xC0FF_0000_0000_0001, 128 * 1024);
    let n = source.len();

    let lz4_block = 4096usize;
    let lz4_blocks: Vec<Vec<u8>> = source.chunks(lz4_block).map(lz4_flex::compress).collect();
    let zstd_chunk = 64 * 1024usize;
    let zstd_frames: Vec<Vec<u8>> = source
        .chunks(zstd_chunk)
        .map(|c| zstd::stream::encode_all(c, 0).unwrap())
        .collect();

    let cases = [
        (0usize, 1usize),
        (0, 4096),
        (4095, 10),
        (100, 5000),
        (n / 2, 4096),
        (n - 7, 7),
    ];
    for (offset, len) in cases {
        assert_eq!(
            lz4_read(&source, &lz4_blocks, lz4_block, offset, len),
            &source[offset..offset + len],
            "lz4 at ({offset},{len})"
        );
        assert_eq!(
            zstd_read(&zstd_frames, zstd_chunk, offset, len),
            &source[offset..offset + len],
            "zstd at ({offset},{len})"
        );
    }
}
