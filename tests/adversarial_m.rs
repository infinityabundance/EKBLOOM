//! Adversarial/security review of the M1–M5 round.
//!
//! Executable attacks over the newest surface: machine-4 complete successor state
//! (M1), exact physical accounting (M2), committed-vs-active budget (M2),
//! request-level bounded work (M3), the machine-4 engine path + `create_auto` core
//! reuse (M5), and the pinned-hot baseline (M4). Every byte assertion is against
//! the authoritative machine, `reader::read`, or the exact serialized form — never
//! against another manifested structure.

use std::path::{Path, PathBuf};

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::blocks::{BlockIndex, BLOCK_INDEX_PREFIX_LEN};
use ekbloom::bloom::{BloomKind, BloomShell};
use ekbloom::bloom_region::RegionTable;
use ekbloom::checkpoint::{CheckpointSet, CKPT_FIXED_LEN};
use ekbloom::machine::{machine_by_id, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE};
use ekbloom::reader::open_bytes;
use ekbloom::state::Snapshot;
use ekbloom::{
    create_auto, create_auto_within_budget, create_machine, create_machine_within_budget, inspect,
    manifest_hot_regions, pinned_block_count, read, read_region_scoped, select_machine, verify,
    verify_deep, write_body, AccessTrace, CostModel, DirectoryKind, EkbloomBody, EkbloomError,
    Engine, EnginePath, History, ManifestationBudget, ManifestationProfile, PathBloom,
    PinnedHotCache, ReadRequest, RegionReader, SweepConfig, TileSet,
};

const M1: u32 = 1;
const M2: u32 = 2;
const M3: u32 = 3;
const M4: u32 = 4;

/// `BloomShell::to_bytes` fixed header: `kind u32 ++ depth u16 ++ generation u64 ++
/// logical_len u64 ++ entries u64 ++ status u32 ++ core_hash[32] ++ payload_len u64`.
const BLOOM_SHELL_WIRE_HEADER: u64 = 4 + 2 + 8 + 8 + 8 + 4 + 32 + 8;
/// `history::HEADER_LEN`: `generation u64 ++ logical_len u64 ++ core_len u64 ++
/// core_hash[32] ++ count u64`.
const HISTORY_WIRE_HEADER: u64 = 8 + 8 + 8 + 32 + 8;

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

