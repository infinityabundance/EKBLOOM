//! Physical-accounting invariant across the four machines.
//!
//! Pins the rule that every manifested structure's `physical_bytes()` equals its
//! serialized length, or differs from it by exactly one documented fixed
//! container header (never by a repeated *shared* header). In particular a
//! [`BlockIndex`]'s rANS model header lives once in the encoded `core`, so
//! `BlockIndex::physical_bytes()` must equal `BlockIndex::to_bytes().len()` and
//! must **not** re-count the machine-2/4 model.

use ekbloom::blocks::{BlockIndex, BLOCK_INDEX_PREFIX_LEN};
use ekbloom::checkpoint::{CheckpointSet, CKPT_FIXED_LEN};
use ekbloom::{
    create, create_machine, machine_by_id, manifest_hot_regions, AccessTrace, BloomShell,
    DirectoryKind, History, ManifestationBudget, ManifestationProfile, OrMachine, PackedHistory,
    PackedRestarts, PathBloom, SweepConfig, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I,
    MACHINE_STORE, NL2, TILE_SET_PREFIX_LEN,
};

/// `History::to_bytes` fixed header: `u64 generation ++ u64 logical_len ++
/// u64 core_len ++ [u8;32] core_hash ++ u64 count` = 64 bytes. Mirrors the private
/// `history::HEADER_LEN`.
const HISTORY_WIRE_HEADER: u64 = 8 + 8 + 8 + 32 + 8;
/// `BloomShell::to_bytes` fixed header (`kind u32 ++ depth u16 ++ generation u64
/// ++ logical_len u64 ++ entries u64 ++ status u32 ++ core_hash [32] ++ payload_len
/// u64` = 74 bytes). Mirrors the private `bloom::SHELL_HEADER_LEN`.
const BLOOM_SHELL_WIRE_HEADER: u64 = 4 + 2 + 8 + 8 + 8 + 4 + 32 + 8;

const MACHINES: [u32; 4] = [MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE, MACHINE_RANS0I];

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

/// An order-0 skewed source that machines 2 and 4 compress (`V < N`).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(if r < 60 {
            b'a'
        } else if r < 85 {
            b'b'
        } else if r < 95 {
            b'c'
        } else {
            b'd'
        });
    }
    out
}

fn core_for(machine: u32, source: &[u8]) -> Vec<u8> {
    create_machine(source, ManifestationProfile::Root, machine)
        .unwrap()
        .core
}

/// Fixed non-payload scaffolding in `TileSet::to_bytes`: the 96-byte prefix, the
/// `u64` length prefix on each directory vector (one for `Flat`, two for
/// `TwoLevelRadix`) and the trailing `u64` tile-blob length.
fn tile_wire_scaffold(kind: DirectoryKind) -> u64 {
    let directory_prefixes = match kind {
        DirectoryKind::Flat => 8,
        DirectoryKind::TwoLevelRadix => 16,
    };
    TILE_SET_PREFIX_LEN as u64 + directory_prefixes + 8
}

#[test]
fn structure_physical_bytes_match_serialized_lengths_across_machines() {
    let source = skewed(0xAC00_0000_0000_0001, 4096);
    for machine in MACHINES {
        let m = machine_by_id(machine).unwrap();
        let core = core_for(machine, &source);
        let n = source.len() as u64;

        // BlockIndex: exact equality — the shared model header is NOT re-counted.
        let block = BlockIndex::build_machine(&core, n, 16, machine).unwrap();
        assert_eq!(
            block.physical_bytes(),
            block.to_bytes().len() as u64,
            "machine {machine}: BlockIndex physical_bytes must equal its serialized length"
        );
        assert_eq!(
            block.physical_bytes(),
            BLOCK_INDEX_PREFIX_LEN as u64 + block.block_count() as u64 * block.entry_bytes(),
            "machine {machine}: BlockIndex is directory + restart records only"
        );

        // History: payload + fixed wire header.
        let history = History::build_with(m, &core, n, 1).unwrap();
        assert_eq!(
            history.to_bytes().len() as u64,
            history.physical_bytes() + HISTORY_WIRE_HEADER,
            "machine {machine}: History relation"
        );

        // CheckpointSet: payload + fixed wire header.
        let cps = CheckpointSet::build_with(m, &core, n, 64).unwrap();
        assert_eq!(
            cps.to_bytes().len() as u64,
            cps.physical_bytes() + CKPT_FIXED_LEN as u64,
            "machine {machine}: CheckpointSet relation"
        );

        // BloomShell (path bloom): payload + fixed shell header.
        let shell = PathBloom::build_with(m, &core, n, 4, 1).unwrap();
        assert_eq!(
            shell.to_bytes().len() as u64,
            shell.physical_bytes() + BLOOM_SHELL_WIRE_HEADER,
            "machine {machine}: BloomShell relation"
        );

        // TileSet: materialised data + directory descriptors + the fixed
        // serialization scaffolding (prefix, vector length prefixes, blob length).
        let mut body = create_machine(&source, ManifestationProfile::Root, machine).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, n)],
            counts: vec![1],
        };
        for kind in [DirectoryKind::Flat, DirectoryKind::TwoLevelRadix] {
            body.tiles.clear();
            body.generation += 1;
            ekbloom::TileSet::manifest_hot_tiles(&mut body, &trace, 10, kind, &generous()).unwrap();
            let tile_set = body.tiles.last().unwrap();
            assert_eq!(
                tile_set.to_bytes().len() as u64,
                tile_set.physical_bytes() + tile_wire_scaffold(kind),
                "machine {machine}: TileSet relation ({kind:?})"
            );
        }
    }

    // A `BloomShell` obtained by hand must obey the same fixed relation, so the
    // documented constant can never silently drift from the wire layout.
    let shell = BloomShell {
        kind: ekbloom::BloomKind::Path,
        depth: 4,
        generation: 0,
        logical_len: 0,
        entries: 0,
        payload: vec![0u8; 22],
        status: ekbloom::Certification::Certified,
        core_hash: [0u8; 32],
    };
    assert_eq!(
        shell.to_bytes().len() as u64,
        shell.physical_bytes() + BLOOM_SHELL_WIRE_HEADER
    );
}

