//! Exactness, byte-layout and bounds tests for `ekbloom::packed`.
//!
//! Only the public API is exercised. The 16-bit packed forms are checked against
//! the authoritative machine state log for every position on small inputs (and
//! sampled on larger ones), replay is shown to stay within `stride - 1` steps,
//! `state_from_hot` is checked to reproduce `x_i` from the manifested symbol
//! surface, and the wire forms are shown to round-trip exactly and reject
//! truncation. Packing must not disturb the existing machine-1 structures.

use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::packed::{state_from_hot, PackedHistory, PackedRestarts};
use ekbloom::{CheckpointSet, History, Snapshot};

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

fn sample_sources() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        (0u8..=255).collect(),
        vec![3u8; 64],
        (0..65u32)
            .map(|i| if i % 2 == 0 { 0x11 } else { 0xEE })
            .collect(),
        (0..17u32).map(|i| (i * 7) as u8).collect(),
        lcg(0xDEAD_BEEF_1234_5678, 300),
        lcg(4, 1024),
    ]
}

/// Stride shifts exercised on small inputs: stride `1, 2, 4, ..., 256`.
const SMALL_SHIFTS: [u8; 9] = [0, 1, 2, 3, 4, 5, 6, 7, 8];

#[test]
fn snapshot_at_matches_state_log_for_all_positions_and_strides() {
    for src in sample_sources() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let log = state_log(&core, n).unwrap();

        for sh in SMALL_SHIFTS {
            let pr = PackedRestarts::build(&core, n, sh).unwrap();
            assert_eq!(pr.logical_len, n);
            for i in 0..=n {
                let expect = Snapshot::new(log[i as usize], i);
                assert_eq!(
                    pr.snapshot_at(&core, i).unwrap(),
                    expect,
                    "snapshot mismatch n={n} sh={sh} i={i}"
                );
                // The covering checkpoint sits at `index << sh`.
                let (ckpt, _idx) = pr.position_of(i).unwrap();
                assert_eq!(ckpt, (i >> sh) << sh);
                assert!(i - ckpt < (1u64 << sh));
            }
            // Past the terminal state there is no covering restart.
            assert_eq!(pr.position_of(n + 1), None);
            assert!(pr.snapshot_at(&core, n + 1).is_err());
        }
    }
}

#[test]
fn snapshot_at_matches_state_log_sampled_larger() {
    let src = lcg(0x0FED_CBA9_8765_4321, 4097);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    let log = state_log(&core, n).unwrap();

    for &sh in &[0u8, 1, 4, 6, 8, 12] {
        let pr = PackedRestarts::build(&core, n, sh).unwrap();
        // Stride 1 is exact for every position; larger strides are sampled.
        let step = (1u64 << sh).max(1);
        let mut positions: Vec<u64> = Vec::new();
        let mut i = 0u64;
        while i <= n {
            positions.push(i);
            i += step;
        }
        // Always include the terminal position and a few off-boundary probes.
        positions.push(n);
        for k in 0..8u64 {
            if k <= n {
                positions.push(k);
            }
        }
        for &i in &positions {
            assert_eq!(
                pr.snapshot_at(&core, i).unwrap(),
                Snapshot::new(log[i as usize], i),
                "sampled mismatch n={n} sh={sh} i={i}"
            );
        }
    }
}

#[test]
fn replay_steps_are_bounded_by_stride_minus_one() {
    let src = lcg(0x0000_1111_2222_3333, 300);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    let log = state_log(&core, n).unwrap();

    for sh in SMALL_SHIFTS {
        let stride = 1u64 << sh;
        let pr = PackedRestarts::build(&core, n, sh).unwrap();
        assert_eq!(pr.max_replay_steps(), stride - 1);

        for i in 0..=n {
            let (ckpt, idx) = pr.position_of(i).unwrap();
            let mut omega = Snapshot::new(u32::from(pr.states[idx]), ckpt);
            let mut steps: u64 = 0;
            while omega.cursor < i {
                NL2.reveal(&core, &mut omega).unwrap();
                steps += 1;
                assert!(steps < stride, "exceeded stride-1: sh={sh} i={i}");
            }
            assert_eq!(omega, Snapshot::new(log[i as usize], i));
            assert_eq!(steps, i - ckpt);
        }
    }
}

#[test]
fn packed_history_matches_state_log_for_all_positions() {
    for src in sample_sources() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let log = state_log(&core, n).unwrap();

        let ph = PackedHistory::build(&core, n).unwrap();
        assert_eq!(ph.states.len() as u64, n + 1);
        for i in 0..=n {
            assert_eq!(
                ph.state_at(i),
                Some(log[i as usize] as u16),
                "history mismatch n={n} i={i}"
            );
        }
        assert_eq!(ph.state_at(n + 1), None);
    }
}