fn small_cfg() -> SweepConfig {
    SweepConfig {
        iterations: 1,
        seed: 7,
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

/// An order-0 skewed 4-symbol source (machines 2/4 compress it: `V < N`).
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

fn tmp(name: &str) -> PathBuf {
    let dir = Path::new("target/adversarial-m").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Serialize + reopen + deep-verify a body (fails if verification is false).
fn reopen(body: &EkbloomBody, name: &str) -> EkbloomBody {
    let path = tmp(name).join("b.ekb");
    write_body(&path, body).unwrap();
    let b = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert!(
        verify_deep(&b).unwrap().ok,
        "{name}: reopened body must verify"
    );
    b
}

/// Read every in-bounds probe through the engine and the authoritative reader;
/// require byte-identity, `reveals == length`, and `positioning <= bound`.
fn assert_probes(body: &EkbloomBody, engine: &Engine, probes: &[(u64, u64)], n: u64, label: &str) {
    for &(off, len) in probes {
        if off.saturating_add(len) > n {
            continue;
        }
        let req = ReadRequest {
            offset: off,
            length: len,
        };
        let want = read(body, req).unwrap();
        let got = engine.read(body, req).unwrap();
        assert_eq!(got, want, "{label}: engine != reader at ({off},{len})");
        let (got2, steps) = engine.read_counted_steps(body, req).unwrap();
        assert_eq!(got2, want, "{label}: counted != reader at ({off},{len})");
        assert_eq!(steps.reveals, len, "{label}: reveal count at ({off},{len})");
        assert!(
            steps.positioning <= engine.worst_case_steps_for(req),
            "{label}: positioning {} > bound {} at ({off},{len})",
            steps.positioning,
            engine.worst_case_steps_for(req)
        );
    }
    // Out-of-bounds and zero-length mirror `reader::read`.
    assert_eq!(
        engine
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
    if n > 0 {
        assert!(engine
            .read(
                body,
                ReadRequest {
                    offset: n - 1,
                    length: 2
                }
            )
            .is_err());
    }
}

const PROBES: &[(u64, u64)] = &[
    (0, 1),
    (0, 4096),
    (1, 1),
    (4093, 9),
    (4096, 1),
    (4095, 2),
    (8191, 3),
    (65536, 1),
    (65535, 65536),
    (0, 0),
];

// ---------------------------------------------------------------------------
// Attack 1 + 5 — machine-4 complete state and first-class engine paths.
// ---------------------------------------------------------------------------

/// Force every accelerated path the engine can choose on a machine-4 body, read
/// **far beyond** the single lookup (so a reconstructed successor is consumed),
/// compare every byte with the authoritative machine, and repeat after a
/// serialize + reopen. Machines 1/2/3 keep their historic record forms.
#[test]
fn machine4_every_accelerated_path_is_byte_exact_beyond_the_lookup() {
    let source = skewed(0x4D31_0000_0000_0001, 64 * 1024);
    let n = source.len() as u64;
    let mut probes: Vec<(u64, u64)> = PROBES.to_vec();
    probes.extend([
        (n / 2, 1234),
        (n - 7, 7),
        (n - 1, 1),
        (0, n),
        (n / 3, n / 3),
        (40_000, 20_000),
    ]);

    // DirectStorage: a certified whole-object path bloom is the only candidate.
    let body = create_machine(&source, ManifestationProfile::Path { depth: 4 }, M4).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert!(engine.plan_ref().all_certified());
    assert!(engine
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::DirectStorage));
    assert_probes(&body, &engine, &probes, n, "m4 DirectStorage");
    let re = reopen(&body, "m4-path");
    let engine = Engine::plan(&re, &generous(), &workload4k()).unwrap();
    assert_probes(&re, &engine, &probes, n, "m4 DirectStorage reopened");

    // HotTile: a fully materialised tile set covers every region.
    let mut body = create_machine(&source, ManifestationProfile::Root, M4).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert!(engine
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::HotTile));
    assert_probes(&body, &engine, &probes, n, "m4 HotTile");

    // PackedRestart: a `K = 64` checkpoint is the only bounded structure.
    let mut body = create_machine(&source, ManifestationProfile::Root, M4).unwrap();
    add_checkpoint(&mut body, 64);
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert!(engine
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::PackedRestart));
    assert_eq!(engine.worst_case_steps(), 63);
    assert_probes(&body, &engine, &probes, n, "m4 PackedRestart");
    let re = reopen(&body, "m4-ckpt");
    let engine = Engine::plan(&re, &generous(), &workload4k()).unwrap();
    assert_probes(&re, &engine, &probes, n, "m4 PackedRestart reopened");

    // BlockRestart: the machine-4 block index is the only bounded structure.
    let mut body = create_machine(&source, ManifestationProfile::Root, M4).unwrap();
    add_blocks(&mut body, 1024);
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert!(engine
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::BlockRestart));
    assert_eq!(engine.worst_case_steps(), 1023);
    assert_probes(&body, &engine, &probes, n, "m4 BlockRestart");
    // The prepared-interleaved-ctx path and the unprepared per-read path must be
    // byte-identical (the engine caches a machine-4 ctx; a plan without one falls
    // back).
    let unprep = Engine::from_plan(engine.plan_ref().clone());
    assert_probes(&body, &unprep, &probes, n, "m4 BlockRestart unprepared");
    let re = reopen(&body, "m4-blocks");
    let engine = Engine::plan(&re, &generous(), &workload4k()).unwrap();
    assert_probes(&re, &engine, &probes, n, "m4 BlockRestart reopened");
}

