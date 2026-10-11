//! Sparse checkpoint sets with mechanically bounded restart replay.
//!
//! At interval `K`, `C_K = { Omega_{jK} : 0 <= j <= floor(n / K) }`. Each
//! checkpoint stores the complete resumable state (packed as
//! [`Snapshot`]: `u32` state ++ `u64` cursor = 12 bytes); the logical position
//! is implicit as `j*K`. Restarting at position `i` loads `Omega_{(i/K)*K}` and
//! advances at most `K-1` steps, so [`CheckpointSet::max_replay_steps`] is
//! `K-1` and [`CheckpointSet::restore`] never performs more than that.
//!
//! For a machine whose cursor equals the logical position (`OrANS-NL2`,
//! `machine_id = 1`) the cursor equals the logical position, so a checkpoint at
//! slot `j` carries `cursor = j*K`. A cursor-divergent machine (e.g. machine 2)
//! instead stores the true backing cursor; the logical position remains implicit
//! as `j*K`, and the cursor-equality invariant is only enforced for machine 1.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_NL2, NL2};
use crate::state::Snapshot;

/// Checkpoint intervals supported by the frozen contract, densest last.
pub const SUPPORTED_INTERVALS: [u32; 8] = [65536, 4096, 1024, 256, 64, 16, 4, 1];

/// Fixed size of the checkpoint wire header: `u32` interval, `u64` count.
const HEADER_LEN: usize = 4 + 8;

/// A set of complete restart states at a fixed interval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckpointSet {
    /// Checkpoint interval `K`.
    pub interval: u32,
    /// Number of resume points (`floor(n / K) + 1`).
    pub count: u64,
    /// `resume[j] = Omega_{jK}`.
    pub resume: Vec<Snapshot>,
}

impl CheckpointSet {
    /// Build a checkpoint set for `core` producing `logical_len` symbols at
    /// `interval`, which must be a member of [`SUPPORTED_INTERVALS`].
    ///
    /// Rejects an unsupported interval with [`EkbloomError::BadInterval`].
    pub fn build(core: &[u8], logical_len: u64, interval: u32) -> Result<CheckpointSet> {
        Self::build_with(&NL2, core, logical_len, interval)
    }

    /// Machine-general [`CheckpointSet::build`].
    pub fn build_with(
        machine: &dyn OrMachine,
        core: &[u8],
        logical_len: u64,
        interval: u32,
    ) -> Result<CheckpointSet> {
        if !Self::supported_interval(interval) {
            return Err(EkbloomError::BadInterval { interval });
        }
        machine.check_core(core, logical_len)?;
        let ctx = crate::machine::prepare(machine, core)?;

        let k = u64::from(interval);
        let mut resume = Vec::new();
        let mut omega = machine.init(core)?;
        resume.push(omega);

        let mut i: u64 = 0;
        while i < logical_len {
            ctx.reveal(core, &mut omega)?;
            i += 1;
            if i.is_multiple_of(k) {
                resume.push(omega);
            }
        }

        Ok(CheckpointSet {
            interval,
            count: resume.len() as u64,
            resume,
        })
    }

    /// Whether `interval` is one of [`SUPPORTED_INTERVALS`].
    #[inline]
    pub fn supported_interval(interval: u32) -> bool {
        SUPPORTED_INTERVALS.contains(&interval)
    }

    /// The checkpoint `Omega_{(i/K)*K}` covering logical position `i`, if the
    /// set has a resume point for that interval slot.
    pub fn restart_point(&self, i: u64) -> Option<Snapshot> {
        let k = u64::from(self.interval);
        let j = i / k;
        let idx = usize::try_from(j).ok()?;
        if idx as u64 >= self.count {
            return None;
        }
        self.resume.get(idx).copied()
    }

    /// Restore `Omega_i` exactly by advancing at most `K-1` steps from the
    /// covering checkpoint. The number of steps taken is exactly `i % K`.
    pub fn restore(&self, core: &[u8], i: u64) -> Result<Snapshot> {
        self.restore_with(&NL2, core, i)
    }

