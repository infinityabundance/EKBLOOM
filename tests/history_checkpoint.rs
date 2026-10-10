//! End-to-end integration tests for the `history` and `checkpoint` modules.
//!
//! These exercise only the public API (`ekbloom::{history, checkpoint, machine,
//! state}`) and assert agreement with the authoritative machine decode.

use ekbloom::checkpoint::{CheckpointSet, SUPPORTED_INTERVALS};
use ekbloom::history::History;
use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::state::Snapshot;

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

fn cases() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        vec![0x42],
        (0u8..=255).collect(),
        vec![9u8; 100],
        (0..250u32)
            .map(|i| if i % 3 == 0 { 0x01 } else { 0xFE })
            .collect(),
        lcg(0x0BAD_F00D_CAFE_BEEF, 257),
        lcg(0x1122_3344_5566_7788, 1024),
    ]
}

#[test]
fn history_positions_and_decode_agree() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let h = History::build(&core, n, 7).unwrap();
        let decoded = NL2.decode(&core, n).unwrap();

        assert_eq!(h.generation, 7);
        assert_eq!(h.logical_len, n);
        assert_eq!(h.core_len, core.len() as u64);
        assert_eq!(h.states.len() as u64, n + 1);
        assert_eq!(h.physical_bytes(), 4 * (n + 1));

        let logs = state_log(&core, n).unwrap();
        for i in 0..=n {
            assert_eq!(h.state_at(i), Some(logs[i as usize]));
            assert_eq!(h.snapshot_at(i), Some(Snapshot::new(logs[i as usize], i)));
        }

        // Advancing from Omega_i must reveal the authoritative symbol s_i.
        for i in 0..n {
            let mut omega = h.snapshot_at(i).unwrap();
            let s = NL2.reveal(&core, &mut omega).unwrap();
            assert_eq!(s, decoded[i as usize]);
            assert_eq!(omega, Snapshot::new(logs[i as usize + 1], i + 1));
        }
    }
}

#[test]
fn checkpoint_restore_agrees_with_state_log_all_intervals() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let logs = state_log(&core, n).unwrap();

        for &k in SUPPORTED_INTERVALS.iter() {
            let cps = CheckpointSet::build(&core, n, k).unwrap();
            let kk = u64::from(k);
            assert_eq!(cps.max_replay_steps(), k - 1);
            assert_eq!(cps.count, n / kk + 1);

            for i in 0..=n {
                let want = Snapshot::new(logs[i as usize], i);
                assert_eq!(cps.restore(&core, i).unwrap(), want);

                // Count actual replay steps and enforce the K-1 bound.
                let mut omega = cps.restart_point(i).unwrap();
                let mut steps: u32 = 0;
                while omega.cursor < i {
                    NL2.reveal(&core, &mut omega).unwrap();
                    steps += 1;
                    assert!(steps <= cps.max_replay_steps());
                }
                assert_eq!(omega, want);
                assert_eq!(u64::from(steps), i % kk);
            }
        }
    }
}

#[test]
fn history_roundtrip_and_truncation() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let h = History::build(&core, src.len() as u64, 3).unwrap();
        let bytes = h.to_bytes();
        assert_eq!(History::from_bytes(&bytes).unwrap(), h);

        for cut in 0..bytes.len() {
            assert!(History::from_bytes(&bytes[..cut]).is_err());
        }
    }
}

#[test]
fn checkpoint_roundtrip_and_truncation() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        for &k in SUPPORTED_INTERVALS.iter() {
            let cps = CheckpointSet::build(&core, n, k).unwrap();
            let bytes = cps.to_bytes();
            assert_eq!(CheckpointSet::from_bytes(&bytes).unwrap(), cps);
            for cut in 0..bytes.len() {
                assert!(CheckpointSet::from_bytes(&bytes[..cut]).is_err());
            }
        }
    }
}

#[test]
fn history_verify_against_detects_tampering() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let h = History::build(&core, src.len() as u64, 0).unwrap();
        h.verify_against(&core).unwrap();

        if let Some(first) = core.first().copied() {
            let mut bad = core.clone();
            bad[0] = first ^ 0x5A;
            assert!(h.verify_against(&bad).is_err());
        }
    }
}

#[test]
fn history_and_checkpoint_agree() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let h = History::build(&core, n, 0).unwrap();
        let cps = CheckpointSet::build(&core, n, 16).unwrap();

        for i in 0..=n {
            assert_eq!(h.snapshot_at(i).unwrap(), cps.restore(&core, i).unwrap());
            if i % 16 == 0 {
                assert_eq!(h.snapshot_at(i).unwrap(), cps.restart_point(i).unwrap());
            }
        }
    }
}
