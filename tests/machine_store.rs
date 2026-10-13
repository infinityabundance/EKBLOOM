//! Machine 3 (`OrANS-Store`, the identity/verbatim codec) and the `auto`
//! representation policy.
//!
//! These prove that (a) machine 3 roundtrips exactly for every source shape with
//! `V = N` and no expansion, (b) it drives every manifested structure (state log,
//! history, checkpoints, path blooms, blocks, tiles, the bounded engine) through
//! the machine-general code paths, and (c) the `auto` policy picks machine 3 for
//! incompressible data (base footprint `<= N`) and machine 2 for compressible data
//! (`V < N`).

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::bloom_path::PathBloom;
use ekbloom::checkpoint::CheckpointSet;
use ekbloom::history::History;
use ekbloom::machine::{machine_by_id, state_log_with, OrMachine, MACHINE_RANS0, MACHINE_STORE};
use ekbloom::machine_store::STORE;
use ekbloom::manifest::{create_auto, create_machine};
use ekbloom::reader::read;
use ekbloom::{
    verify, verify_deep, AccessTrace, BlockIndex, DirectoryKind, Engine, EnginePath,
    ManifestationBudget, ManifestationProfile, ReadRequest, TileSet,
};
use std::path::PathBuf;

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

/// English-like low-entropy text (order-0 compressible).
fn skewed(len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b" etaoinshrdlu,.ET\"'\n";
    const WEIGHTS: &[u32] = &[40, 25, 18, 15, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 3, 2, 2];
    let total: u32 = WEIGHTS.iter().sum();
    let mut s: u64 = 0x5EED_1234_ABCD_0001;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut pick = ((s >> 33) as u32) % total;
        for (i, &w) in WEIGHTS.iter().enumerate() {
            if pick < w {
                out.push(ALPHABET[i % ALPHABET.len()]);
                break;
            }
            pick -= w;
        }
    }
    out
}

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

#[test]
fn identity_roundtrip_for_every_shape() {
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x5A],
        (0u8..=255).collect(),
        vec![7u8; 300],
        (0..300u32)
            .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
            .collect(),
        skewed(1024),
        lcg(0xDEAD_BEEF_1234_5678, 1024),
    ];
    for src in cases {
        let core = STORE.encode(&src).unwrap();
        assert_eq!(core, src, "machine 3 must store verbatim");
        assert_eq!(core.len(), src.len(), "V = N exactly");
        let n = src.len() as u64;
        STORE.check_core(&core, n).unwrap();
        assert_eq!(STORE.decode(&core, n).unwrap(), src);
        assert_eq!(
            state_log_with(&STORE, &core, n).unwrap().len() as u64,
            n + 1
        );
    }
}

#[test]
fn machine_three_drives_every_structure() {
    let src = skewed(300);
    let core = STORE.encode(&src).unwrap();
    let n = src.len() as u64;
    let machine = machine_by_id(MACHINE_STORE).unwrap();
    let decoded = machine.decode(&core, n).unwrap();
    assert_eq!(decoded, src);

    // History.
    let history = History::build_with(machine, &core, n, 3).unwrap();
    history.verify_against_with(machine, &core).unwrap();

    // Checkpoints (packed and full wire forms both parse and restore).
    for &k in &[1u32, 4, 16, 64] {
        let cps = CheckpointSet::build_with(machine, &core, n, k).unwrap();
        let bytes = cps.to_bytes();
        let parsed = CheckpointSet::from_bytes_with(machine, &bytes).unwrap();
        assert_eq!(parsed, cps);
        for ordinal in 0..cps.count {
            let target = ordinal * u64::from(k);
            let omega = cps.restore_with(machine, &core, target).unwrap();
            assert_eq!(omega.cursor, target, "cursor equals position");
        }
    }

    // Path bloom: hot symbols reproduce the machine.
    for &d in &[1u16, 2, 4] {
        let shell = PathBloom::build_with(machine, &core, n, d, 5).unwrap();
        let pb = PathBloom::from_shell(&shell).unwrap();
        for pos in 0..n {
            assert_eq!(pb.symbol_at(pos).unwrap(), decoded[pos as usize]);
        }
    }

    // Block index (machine-general, full 12-byte restarts for machine 3).
    let blocks = BlockIndex::build_machine(&core, n, 64, MACHINE_STORE).unwrap();
    assert!(blocks.verify_against(&core).unwrap());
    for (off, len) in [(0u64, 1u64), (63, 3), (100, 200), (n - 5, 5)] {
        let got = blocks.read_range(&core, off, len).unwrap();
        assert_eq!(got, &src[off as usize..(off + len) as usize]);
    }
}

