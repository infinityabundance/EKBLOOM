//! Tests for the interleaved order-0 rANS machine (`OrANS-RANS0I`, `machine_id = 4`).
//!
//! Covers: exact roundtrip over every source category and several lane counts;
//! byte-exactness against `reader::read` and against machine 2's symbol sequence;
//! state-log / checkpoint / block / path-bloom exactness; invalid-header rejection;
//! and that machines 1/2/3 are unaffected.

use ekbloom::machine_rans_i::{encode_with_k, RansIContext};
use ekbloom::{
    create_machine, machine_by_id, reader, state_log_with, verify_deep, write_body, BlockIndex,
    CheckpointSet, FileBody, ManifestationProfile, OrMachine, PathBloom, ReadRequest, MACHINE_NL2,
    MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE, RANS0, RANS0I,
};

/// A deterministic LCG byte stream (high entropy).
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

/// A skewed 4-symbol source (order-0 compressible: `V < N`).
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

fn categories() -> Vec<(&'static str, Vec<u8>)> {
    let mut all_bytes = Vec::with_capacity(256);
    all_bytes.extend(0u8..=255);
    vec![
        ("empty", Vec::new()),
        ("one", vec![0x42]),
        ("all_bytes", all_bytes),
        ("repeating", vec![b'x'; 5000]),
        ("alternating", (0..4096).map(|i| (i % 2) as u8).collect()),
        ("skewed", skewed(0xABCD_1234, 8192)),
        ("random", lcg(0x0FED_CBA9, 8192)),
    ]
}

const LANES: [usize; 4] = [1, 2, 4, 8];

#[test]
fn roundtrip_every_category_and_lane_count() {
    for (name, src) in categories() {
        for k in LANES {
            let core = encode_with_k(&src, k).unwrap();
            assert!(
                RANS0I.check_core(&core, src.len() as u64).is_ok(),
                "{name} k={k} check_core"
            );
            let got = RANS0I.decode(&core, src.len() as u64).unwrap();
            assert_eq!(got, src, "{name} k={k} decode mismatch");
        }
    }
}

#[test]
fn default_encode_roundtrips() {
    let src = skewed(0x1111_2222, 4096);
    let core = RANS0I.encode(&src).unwrap();
    assert_eq!(RANS0I.decode(&core, src.len() as u64).unwrap(), src);
}

#[test]
fn every_lane_count_decodes_the_same_symbols() {
    let src = skewed(0x5EED_5EED, 3000);
    let reference = skewed(0x5EED_5EED, 3000);
    for k in [1usize, 2, 4, 8] {
        let core = encode_with_k(&src, k).unwrap();
        let got = RANS0I.decode(&core, src.len() as u64).unwrap();
        assert_eq!(got, reference, "k={k} produced a different symbol sequence");
    }
}

#[test]
fn decode_matches_machine2_symbols() {
    // Both machines use the same static order-0 model, so the decoded symbol
    // sequence must be identical even though the cores differ.
    for (name, src) in categories() {
        let m2 = RANS0.encode(&src).unwrap();
        let m4 = encode_with_k(&src, 4).unwrap();
        let a = RANS0.decode(&m2, src.len() as u64).unwrap();
        let b = RANS0I.decode(&m4, src.len() as u64).unwrap();
        assert_eq!(a, b, "{name}: machine-4 symbols differ from machine 2");
        assert_eq!(&b, &src, "{name}: machine-4 decode is not the source");
    }
}

#[test]
fn reveal_sequence_matches_bulk_decode_and_state_log() {
    let src = skewed(0x7A17_0001, 1500);
    let core = encode_with_k(&src, 4).unwrap();
    let n = src.len() as u64;

    // `reveal` one symbol at a time.
    let mut omega = RANS0I.init(&core).unwrap();
    let mut stepwise = Vec::with_capacity(src.len());
    for _ in 0..n {
        let before = omega;
        assert_eq!(RANS0I.peek(&core, &before).unwrap(), src[stepwise.len()]);
        stepwise.push(RANS0I.reveal(&core, &mut omega).unwrap());
    }
    assert_eq!(stepwise, src);
    assert_eq!(RANS0I.decode(&core, n).unwrap(), src);

    // The state log has one entry per position plus the terminal state, and its
    // first entry is genesis.
    let log = state_log_with(&RANS0I, &core, n).unwrap();
    assert_eq!(log.len(), src.len() + 1);
    assert_eq!(log[0], RANS0I.init(&core).unwrap().state);

    // `prepare`-based decode matches too.
    let ctx = RansIContext::from_core(&core).unwrap();
    let mut omega = RANS0I.init(&core).unwrap();
    let mut out = Vec::new();
    ctx.decode_into(&core, &mut omega, n, &mut out).unwrap();
    assert_eq!(out, src);
}

