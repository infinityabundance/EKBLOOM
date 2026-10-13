//! Machine 2 (`OrANS-RANS0`, an order-0 range-ANS codec) integration tests.
//!
//! These prove that machine 2 (a) roundtrips exactly, (b) actually compresses
//! skewed sources (`V < N`) while offering no win on uniform random sources, and
//! (c) drives every manifested structure (state log, history, checkpoints, path
//! blooms, and end-to-end `read`/`verify`) byte-exactly — without disturbing
//! machine 1.

use ekbloom::bloom_path::PathBloom;
use ekbloom::checkpoint::{CheckpointSet, SUPPORTED_INTERVALS};
use ekbloom::history::History;
use ekbloom::machine::{
    machine_by_id, state_log_with, OrMachine, MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE, NL2,
};
use ekbloom::machine_rans::{Rans0, RansContext};
use ekbloom::manifest::create_machine;
use ekbloom::reader::read;
use ekbloom::state::Snapshot;
use ekbloom::{verify, verify_deep, ManifestationProfile, ReadRequest};

/// The machine 2 instance under test.
const RANS0: Rans0 = Rans0;

/// Deterministic pseudorandom bytes from a small LCG (no new dependencies).
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

/// English-like low-entropy text: a small alphabet with a heavily skewed
/// frequency distribution (mostly spaces and common letters).
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

/// The reference snapshot sequence for `core` over `n` symbols.
fn snapshots(machine: &dyn OrMachine, core: &[u8], n: u64) -> Vec<Snapshot> {
    let mut omega = machine.init(core).unwrap();
    let mut out = vec![omega];
    for _ in 0..n {
        machine.reveal(core, &mut omega).unwrap();
        out.push(omega);
    }
    out
}

#[test]
fn roundtrip_exact_for_every_shape() {
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
        let core = RANS0.encode(&src).unwrap();
        let n = src.len() as u64;
        // `check_core` accepts, and decode is the exact inverse of encode.
        RANS0.check_core(&core, n).unwrap();
        let back = RANS0.decode(&core, n).unwrap();
        assert_eq!(back, src, "roundtrip failed for {n} bytes");

        // `peek` agrees with `reveal` step by step.
        let mut omega = RANS0.init(&core).unwrap();
        for &expected in src.iter() {
            assert_eq!(RANS0.peek(&core, &omega).unwrap(), expected);
            assert_eq!(RANS0.reveal(&core, &mut omega).unwrap(), expected);
        }
        // The state log begins at the genesis state and has n+1 entries.
        let log = state_log_with(&RANS0, &core, n).unwrap();
        assert_eq!(log.len() as u64, n + 1);
        assert_eq!(log[0], RANS0.init(&core).unwrap().state);
    }
}

#[test]
fn empty_source_has_only_the_model_header() {
    let core = RANS0.encode(&[]).unwrap();
    assert_eq!(core.len(), 512 + 4);
    assert!(RANS0.decode(&core, 0).unwrap().is_empty());
}

#[test]
fn skewed_source_compresses() {
    let src = skewed(16 * 1024);
    let core = RANS0.encode(&src).unwrap();
    assert!(
        core.len() < src.len(),
        "expected V < N for skewed input: core {} vs source {}",
        core.len(),
        src.len()
    );
    assert_eq!(RANS0.decode(&core, src.len() as u64).unwrap(), src);
    // Reference the real ratio in the test log for evidence.
    eprintln!(
        "machine 2 skewed: V/N = {} / {} = {:.3}",
        core.len(),
        src.len(),
        core.len() as f64 / src.len() as f64
    );
}

#[test]
fn random_source_is_not_compressed() {
    let src = lcg(0x0123_4567_89AB_CDEF, 32 * 1024);
    let core = RANS0.encode(&src).unwrap();
    assert!(
        core.len() >= src.len(),
        "order-0 offers no win on uniform data: core {} vs source {}",
        core.len(),
        src.len()
    );
    assert_eq!(RANS0.decode(&core, src.len() as u64).unwrap(), src);
    eprintln!(
        "machine 2 random: V/N = {} / {} = {:.3}",
        core.len(),
        src.len(),
        core.len() as f64 / src.len() as f64
    );
}