#[test]
fn machine_three_works_with_tiles_and_the_engine() {
    let src = lcg(0x0E33_0000_0000_0001, 64 * 1024);
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_STORE).unwrap();
    assert_eq!(body.machine_id, MACHINE_STORE);
    let n = body.logical_len;
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    body.generation += 1;

    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(engine.worst_case_steps(), 0);
    assert!(engine
        .plan_ref()
        .regions()
        .iter()
        .all(|r| r.path == EnginePath::HotTile));

    // Every path is byte-exact, and byte_at/read_into match reader::read exactly.
    for (offset, length) in [(0u64, 1u64), (4095, 2), (4096, 4096), (n - 7, 7), (0, n)] {
        let want = read(&body, ReadRequest { offset, length }).unwrap();
        assert_eq!(
            engine.read(&body, ReadRequest { offset, length }).unwrap(),
            want
        );
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
fn machine_three_manifest_verify_and_file_roundtrip() {
    for profile in [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Dense { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ] {
        let src = lcg(0xF11E_0000_0000_0001, 8 * 1024);
        let body = create_machine(&src, profile, MACHINE_STORE).unwrap();
        assert!(verify(&body).unwrap().ok, "verify failed for {profile:?}");
        assert!(
            verify_deep(&body).unwrap().ok,
            "verify_deep failed for {profile:?}"
        );
        for (off, len) in [(0u64, 1u64), (17, 33), (8000, 100), (8191, 1)] {
            assert_eq!(
                read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: len
                    }
                )
                .unwrap(),
                src[off as usize..(off + len) as usize].to_vec()
            );
        }
        // The engine accepts any profile that carries a bounded path. For machine 3
        // a structureless `Root` body is coverable by the verbatim core itself
        // (`StructureRef::Core`, an `O(1)` slice with zero steps); a state-only
        // `Full` history is NOT an `AffineSummary` (that is machine-1 only) and
        // offers no bounded path, so it is rejected.
        if matches!(profile, ManifestationProfile::Full) {
            assert!(Engine::plan(&body, &generous(), &workload4k()).is_err());
        } else {
            let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
            assert!(engine.plan_ref().all_certified());
            if matches!(profile, ManifestationProfile::Root) {
                assert_eq!(engine.worst_case_steps(), 0);
                assert!(engine
                    .plan_ref()
                    .regions()
                    .iter()
                    .all(|r| r.path == EnginePath::DirectStorage
                        && matches!(r.structure, ekbloom::StructureRef::Core)));
            }
        }
    }
}

/// Write a body to a temp `.ekb`, reopen it, and confirm the machine id and bytes
/// survive the wire format.
#[test]
fn machine_three_is_recorded_in_the_format() {
    let src = lcg(0x0F11_0000_0000_0001, 4096);
    let body =
        create_machine(&src, ManifestationProfile::Path { depth: 4 }, MACHINE_STORE).unwrap();
    let dir = std::env::temp_dir();
    let path: PathBuf = dir.join(format!("ekbloom-machine3-{}.ekb", std::process::id()));
    ekbloom::writer::write_body(&path, &body).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let reopened = ekbloom::reader::open_bytes(&bytes).unwrap();
    assert_eq!(reopened.machine_id, MACHINE_STORE);
    assert_eq!(reopened.logical_len, src.len() as u64);
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn auto_policy_never_exceeds_raw_and_picks_the_right_machine() {
    // Uniform / incompressible: order-0 cannot shrink it, so `auto` must pick
    // machine 3 and the base footprint is exactly N (<= raw).
    let uniform = lcg(0xA070_0000_0000_0001, 32 * 1024);
    let body = create_auto(&uniform, ManifestationProfile::Root).unwrap();
    assert_eq!(body.machine_id, MACHINE_STORE, "uniform -> verbatim");
    assert_eq!(body.visible_bytes(), uniform.len() as u64);
    assert!(
        body.physical_bytes() <= uniform.len() as u64,
        "base footprint {} must be <= raw {}",
        body.physical_bytes(),
        uniform.len()
    );
    assert_eq!(
        read(
            &body,
            ReadRequest {
                offset: 0,
                length: uniform.len() as u64
            }
        )
        .unwrap(),
        uniform
    );

    // Skewed: machine 2 wins with V < N.
    let text = skewed(32 * 1024);
    let body = create_auto(&text, ManifestationProfile::Root).unwrap();
    assert_eq!(body.machine_id, MACHINE_RANS0, "skewed -> rANS");
    assert!(
        body.visible_bytes() < text.len() as u64,
        "skewed V/N = {}/{}",
        body.visible_bytes(),
        text.len()
    );
    assert_eq!(
        read(
            &body,
            ReadRequest {
                offset: 0,
                length: text.len() as u64
            }
        )
        .unwrap(),
        text
    );

    // Explicit overrides are honoured regardless of the policy.
    let forced3 = create_machine(&uniform, ManifestationProfile::Root, MACHINE_STORE).unwrap();
    assert_eq!(forced3.machine_id, MACHINE_STORE);
    let forced2 = create_machine(&uniform, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    assert_eq!(forced2.machine_id, MACHINE_RANS0);
    assert!(forced2.visible_bytes() > uniform.len() as u64);
}