#[test]
fn checkpoint_restores_the_complete_state() {
    let src = skewed(0xC0DE_0001, 2048);
    let core = RANS0I.encode(&src).unwrap();
    let n = src.len() as u64;
    let interval = 64u32;
    let cps = CheckpointSet::build_with(&RANS0I, &core, n, interval).unwrap();

    for i in 0..=n {
        let restored = cps.restore_with(&RANS0I, &core, i).unwrap();
        // Resuming from the restored complete state must reveal `src[i..]` exactly.
        let mut omega = restored;
        let ctx = RansIContext::from_core(&core).unwrap();
        let take = (n - i).min(8);
        let mut out = Vec::new();
        ctx.decode_into(&core, &mut omega, take, &mut out).unwrap();
        assert_eq!(out, src[i as usize..(i + take) as usize]);
    }
}

#[test]
fn block_index_reads_are_exact_bounded_and_certified() {
    let src = skewed(0xB10C_0002, 4096);
    let core = encode_with_k(&src, 4).unwrap();
    let n = src.len() as u64;
    for b in [1u32, 7, 64, 256, 1024, 4096] {
        let idx = BlockIndex::build_machine(&core, n, b, MACHINE_RANS0I).unwrap();
        assert!(idx.certify(&core).unwrap());
        assert!(idx.verify_against(&core).unwrap());
        for &(off, len) in &[
            (0u64, 0u64),
            (0, 1),
            (1, 9),
            (63, 2),
            (64, 200),
            (n - 3, 3),
            (n, 0),
        ] {
            let (got, steps) = idx.read_range_counted(&core, off, len).unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
            assert!(steps <= idx.max_steps(off, len));
        }
    }
}

#[test]
fn block_index_wire_roundtrip_is_exact() {
    let src = skewed(0xB10C_0003, 2048);
    let core = encode_with_k(&src, 4).unwrap();
    let n = src.len() as u64;
    let idx = BlockIndex::build_machine(&core, n, 256, MACHINE_RANS0I).unwrap();
    let bytes = idx.to_bytes();
    let back = BlockIndex::from_bytes(&bytes).unwrap();
    assert_eq!(back, idx);
    assert_eq!(back.to_bytes(), bytes);
    assert!(back.verify());
    assert!(back.verify_against(&core).unwrap());

    // A tampered lane state is rejected.
    let mut tampered = idx.clone();
    tampered.entries[1].lanes[0] ^= 0xFFFF;
    assert!(!tampered.certify(&core).unwrap());
    assert!(!tampered.verify_against(&core).unwrap());
}

#[test]
fn path_bloom_is_exact() {
    let src = skewed(0xB100_0004, 1024);
    let core = RANS0I.encode(&src).unwrap();
    let n = src.len() as u64;
    for depth in [1u16, 2, 4, 8, 16, 32] {
        let shell = PathBloom::build_with(&RANS0I, &core, n, depth, 1).unwrap();
        let pb = PathBloom::from_shell(&shell).unwrap();
        for pos in 0..n {
            assert_eq!(pb.symbol_at(pos).unwrap(), src[pos as usize]);
        }
    }
}

#[test]
fn reader_read_matches_source_for_every_profile() {
    let src = skewed(0x8EAD_0005, 3000);
    let n = src.len() as u64;
    let profiles = [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ];
    let ranges = [
        (0u64, 1u64),
        (0, 64),
        (7, 100),
        (255, 300),
        (n - 5, 5),
        (n, 0),
    ];
    for profile in profiles {
        let body = create_machine(&src, profile, MACHINE_RANS0I).unwrap();
        assert_eq!(body.machine_id, MACHINE_RANS0I);
        for (offset, length) in ranges {
            let got = reader::read(&body, ReadRequest { offset, length }).unwrap();
            assert_eq!(
                got,
                &src[offset as usize..(offset + length) as usize],
                "profile {profile:?} read ({offset},{length})"
            );
        }
        // `snapshot_at` is complete and resumes exactly.
        let snap = body.snapshot_at(100).unwrap();
        let mut omega = snap;
        let ctx = RansIContext::from_core(&body.core).unwrap();
        let mut out = Vec::new();
        ctx.decode_into(&body.core, &mut omega, 16, &mut out)
            .unwrap();
        assert_eq!(out, src[100..116]);
    }
}