#[test]
fn history_checkpoint_and_path_bloom_are_byte_exact() {
    let src = skewed(300);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    let machine = machine_by_id(MACHINE_RANS0).unwrap();
    let decoded = machine.decode(&core, n).unwrap();
    let logs = state_log_with(machine, &core, n).unwrap();
    let snaps = snapshots(machine, &core, n);

    // History: the stored projection matches a fresh recomputation.
    let history = History::build_with(machine, &core, n, 3).unwrap();
    history.verify_against_with(machine, &core).unwrap();
    for i in 0..=n {
        assert_eq!(history.state_at(i), Some(logs[i as usize]));
    }

    // Checkpoints: every restart ordinal restores exactly, and the wire form
    // round-trips under the machine-aware parser.
    for &k in &[1u32, 4, 16, 64] {
        assert!(SUPPORTED_INTERVALS.contains(&k));
        let cps = CheckpointSet::build_with(machine, &core, n, k).unwrap();
        let bytes = cps.to_bytes();
        let parsed = CheckpointSet::from_bytes_with(machine, &bytes).unwrap();
        assert_eq!(parsed, cps);
        for ordinal in 0..cps.count {
            let target = ordinal * u64::from(k);
            assert_eq!(
                cps.restore_with(machine, &core, target).unwrap(),
                snaps[target as usize],
                "checkpoint restart mismatch at {target} (k={k})"
            );
        }
    }

    // Path bloom: hot symbols and cold successors reproduce the machine.
    for &d in &[1u16, 2, 4, 8] {
        let shell = PathBloom::build_with(machine, &core, n, d, 5).unwrap();
        let pb = PathBloom::from_shell(&shell).unwrap();
        for pos in 0..n {
            assert_eq!(pb.symbol_at(pos).unwrap(), decoded[pos as usize]);
        }
        let dd = u64::from(d);
        for (j, &cold) in pb.cold.iter().enumerate() {
            let end = (dd * (j as u64 + 1)).min(n);
            assert_eq!(cold, logs[end as usize]);
        }
    }
}

#[test]
fn invalid_model_headers_error_cleanly() {
    // Truncated header.
    assert!(RANS0.check_core(&[0u8; 100], 1).is_err());
    assert!(RANS0.check_core(&[], 0).is_err());

    // Frequencies that do not sum to M.
    let mut core = vec![0u8; 512 + 4];
    for i in 0..256usize {
        let f = if i == 0 { 17u16 } else { 16u16 };
        core[i * 2..i * 2 + 2].copy_from_slice(&f.to_le_bytes());
    }
    let err = RANS0.check_core(&core, 1).unwrap_err();
    assert!(matches!(err, ekbloom::EkbloomError::BadSection { .. }));

    // A zero frequency.
    let mut core = vec![0u8; 512 + 4];
    for i in 0..256usize {
        let f = if i == 0 {
            0u16
        } else if i == 1 {
            32u16
        } else {
            16u16
        };
        core[i * 2..i * 2 + 2].copy_from_slice(&f.to_le_bytes());
    }
    let err = RANS0.check_core(&core, 1).unwrap_err();
    assert!(matches!(err, ekbloom::EkbloomError::BadSection { .. }));

    // A valid header with no renormalisation bytes cannot yield a long stream.
    let core = RANS0.encode(&skewed(300)).unwrap();
    let header = &core[..512 + 4];
    assert!(RANS0.decode(header, 300).is_err());
}