/// The machine-4 regional (wide) successor is genuinely required: the stored
/// wide snapshot differs from a `(state,cursor)`-only reconstruction, the reader
/// leaps with the stored lanes, and the read is byte-exact beyond the leap and
/// after reopen.
#[test]
fn machine4_region_wide_successor_is_used_not_a_state_cursor_projection() {
    let src = skewed(0x4D31_0000_0000_0002, 8192);
    let mut body = create_machine(&src, ManifestationProfile::Root, M4).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    let report = manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
    assert!(
        report.selected_regions > 0,
        "no region certified: {report:?}"
    );
    assert!(RegionReader::new(&body).has_regions());

    let shell = body
        .blooms
        .iter()
        .find(|s| s.kind == ekbloom::BloomKind::Regional)
        .unwrap();
    let table = RegionTable::from_shell(shell).unwrap();
    assert!(table.wide, "machine-4 table must be the wide form");
    assert_eq!(table.cursor_bytes, 0);

    // The stored wide record carries lanes that a `(state,cursor)` projection drops.
    let depth = u16::from_le_bytes([shell.payload[0], shell.payload[1]]) as usize;
    let off = 46 + 2 + depth; // 46-byte regional prefix ++ key ++ symbols[D]
    let stored = Snapshot::unpack_wide(&shell.payload[off..]).unwrap();
    assert_ne!(
        stored,
        Snapshot::new(stored.state, stored.cursor),
        "the wide record must not be a state/cursor projection"
    );

    for (o, l) in [(0u64, 64u64), (0, 8192), (5, 8000), (8191, 1)] {
        let got = read_region_scoped(&body, o, l).unwrap();
        assert_eq!(
            got,
            &src[o as usize..(o + l) as usize],
            "m4 region ({o},{l})"
        );
        assert_eq!(
            got,
            read(
                &body,
                ReadRequest {
                    offset: o,
                    length: l
                }
            )
            .unwrap()
        );
    }
    let re = reopen(&body, "m4-region");
    for (o, l) in [(0u64, 64u64), (0, 8192)] {
        assert_eq!(
            read_region_scoped(&re, o, l).unwrap(),
            &src[o as usize..(o + l) as usize],
            "reopened m4 region ({o},{l})"
        );
    }
}

/// Machines 1/2/3 keep their historic, compact restart forms under the capability
/// predicate (only machine 4 goes wide).
#[test]
fn machines_one_two_three_restart_forms_are_unchanged() {
    let src = skewed(0x4D31_0000_0000_0003, 4096);
    let n = src.len() as u64;
    for (machine, entry, packed) in [
        (MACHINE_NL2, 2u64, true),
        (MACHINE_RANS0, 12u64, false),
        (MACHINE_STORE, 12u64, false),
        (MACHINE_RANS0I, Snapshot::WIDE_LEN as u64, false),
    ] {
        let core = create_machine(&src, ManifestationProfile::Root, machine)
            .unwrap()
            .core;
        let idx = BlockIndex::build_machine(&core, n, 16, machine).unwrap();
        assert_eq!(idx.entry_bytes(), entry, "machine {machine} entry bytes");
        assert_eq!(idx.packed(), packed, "machine {machine} packed");
    }
}

// ---------------------------------------------------------------------------
// Attack 2 — exact physical accounting across machines 1–4.
// ---------------------------------------------------------------------------

/// Every structure's `physical_bytes()` matches its documented exact relation to
/// `to_bytes().len()`, on all four machines, including the machine-4 wide block
/// index and the wide regional shell. A shared model header is never re-counted.
#[test]
fn accounting_relations_exact_across_machines() {
    let source = skewed(0xAC00_0000_0000_0011, 4096);
    let n = source.len() as u64;
    for machine in [M1, M2, M3, M4] {
        let m = machine_by_id(machine).unwrap();
        let core = create_machine(&source, ManifestationProfile::Root, machine)
            .unwrap()
            .core;

        // BlockIndex: directory + restart records only (no repeated model header).
        let block = BlockIndex::build_machine(&core, n, 16, machine).unwrap();
        assert_eq!(
            block.physical_bytes(),
            block.to_bytes().len() as u64,
            "machine {machine}: BlockIndex physical != serialized"
        );
        assert_eq!(
            block.physical_bytes(),
            BLOCK_INDEX_PREFIX_LEN as u64 + block.block_count() as u64 * block.entry_bytes(),
            "machine {machine}: BlockIndex must not re-count the shared model header"
        );

        // History + CheckpointSet + BloomShell fixed-header relations.
        let history = History::build_with(m, &core, n, 1).unwrap();
        assert_eq!(
            history.to_bytes().len() as u64,
            history.physical_bytes() + HISTORY_WIRE_HEADER
        );
        let cps = CheckpointSet::build_with(m, &core, n, 64).unwrap();
        assert_eq!(
            cps.to_bytes().len() as u64,
            cps.physical_bytes() + CKPT_FIXED_LEN as u64
        );
        let shell = PathBloom::build_with(m, &core, n, 4, 1).unwrap();
        assert_eq!(
            shell.to_bytes().len() as u64,
            shell.physical_bytes() + BLOOM_SHELL_WIRE_HEADER
        );
    }
}