    /// Machine-general [`CheckpointSet::restore`].
    pub fn restore_with(&self, machine: &dyn OrMachine, core: &[u8], i: u64) -> Result<Snapshot> {
        let k = u64::from(self.interval);
        let j = i / k;
        let idx = usize::try_from(j).map_err(|_| EkbloomError::InvalidArgument {
            what: format!("checkpoint position {i} out of range"),
        })?;
        let mut omega = *self
            .resume
            .get(idx)
            .ok_or_else(|| EkbloomError::InvalidArgument {
                what: format!("checkpoint position {i} out of range"),
            })?;

        // For a cursor==position machine the covering checkpoint must sit at the
        // implicit position `j*K`; other machines carry an independent cursor.
        if machine.machine_id() == MACHINE_NL2 {
            let expected_cursor = j.checked_mul(k).ok_or(EkbloomError::Overflow {
                what: "checkpoint cursor",
            })?;
            if omega.cursor != expected_cursor {
                return Err(EkbloomError::BadSection {
                    reason: format!(
                        "checkpoint cursor {} does not match implicit position {expected_cursor}",
                        omega.cursor
                    ),
                });
            }
        }

        let steps = i % k;
        if steps > 0 {
            let ctx = crate::machine::prepare(machine, core)?;
            for _ in 0..steps {
                ctx.reveal(core, &mut omega)?;
            }
        }
        Ok(omega)
    }

    /// Maximum number of replay steps from any checkpoint: `K - 1`.
    #[inline]
    pub fn max_replay_steps(&self) -> u32 {
        self.interval.saturating_sub(1)
    }