#[test]
fn invalid_headers_error_cleanly() {
    // Too short.
    assert!(RansIContext::from_core(&[]).is_err());
    assert!(RansIContext::from_core(&[0u8; 200]).is_err());

    let good = encode_with_k(b"some compressible text some compressible text", 4).unwrap();

    // Lane count out of range.
    let mut bad_k = good.clone();
    bad_k[512..516].copy_from_slice(&0u32.to_le_bytes());
    assert!(RansIContext::from_core(&bad_k).is_err());
    bad_k[512..516].copy_from_slice(&16u32.to_le_bytes());
    assert!(RansIContext::from_core(&bad_k).is_err());

    // Frequencies that do not sum to M.
    let mut bad_freq = good.clone();
    bad_freq[0..2].copy_from_slice(&0u16.to_le_bytes());
    assert!(RansIContext::from_core(&bad_freq).is_err());

    // A lane state below RANS_L.
    let mut bad_state = good.clone();
    let off = 516; // STATES_OFFSET for k = 1..; first lane state
    bad_state[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(RansIContext::from_core(&bad_state).is_err());

    // A truncated renorm stream is rejected during decode.
    let mut short = good.clone();
    short.truncate(520);
    assert!(RANS0I.decode(&short, 1000).is_err());
}

#[test]
fn machines_one_two_three_are_unaffected() {
    assert_eq!(
        machine_by_id(MACHINE_NL2).unwrap().machine_id(),
        MACHINE_NL2
    );
    assert_eq!(
        machine_by_id(MACHINE_RANS0).unwrap().machine_id(),
        MACHINE_RANS0
    );
    assert_eq!(
        machine_by_id(MACHINE_STORE).unwrap().machine_id(),
        MACHINE_STORE
    );
    assert_eq!(
        machine_by_id(MACHINE_RANS0I).unwrap().machine_id(),
        MACHINE_RANS0I
    );

    let src = skewed(0x1234_9999, 2048);
    let n = src.len() as u64;
    for machine in [MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE] {
        let m = machine_by_id(machine).unwrap();
        let core = m.encode(&src).unwrap();
        assert_eq!(m.decode(&core, n).unwrap(), src, "machine {machine} broke");
        let body = create_machine(&src, ManifestationProfile::Root, machine).unwrap();
        let got = reader::read(
            &body,
            ReadRequest {
                offset: 100,
                length: 200,
            },
        )
        .unwrap();
        assert_eq!(got, src[100..300], "machine {machine} reader broke");
    }

    // Machine-2 checkpoints still pack to 2-byte records only for machine 1; a
    // machine-2 snapshot is still a plain `(state, cursor)` with zero lanes.
    let core = RANS0.encode(&src).unwrap();
    let cp = CheckpointSet::build_with(&RANS0, &core, n, 64).unwrap();
    assert!(cp
        .resume
        .iter()
        .all(|s| s.lanes == [0u32; 8] && s.lane == 0));
}

#[test]
fn ekb_file_roundtrip_with_wide_checkpoints_and_blocks() {
    let src = skewed(0xF11E_0006, 4096);
    let n = src.len() as u64;
    let mut body = create_machine(
        &src,
        ManifestationProfile::Sparse { interval: 64 },
        MACHINE_RANS0I,
    )
    .unwrap();
    let idx = BlockIndex::build_machine(&body.core, n, 256, MACHINE_RANS0I).unwrap();
    body.blocks.push(idx);

    let dir = std::env::temp_dir().join(format!("ekbloom-m4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m4.ekb");
    write_body(&path, &body).unwrap();

    // In-memory reconstruction from the sections (wide checkpoints + blocks).
    let reopened = reader::open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(reopened.machine_id, MACHINE_RANS0I);
    assert!(verify_deep(&reopened).unwrap().ok);

    // The bounded file reader (its own section parsers).
    let file_body = FileBody::open(&path).unwrap();
    for &(off, len) in &[(0u64, 0u64), (0, 1), (7, 100), (100, 200), (n - 5, 5)] {
        let expect = &src[off as usize..(off + len) as usize];
        let req = ReadRequest {
            offset: off,
            length: len,
        };
        assert_eq!(reader::read(&reopened, req).unwrap(), expect);
        assert_eq!(reader::read_file(&path, req).unwrap(), expect);
        assert_eq!(file_body.read(req).unwrap(), expect);
    }
    std::fs::remove_dir_all(&dir).ok();
}