#[test]
fn packed_byte_counts_are_exact() {
    for src in sample_sources() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;

        let ph = PackedHistory::build(&core, n).unwrap();
        assert_eq!(ph.physical_bytes(), 2 * (n + 1));
        assert_eq!(ph.to_bytes().len() as u64, 16 + 2 * (n + 1));

        for sh in SMALL_SHIFTS {
            let stride = 1u64 << sh;
            let count = n / stride + 1;
            let pr = PackedRestarts::build(&core, n, sh).unwrap();
            assert_eq!(pr.states.len() as u64, count);
            assert_eq!(pr.physical_bytes(), 2 * count);
            assert_eq!(pr.to_bytes().len() as u64, 16 + 2 * count);
        }
    }
}

#[test]
fn byte_roundtrips_are_exact_and_truncation_rejected() {
    let core = NL2.encode(&lcg(0x5150_5150_5150_5150, 259)).unwrap();
    let n = core.len() as u64;

    // Packed history.
    let ph = PackedHistory::build(&core, n).unwrap();
    let bytes = ph.to_bytes();
    assert_eq!(PackedHistory::from_bytes(&bytes).unwrap(), ph);
    for cut in 0..bytes.len() {
        assert!(
            PackedHistory::from_bytes(&bytes[..cut]).is_err(),
            "history truncation accepted at {cut}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(PackedHistory::from_bytes(&trailing).is_err());

    // Packed restarts.
    for sh in SMALL_SHIFTS {
        let pr = PackedRestarts::build(&core, n, sh).unwrap();
        let bytes = pr.to_bytes();
        assert_eq!(PackedRestarts::from_bytes(&bytes).unwrap(), pr);
        for cut in 0..bytes.len() {
            assert!(
                PackedRestarts::from_bytes(&bytes[..cut]).is_err(),
                "restart truncation accepted at sh={sh} cut={cut}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(PackedRestarts::from_bytes(&trailing).is_err());
    }

    // Non-zero reserved bytes and an out-of-range stride shift are rejected.
    let pr = PackedRestarts::build(&core, n, 4).unwrap();
    let mut bad = pr.to_bytes();
    bad[0] = 64;
    assert!(PackedRestarts::from_bytes(&bad).is_err());
    let mut bad = pr.to_bytes();
    bad[3] = 1;
    assert!(PackedRestarts::from_bytes(&bad).is_err());

    let mut bad = ph.to_bytes();
    bad[12] = 1;
    assert!(PackedHistory::from_bytes(&bad).is_err());
}

#[test]
fn state_from_hot_reconstructs_the_state_when_the_surface_covers_it() {
    for src in sample_sources() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let log = state_log(&core, n).unwrap();
        // The authoritative manifested output surface.
        let hot = NL2.decode(&core, n).unwrap();

        for i in 0..=n {
            assert_eq!(
                state_from_hot(&hot, i),
                Some(log[i as usize] as u16),
                "hot reconstruction mismatch n={n} i={i}"
            );
        }
        // Coverage: `x_i` needs positions `i-2, i-1`.
        assert_eq!(state_from_hot(&hot, n + 1), None, "n={n}");
    }

    // Genesis is always zero; `x_1` needs only the first symbol.
    assert_eq!(state_from_hot(&[], 0), Some(0));
    assert_eq!(state_from_hot(&[], 1), None);
    assert_eq!(state_from_hot(&[0xAB], 1), Some(0x00AB));
    assert_eq!(state_from_hot(&[0xAB], 2), None);
    assert_eq!(state_from_hot(&[0x12, 0x34], 2), Some(0x1234));
}

#[test]
fn machine1_structures_are_unaffected_by_packing() {
    let src = lcg(0x2222_3333_4444_5555, 700);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;

    let history = History::build(&core, n, 7).unwrap();
    for &(k, sh) in &[(16u32, 4u8), (64, 6), (256, 8)] {
        let cps = CheckpointSet::build(&core, n, k).unwrap();
        let pr = PackedRestarts::build(&core, n, sh).unwrap();
        for i in 0..=n {
            let packed = pr.snapshot_at(&core, i).unwrap();
            assert_eq!(
                cps.restore(&core, i).unwrap(),
                packed,
                "checkpoint/restart disagreement k={k} i={i}"
            );
            assert_eq!(history.snapshot_at(i), Some(packed));
        }
    }
}

#[test]
fn build_rejects_invalid_stride_shift() {
    let core = NL2.encode(b"abc").unwrap();
    for sh in [64u8, 100, 255] {
        assert!(PackedRestarts::build(&core, 3, sh).is_err());
    }
}

#[test]
fn empty_and_single_position_objects() {
    // Empty object: one genesis restart and one genesis history state.
    let pr = PackedRestarts::build(&[], 0, 5).unwrap();
    assert_eq!(pr.states, vec![0u16]);
    assert_eq!(pr.snapshot_at(&[], 0).unwrap(), Snapshot::new(0, 0));
    assert!(pr.snapshot_at(&[], 1).is_err());

    let ph = PackedHistory::build(&[], 0).unwrap();
    assert_eq!(ph.states, vec![0u16]);
    assert_eq!(ph.physical_bytes(), 2);
    assert_eq!(PackedHistory::from_bytes(&ph.to_bytes()).unwrap(), ph);
}