#[test]
fn body_physical_bytes_counts_each_structure_exactly_once() {
    let source = skewed(0xAC00_0000_0000_0002, 4096);
    for machine in MACHINES {
        let mut body = create_machine(&source, ManifestationProfile::Root, machine).unwrap();
        let n = body.logical_len;
        let base = body.physical_bytes();
        assert_eq!(base, body.core.len() as u64, "root body is the core alone");

        // Appending a block index grows the accounted body by exactly its
        // serialized length — never by an extra (shared) model header.
        let block = BlockIndex::build_machine(&body.core, n, 16, machine).unwrap();
        let wire = block.to_bytes().len() as u64;
        body.blocks.push(block);
        body.generation += 1;
        assert_eq!(
            body.physical_bytes() - base,
            wire,
            "machine {machine}: BlockIndex contributes exactly its serialized length"
        );

        // History contributes its payload; the container header is not double-counted.
        let history =
            History::build_with(machine_by_id(machine).unwrap(), &body.core, n, 2).unwrap();
        let history_payload = history.physical_bytes();
        body.history = Some(history);
        body.generation += 1;
        assert_eq!(body.physical_bytes() - base - wire, history_payload);

        // Checkpoint contributes its payload.
        let cps =
            CheckpointSet::build_with(machine_by_id(machine).unwrap(), &body.core, n, 64).unwrap();
        let cps_payload = cps.physical_bytes();
        body.checkpoints.push(cps);
        body.generation += 1;
        assert_eq!(
            body.physical_bytes() - base - wire - history_payload,
            cps_payload
        );

        // `inspect` attributes the same total to the same components.
        let report = ekbloom::inspect(&body);
        assert_eq!(report.physical_bytes, body.physical_bytes());
        let summed: u64 = report.components.iter().map(|c| c.bytes).sum();
        assert_eq!(summed, body.physical_bytes());
    }
}

#[test]
fn regional_shells_packed_forms_and_certificates_obey_their_relations() {
    let src = skewed(0xAC00_0000_0000_0003, 1024);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;

    // Packed machine-1 forms: payload + a fixed 16-byte container header.
    let packed_history = PackedHistory::build(&core, n).unwrap();
    assert_eq!(
        packed_history.to_bytes().len() as u64,
        packed_history.physical_bytes() + 16
    );
    let packed_restarts = PackedRestarts::build(&core, n, 8).unwrap();
    assert_eq!(
        packed_restarts.to_bytes().len() as u64,
        packed_restarts.physical_bytes() + 16
    );

    // Regional blooms and their certificates: each shell obeys the same fixed
    // shell-header relation, and `inspect` counts each bloom/certificate once.
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_NL2).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    let cfg = SweepConfig {
        iterations: 1,
        seed: 1,
    };
    manifest_hot_regions(&mut body, &trace, &generous(), &cfg).unwrap();
    assert!(
        body.blooms
            .iter()
            .any(|b| b.kind == ekbloom::BloomKind::Regional),
        "a regional shell must be materialised"
    );
    for bloom in &body.blooms {
        assert_eq!(
            bloom.to_bytes().len() as u64,
            bloom.physical_bytes() + BLOOM_SHELL_WIRE_HEADER
        );
    }
    assert_eq!(body.certificates.len(), body.blooms.len());
    let report = ekbloom::inspect(&body);
    let bloom_bytes: u64 = body.blooms.iter().map(|b| b.physical_bytes()).sum();
    let cert_bytes: u64 = body
        .certificates
        .iter()
        .map(|c| c.to_bytes().len() as u64)
        .sum();
    let bloom_components: u64 = report
        .components
        .iter()
        .filter(|c| c.kind == "bloom")
        .map(|c| c.bytes)
        .sum();
    let cert_components: u64 = report
        .components
        .iter()
        .filter(|c| c.kind == "certificate")
        .map(|c| c.bytes)
        .sum();
    assert_eq!(bloom_components, bloom_bytes);
    assert_eq!(cert_components, cert_bytes);

    // A Path profile: the certificate's accounted contribution is its exact wire
    // length (counted once), and the shell obeys its fixed relation.
    let path_body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    for (bloom, cert) in path_body.blooms.iter().zip(path_body.certificates.iter()) {
        assert_eq!(
            bloom.to_bytes().len() as u64,
            bloom.physical_bytes() + BLOOM_SHELL_WIRE_HEADER
        );
        assert!(!cert.to_bytes().is_empty());
    }
}