    /// Packed physical cost of the resume points: `count * 12`.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.count.saturating_mul(Snapshot::PACKED_LEN as u64)
    }

    /// Serialize: `u32` interval, `u64` count, then `count` packed
    /// [`Snapshot`]s ([`Snapshot::pack`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.resume.len() as u64;
        let capacity =
            HEADER_LEN.saturating_add(self.resume.len().saturating_mul(Snapshot::PACKED_LEN));
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(&self.interval.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        for s in &self.resume {
            out.extend_from_slice(&s.pack());
        }
        out
    }

    /// Parse the exact wire layout produced by [`CheckpointSet::to_bytes`].
    ///
    /// Validates the interval, a non-zero count, the arithmetic span, and each
    /// resume point's implicit cursor. Rejects truncation with
    /// [`EkbloomError::Truncated`] and inconsistency with
    /// [`EkbloomError::BadSection`].
    pub fn from_bytes(bytes: &[u8]) -> Result<CheckpointSet> {
        Self::from_bytes_with(&NL2, bytes)
    }

    /// Machine-general [`CheckpointSet::from_bytes`].
    ///
    /// The `cursor == j*K` invariant is only enforced for machine 1; a
    /// cursor-divergent machine stores the true backing cursor verbatim.
    pub fn from_bytes_with(machine: &dyn OrMachine, bytes: &[u8]) -> Result<CheckpointSet> {
        if bytes.len() < HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let interval = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if !Self::supported_interval(interval) {
            return Err(EkbloomError::BadInterval { interval });
        }
        let count = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        if count == 0 {
            return Err(EkbloomError::BadSection {
                reason: "checkpoint set count is zero".to_string(),
            });
        }

        let k = u64::from(interval);
        // The span of `count` checkpoints must not overflow.
        (count - 1).checked_mul(k).ok_or(EkbloomError::Overflow {
            what: "checkpoint span",
        })?;
        let expected = count
            .checked_mul(Snapshot::PACKED_LEN as u64)
            .and_then(|payload| payload.checked_add(HEADER_LEN as u64))
            .ok_or(EkbloomError::Overflow {
                what: "checkpoint byte length",
            })?;
        if (bytes.len() as u64) < expected {
            return Err(EkbloomError::Truncated {
                needed: expected,
                available: bytes.len() as u64,
            });
        }

        let count_usize = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
            what: "checkpoint count",
        })?;
        let enforce_cursor = machine.machine_id() == MACHINE_NL2;
        let mut resume = Vec::with_capacity(count_usize);
        let mut off = HEADER_LEN;
        for j in 0..count {
            let s = Snapshot::unpack(&bytes[off..off + Snapshot::PACKED_LEN])?;
            if enforce_cursor {
                let expected_cursor = j.checked_mul(k).ok_or(EkbloomError::Overflow {
                    what: "checkpoint cursor",
                })?;
                if s.cursor != expected_cursor {
                    return Err(EkbloomError::BadSection {
                        reason: format!(
                            "checkpoint {j} cursor {} does not match implicit position {expected_cursor}",
                            s.cursor
                        ),
                    });
                }
            }
            resume.push(s);
            off += Snapshot::PACKED_LEN;
        }

        Ok(CheckpointSet {
            interval,
            count,
            resume,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::state_log;

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

    #[test]
    fn supported_intervals() {
        for k in SUPPORTED_INTERVALS {
            assert!(CheckpointSet::supported_interval(k));
        }
        let core = NL2.encode(b"abc").unwrap();
        for k in [0u32, 2, 3, 5, 7, 8, 15, 100, 65535, 65537, u32::MAX] {
            assert!(!CheckpointSet::supported_interval(k));
            assert!(matches!(
                CheckpointSet::build(&core, 3, k),
                Err(EkbloomError::BadInterval { .. })
            ));
        }
    }

    #[test]
    fn restore_matches_state_log_all_intervals() {
        for src in sample_sources() {
            let core = NL2.encode(&src).unwrap();
            let n = src.len() as u64;
            let logs = state_log(&core, n).unwrap();
            let decoded = NL2.decode(&core, n).unwrap();

            for &k in SUPPORTED_INTERVALS.iter() {
                let cps = CheckpointSet::build(&core, n, k).unwrap();
                let kk = u64::from(k);
                assert_eq!(cps.interval, k);
                assert_eq!(cps.max_replay_steps(), k - 1);
                assert_eq!(cps.count, n / kk + 1);
                assert_eq!(cps.physical_bytes(), cps.count * 12);

                for i in 0..=n {
                    let expect = Snapshot::new(logs[i as usize], i);
                    assert_eq!(
                        cps.restore(&core, i).unwrap(),
                        expect,
                        "restore mismatch: n={n} k={k} i={i}"
                    );

                    // Instrument the actual replay step count.
                    let mut omega = cps.restart_point(i).unwrap();
                    let mut steps: u32 = 0;
                    while omega.cursor < i {
                        NL2.reveal(&core, &mut omega).unwrap();
                        steps += 1;
                        assert!(
                            steps <= cps.max_replay_steps(),
                            "exceeded K-1 steps: n={n} k={k} i={i}"
                        );
                    }
                    assert_eq!(omega, expect);
                    assert_eq!(u64::from(steps), i % kk);

                    // The restored state must reveal the authoritative symbol.
                    if i < n {
                        let mut probe = cps.restore(&core, i).unwrap();
                        let s = NL2.reveal(&core, &mut probe).unwrap();
                        assert_eq!(s, decoded[i as usize], "symbol mismatch at i={i}");
                    }
                }

                // Out-of-coverage positions have no restart point.
                assert_eq!(cps.restart_point(cps.count * kk), None);
            }
        }
    }

    #[test]
    fn roundtrip_bytes() {
        for src in sample_sources() {
            let core = NL2.encode(&src).unwrap();
            let n = src.len() as u64;
            for &k in SUPPORTED_INTERVALS.iter() {
                let cps = CheckpointSet::build(&core, n, k).unwrap();
                let bytes = cps.to_bytes();
                let back = CheckpointSet::from_bytes(&bytes).unwrap();
                assert_eq!(back, cps);
                assert_eq!(
                    back.to_bytes(),
                    bytes,
                    "not an exact inverse (k={k}, n={n})"
                );
            }
        }
    }

    #[test]
    fn truncation_and_malformed_rejected() {
        let core = NL2.encode(&lcg(5, 300)).unwrap();
        let cps = CheckpointSet::build(&core, 300, 16).unwrap();
        let bytes = cps.to_bytes();
        for cut in 0..bytes.len() {
            assert!(
                CheckpointSet::from_bytes(&bytes[..cut]).is_err(),
                "expected error at cut {cut}"
            );
        }

        let mut b = bytes.clone();
        b[4..12].copy_from_slice(&0u64.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::BadSection { .. })
        ));

        let mut b = bytes.clone();
        b[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::Overflow { .. })
        ));

        let mut b = bytes.clone();
        b[0..4].copy_from_slice(&7u32.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::BadInterval { .. })
        ));

        // First resume point must have cursor 0; corrupt it.
        let mut b = bytes.clone();
        b[16..24].copy_from_slice(&999u64.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::BadSection { .. })
        ));
    }
}