#[test]
fn end_to_end_manifest_verify_and_read() {
    let src = skewed(300);
    let body = create_machine(&src, ManifestationProfile::MaxSpeed, MACHINE_RANS0).unwrap();
    assert_eq!(body.machine_id, MACHINE_RANS0);
    assert_eq!(body.visible_bytes(), body.core.len() as u64);

    assert!(verify(&body).unwrap().ok, "verify failed");
    assert!(verify_deep(&body).unwrap().ok, "verify_deep failed");

    // Full read and several sub-ranges agree with the source through every path.
    let full = read(
        &body,
        ReadRequest {
            offset: 0,
            length: src.len() as u64,
        },
    )
    .unwrap();
    assert_eq!(full, src);
    for &(off, len) in &[(0u64, 1u64), (17, 33), (200, 100), (299, 1)] {
        let got = read(
            &body,
            ReadRequest {
                offset: off,
                length: len,
            },
        )
        .unwrap();
        assert_eq!(got, src[off as usize..(off + len) as usize].to_vec());
    }

    // Root profile reads replay the machine from `Omega_0`.
    let root = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    assert!(verify(&root).unwrap().ok);
    let mid = read(
        &root,
        ReadRequest {
            offset: 100,
            length: 50,
        },
    )
    .unwrap();
    assert_eq!(mid, src[100..150].to_vec());
}

#[test]
fn machine_one_is_unaffected() {
    for src in [b"".as_slice(), b"A", &skewed(256), &lcg(7, 512)] {
        let core = NL2.encode(src).unwrap();
        assert_eq!(core.len(), src.len(), "machine 1 must keep V = N");
        assert_eq!(NL2.decode(&core, src.len() as u64).unwrap(), src);
        assert_eq!(machine_by_id(MACHINE_NL2).unwrap().machine_id(), 1);
    }
    assert!(machine_by_id(MACHINE_STORE).is_some());
    // Machine 4 (interleaved rANS) and machine 5 (packed) are registered.
    assert_eq!(machine_by_id(4).unwrap().machine_id(), 4);
    assert_eq!(machine_by_id(5).unwrap().machine_id(), 5);
    // Machines 6 (segmented), 7 (word16) and 8 (independent lanes) are
    // registered; higher ids are not.
    assert_eq!(machine_by_id(6).unwrap().machine_id(), 6);
    assert_eq!(machine_by_id(7).unwrap().machine_id(), 7);
    assert_eq!(machine_by_id(8).unwrap().machine_id(), 8);
    assert!(machine_by_id(9).is_none());
    assert_eq!(MACHINE_NL2, 1);
    assert_eq!(MACHINE_RANS0, 2);
    assert_eq!(MACHINE_STORE, 3);
}

/// The reusable prepared decoder context must reproduce the authoritative
/// per-symbol symbol/state sequence for every source category, byte-for-byte.
#[test]
fn prepared_context_matches_per_symbol_reveal() {
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x5A],
        (0u8..=255).collect(),
        vec![7u8; 300],
        (0..1024u32)
            .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
            .collect(),
        skewed(4096),
        lcg(0xABCD_1234_5678_9ABC, 4096),
    ];
    for src in cases {
        let core = RANS0.encode(&src).unwrap();
        let n = src.len() as u64;

        // Prepared bulk decode equals the source.
        let prepared = RANS0.decode(&core, n).unwrap();
        assert_eq!(prepared, src);

        // Per-symbol authoritative reveal reproduces the same symbols, and the
        // prepared context (built once) reproduces the same symbol AND state
        // sequence as the per-symbol path.
        let mut omega = RANS0.init(&core).unwrap();
        let mut per_symbol = Vec::with_capacity(src.len());
        let mut states = vec![omega];
        for _ in 0..n {
            per_symbol.push(RANS0.reveal(&core, &mut omega).unwrap());
            states.push(omega);
        }
        assert_eq!(per_symbol, prepared);

        let ctx = RansContext::from_core(&core).unwrap();
        let mut omega = RANS0.init(&core).unwrap();
        for (i, &expect) in src.iter().enumerate() {
            assert_eq!(ctx.peek(&core, &omega).unwrap(), expect, "peek at {i}");
            assert_eq!(
                ctx.reveal(&core, &mut omega).unwrap(),
                expect,
                "reveal at {i}"
            );
            assert_eq!(omega, states[i + 1], "state mismatch at {i}");
        }

        // The overridden state log matches the per-symbol state sequence exactly.
        let log = state_log_with(&RANS0, &core, n).unwrap();
        for (i, s) in states.iter().enumerate() {
            assert_eq!(s.state, log[i], "state-log mismatch at {i}");
        }
        assert_eq!(log[0], RANS0.init(&core).unwrap().state);
    }
}