/// A machine-4 body carrying a wide regional shell keeps `inspect`'s component
/// sum exactly equal to `physical_bytes()`, and the wide shell obeys the fixed
/// shell-header relation (it is counted once, no wide-record drift).
#[test]
fn machine4_wide_region_shell_is_counted_once_and_obeys_the_header_relation() {
    let src = skewed(0xAC00_0000_0000_0012, 4096);
    let mut body = create_machine(&src, ManifestationProfile::Root, M4).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();

    let base = create_machine(&src, ManifestationProfile::Root, M4)
        .unwrap()
        .physical_bytes();
    // Every bloom added contributes exactly its payload length.
    let shell: &BloomShell = body
        .blooms
        .iter()
        .find(|s| s.kind == ekbloom::BloomKind::Regional)
        .unwrap();
    assert_eq!(
        shell.to_bytes().len() as u64,
        shell.physical_bytes() + BLOOM_SHELL_WIRE_HEADER
    );
    assert!(body.physical_bytes() >= base + shell.physical_bytes());

    let report = inspect(&body);
    assert_eq!(report.physical_bytes, body.physical_bytes());
    let summed: u64 = report.components.iter().map(|c| c.bytes).sum();
    assert_eq!(summed, body.physical_bytes());
}

// ---------------------------------------------------------------------------
// Attack 3 — committed budget admission (M2).
// ---------------------------------------------------------------------------

/// A body whose *committed* bytes exceed the budget is rejected even when the
/// *active* plan fits, and an over-budget `n == 0` core is rejected too.
#[test]
fn engine_rejects_committed_over_budget_and_zero_region_machine4() {
    let source = lcg(0xB0D6_0000_0000_0001, 64 * 1024);
    let n = source.len() as u64;
    let mut body = create_machine(&source, ManifestationProfile::Root, M4).unwrap();
    // A large committed structure the plan will not select: a tile band over the
    // first 16 KiB only.
    let trace = AccessTrace {
        regions: vec![(0, 16 * 1024)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    add_checkpoint(&mut body, 64);

    let committed = body.physical_bytes();
    let e = Engine::plan_with(&body, &generous(), n, CostModel::measured_default()).unwrap();
    let active = e.plan_ref().retained_bytes();
    assert!(
        active < committed,
        "active {active} !< committed {committed}"
    );
    assert_eq!(e.plan_ref().committed_bytes(), committed);

    let between = active + 1;
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
        "a fitting active plan must not admit an over-committed body"
    );

    // n == 0 over-budget core rejected.
    let mut empty = create_machine(&[], ManifestationProfile::Root, M4).unwrap();
    let cp = CheckpointSet::build_with(machine_by_id(M4).unwrap(), &empty.core, 0, 64).unwrap();
    empty.checkpoints.push(cp);
    empty.generation += 1;
    let committed = empty.physical_bytes();
    let err = Engine::plan_with(
        &empty,
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
}

/// The hard-cap entry points never admit an over-budget machine-4 body, even
/// though the static estimate is the machine-1 view (which under-counts a wide
/// checkpoint) — the post-build `physical_bytes()` re-check closes the gap.
#[test]
fn hard_cap_is_enforced_even_where_the_estimate_undercounts() {
    let src = skewed(0xB0D6_0000_0000_0002, 4096);
    let profile = ManifestationProfile::Sparse { interval: 64 };
    let actual = create_machine_within_budget(&src, profile, M4, &generous())
        .unwrap()
        .physical_bytes();

    // One byte under the real committed footprint: the estimate passes (it
    // under-counts the wide checkpoint) but the actual re-check must reject.
    let err = create_machine_within_budget(
        &src,
        profile,
        M4,
        &ManifestationBudget {
            maximum_physical_bytes: actual - 1,
        },
    );
    assert!(
        matches!(err, Err(EkbloomError::AllocationTooLarge { requested, limit })
            if requested == actual && limit == actual - 1),
        "the post-build check must reject an over-budget wide body"
    );

    // Exactly the real footprint is admitted.
    let ok = create_machine_within_budget(
        &src,
        profile,
        M4,
        &ManifestationBudget {
            maximum_physical_bytes: actual,
        },
    )
    .expect("a body at its own committed footprint fits");
    assert_eq!(ok.physical_bytes(), actual);
}

// ---------------------------------------------------------------------------
// Attack 4 — request-level bounded work.
// ---------------------------------------------------------------------------

/// A machine-4 body whose plan alternates HotTile (even regions) and
/// PackedRestart (odd regions), read **whole-object**: many cold→hot transitions,
/// each paying a bounded restart. The actual positioning must never exceed the
/// request-level bound, and every byte must match the authoritative reader.
#[test]
fn request_level_bound_covers_many_segments_on_machine4() {
    const N: usize = 64 * 1024;
    const REGION: u64 = 4096;
    let source = skewed(0x1CEB_0000_0000_0001, N);
    let n = source.len() as u64;
    let mut body = create_machine(&source, ManifestationProfile::Root, M4).unwrap();

    // Materialise every even 4 KiB tile: regions 0,2,4,... become HotTile; the
    // odd regions fall to the covering checkpoint.
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    let mut r = 0u64;
    while r < n {
        regions.push((r, REGION.min(n - r)));
        counts.push(1);
        r += 2 * REGION;
    }
    TileSet::manifest_hot_tiles(
        &mut body,
        &AccessTrace { regions, counts },
        12,
        DirectoryKind::Flat,
        &generous(),
    )
    .unwrap();
    add_checkpoint(&mut body, 64);

    let engine =
        Engine::plan_with(&body, &generous(), REGION, CostModel::measured_default()).unwrap();
    assert!(engine.plan_ref().all_certified());
    let dist = engine.plan_ref().distribution();
    assert!(
        dist.iter().any(|(p, _)| *p == EnginePath::HotTile)
            && dist.iter().any(|(p, _)| *p == EnginePath::PackedRestart),
        "the plan must mix hot and cold paths: {dist:?}"
    );

    // Whole-object, plus a start deep in a cold segment.
    for req in [
        ReadRequest {
            offset: 0,
            length: n,
        },
        ReadRequest {
            offset: 1,
            length: n - 1,
        },
        ReadRequest {
            offset: REGION + 5,
            length: n - REGION - 5,
        },
    ] {
        let claimed = engine.worst_case_steps_for(req);
        assert!(claimed < req.length, "claimed {claimed} is not O(segments)");
        let (got, steps) = engine.read_counted_steps(&body, req).unwrap();
        assert_eq!(
            got,
            read(&body, req).unwrap(),
            "m4 multi-segment read ({},{}) not byte-exact",
            req.offset,
            req.length
        );
        assert_eq!(steps.reveals, req.length);
        assert!(
            steps.positioning <= claimed,
            "positioning {} > claimed {} for ({},{})",
            steps.positioning,
            claimed,
            req.offset,
            req.length
        );
    }
}

// ---------------------------------------------------------------------------
// Attack 6 — create_auto core reuse (M5).
// ---------------------------------------------------------------------------

/// `create_auto` (and its budget variant) must be byte-identical to the explicit
/// select-then-create path across machines and profiles; the reused core must be
/// the body's own core (no stale/dangling core).
#[test]
fn create_auto_reuse_is_byte_identical_and_core_bound() {
    let sources = [
        lcg(0x0A00_0000_0000_0001, 4096),    // incompressible -> machine 3
        skewed(0x0A00_0000_0000_0002, 4096), // compressible -> machine 2
        Vec::new(),
        vec![7u8; 1],
    ];
    let profiles = [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 4096,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ];
    for source in &sources {
        let machine = select_machine(source).unwrap();
        for &profile in &profiles {
            let reused = create_auto(source, profile).unwrap();
            let oracle = create_machine(source, profile, machine).unwrap();
            assert_eq!(reused, oracle, "create_auto != explicit for {profile:?}");

            // The body's core is exactly the encode of the source (no dangling core).
            let expected_core = create_machine(source, ManifestationProfile::Root, machine)
                .unwrap()
                .core;
            assert_eq!(reused.core, expected_core, "stale core for {profile:?}");
            assert_eq!(
                reused.core_hash,
                *blake3::hash(&reused.core).as_bytes(),
                "core_hash not bound to the reused core"
            );

            // Budget variant: use a budget that admits both the static estimate
            // and the real footprint (the estimate is a conservative upper bound,
            // so it can exceed the true committed bytes for some profiles).
            let committed = reused.physical_bytes();
            let estimate = ekbloom::estimate_physical_bytes_for_core(
                reused.core.len() as u64,
                source.len() as u64,
                profile,
            );
            let budget = ManifestationBudget {
                maximum_physical_bytes: committed.max(estimate),
            };
            let reused_b = create_auto_within_budget(source, profile, &budget).unwrap();
            let oracle_b = create_machine_within_budget(source, profile, machine, &budget).unwrap();
            assert_eq!(reused_b, oracle_b, "budgeted create_auto != explicit");
            assert!(reused_b.physical_bytes() <= budget.maximum_physical_bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 7 — pinned-hot baseline (M4).
// ---------------------------------------------------------------------------

/// The pinned footprint counts the **actual** pinned decoded bytes, the pinned set
/// is permanent and bounded, and cold lookups never displace it.
#[test]
fn pinned_hot_footprint_is_exact_and_non_evicting() {
    const BLOCK: usize = 4096;
    let src = lcg(0xB1B1_0000_0000_0001, 32 * 1024);
    let blocks: Vec<Vec<u8>> = src.chunks(BLOCK).map(|c| c.to_vec()).collect();
    let hot = 12 * 1024;
    let count = pinned_block_count(hot, BLOCK as u64, blocks.len() as u64);
    assert_eq!(count, 3);

    let mut cache = PinnedHotCache::new(BLOCK as u64, count as usize);
    let mut pinned = 0u64;
    for (b, block) in blocks.iter().enumerate().take(count as usize) {
        assert!(cache.pin(b as u64, block.clone()));
        pinned += block.len() as u64;
    }
    assert_eq!(
        cache.pinned_bytes(),
        pinned,
        "footprint must count real bytes"
    );
    assert_eq!(cache.pinned_blocks(), count as usize);

    // Capacity is full: no further pin, and cold lookups cannot evict.
    assert!(!cache.pin(count as u64, vec![0u8; BLOCK]));
    for b in (count as usize)..blocks.len() {
        assert!(cache.get(b as u64).is_none());
        cache.note_miss();
    }
    assert_eq!(cache.pinned_blocks(), count as usize);
    assert_eq!(
        cache.pinned_bytes(),
        pinned,
        "cold reads must not evict pins"
    );

    // `pinned_block_count` is bounded by `total_blocks` and defined at the edges.
    assert_eq!(pinned_block_count(hot, BLOCK as u64, 2), 2);
    assert_eq!(pinned_block_count(hot, 0, 10), 0);
    assert_eq!(pinned_block_count(hot, BLOCK as u64, 0), 0);
}

// ---------------------------------------------------------------------------
// Attack 1 (verify half) — a forged machine-4 wide successor must be rejected.
// ---------------------------------------------------------------------------

fn assert_regional_check_fails(body: &EkbloomBody, idx: usize) {
    let rep = verify(body).unwrap();
    assert!(!rep.ok, "forged body must not verify ok");
    let name = format!("bloom[{idx}]:regional");
    let c = rep
        .checks
        .iter()
        .find(|c| c.name == name)
        .expect("the regional check must be present");
    assert!(
        !c.passed,
        "a forged wide successor was not caught: {}",
        c.detail
    );
}

/// `verify` re-derives the complete machine-4 regional successor from the
/// authoritative pass, so a *self-consistent but forged* wide record (its Phi_D
/// projection still matching) is rejected — the property the region reader
/// depends on to resume with the stored lanes.
#[test]
fn forged_machine4_wide_successor_is_rejected_by_verify() {
    let src = skewed(0xF0F0_0000_0000_0001, 4096);
    let mut body = create_machine(&src, ManifestationProfile::Root, M4).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
    assert!(verify(&body).unwrap().ok, "the honest body must verify");

    let idx = body
        .blooms
        .iter()
        .position(|s| s.kind == BloomKind::Regional)
        .expect("a regional shell");
    let depth =
        u16::from_le_bytes([body.blooms[idx].payload[0], body.blooms[idx].payload[1]]) as usize;
    // pack_wide begins after the 46-byte prefix, the u16 key and the D symbols.
    let rec = 46 + 2 + depth;

    // (a) Tamper the stored successor cursor (bytes 4..12 of pack_wide).
    let mut cursor_forge = body.clone();
    cursor_forge.blooms[idx].payload[rec + 4] ^= 0xFF;
    assert_regional_check_fails(&cursor_forge, idx);

    // (b) Tamper a stored lane state (bytes 13.. of pack_wide).
    let mut lane_forge = body.clone();
    lane_forge.blooms[idx].payload[rec + 13] ^= 0xFF;
    assert_regional_check_fails(&lane_forge, idx);

    // (c) Tamper the active-lane index (byte 12 of pack_wide).
    let mut active_forge = body.clone();
    active_forge.blooms[idx].payload[rec + 12] ^= 0x01;
    assert_regional_check_fails(&active_forge, idx);

    // The honest reader is still byte-exact after the forgeries above (untouched).
    assert_eq!(read_region_scoped(&body, 0, 64).unwrap(), &src[..64]);
}

// ---------------------------------------------------------------------------
// Attack 1 — cross-machine regional regression (M1 must not perturb 1/2/3).
// ---------------------------------------------------------------------------

/// The region reader leaps with the machine-appropriate successor form — base
/// (machine 1), cursor (machines 2 and 3), wide (machine 4) — and is byte-exact
/// across and beyond the region for every machine.
#[test]
fn region_reader_is_byte_exact_for_all_four_machines() {
    let src = skewed(0x4E61_0000_0000_0099, 4096);
    for machine in [M1, M2, M3, M4] {
        let mut body = create_machine(&src, ManifestationProfile::Root, machine).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, 4)],
            counts: vec![1000],
        };
        let report = manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
        assert!(report.selected_regions > 0, "machine {machine}: no region");
        assert!(RegionReader::new(&body).has_regions(), "machine {machine}");

        let shell = body
            .blooms
            .iter()
            .find(|s| s.kind == BloomKind::Regional)
            .unwrap();
        let table = RegionTable::from_shell(shell).unwrap();
        match machine {
            M1 => {
                assert!(
                    !table.wide && table.cursor_bytes == 0,
                    "machine 1 base form"
                );
            }
            M4 => {
                assert!(table.wide && table.cursor_bytes == 0, "machine 4 wide form");
            }
            _ => {
                assert!(
                    !table.wide && table.cursor_bytes == 8,
                    "machine {machine} cursor form"
                );
            }
        }

        for (o, l) in [
            (0u64, 1u64),
            (0, 4),
            (0, 64),
            (1, 10),
            (3, 5),
            (4, 60),
            (100, 3900),
            (4095, 1),
            (0, 4096),
        ] {
            let got = read_region_scoped(&body, o, l).unwrap();
            assert_eq!(
                got,
                &src[o as usize..(o + l) as usize],
                "m{machine} ({o},{l})"
            );
            assert_eq!(
                got,
                read(
                    &body,
                    ReadRequest {
                        offset: o,
                        length: l
                    }
                )
                .unwrap()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 1/5 — every manifest profile verifies and round-trips on machine 4.
// ---------------------------------------------------------------------------

/// A body produced by **any** manifest profile must pass `verify` (the manifested
/// structures are machine-general), survive serialize + reopen, and read the
/// whole object byte-exactly. A profile whose structure is not machine-4-correct
/// would fail here.
#[test]
fn machine4_every_manifest_profile_verifies_and_roundtrips() {
    let src = skewed(0x4D31_0000_0000_0004, 4096);
    let n = src.len() as u64;
    let profiles = [
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Dense { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 4096,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ];
    for profile in profiles {
        let body = create_machine(&src, profile, M4).unwrap();
        let rep = verify(&body).unwrap();
        assert!(rep.ok, "M4 {profile:?} must verify: {:?}", rep.checks);
        let name = format!("m4-verify-{}-{}", profile.label(), n);
        let re = reopen(&body, &name);
        assert!(verify_deep(&re).unwrap().ok, "M4 {profile:?} reopened");
        assert_eq!(
            read(
                &re,
                ReadRequest {
                    offset: 0,
                    length: n
                }
            )
            .unwrap(),
            src,
            "M4 {profile:?} reopened whole-object read"
        );
    }
}
