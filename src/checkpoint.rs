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
use crate::state::{Snapshot, MAX_LANES};

/// Checkpoint intervals supported by the frozen contract, densest last.
pub const SUPPORTED_INTERVALS: [u32; 8] = [65536, 4096, 1024, 256, 64, 16, 4, 1];

/// Fixed size of the checkpoint wire header: `u32` interval, `u64` count.
const HEADER_LEN: usize = 4 + 8;

/// Magic marking the versioned, compact checkpoint container written by this
/// build (`CheckpointSet::to_bytes`). Legacy files have no magic and are read by
/// the backward-compatible path.
pub const CKPT_MAGIC: [u8; 8] = *b"EKBCKPT\0";
/// Version of the compact checkpoint container.
pub const CKPT_VERSION: u8 = 2;
/// Fixed header length of the compact container: magic(8) ++ version(1) ++
/// format(1) ++ interval(`u32`) ++ count(`u64`).
pub const CKPT_FIXED_LEN: usize = 8 + 1 + 1 + 4 + 8;
/// Largest header any checkpoint container needs (legacy or compact).
pub const CKPT_HEADER_MAX: usize = CKPT_FIXED_LEN;
/// Compact format code: 2-byte packed machine-1 states (cursor derived).
const CKPT_FORMAT_PACKED: u8 = 1;
/// Compact format code: 12-byte complete `(state, cursor)` restarts.
const CKPT_FORMAT_FULL: u8 = 2;
/// Compact format code: complete wide `Snapshot::WIDE_LEN` restarts (interleaved
/// machine 4 with a multi-word lane state).
const CKPT_FORMAT_WIDE: u8 = 3;

/// Decoded wire layout of a checkpoint payload header, for the bounded file
/// reader (which must not materialise the whole set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointLayout {
    /// Checkpoint interval `K`.
    pub interval: u32,
    /// Number of restart records.
    pub count: u64,
    /// Header length in bytes (`12` legacy, [`CKPT_FIXED_LEN`] compact).
    pub header_len: usize,
    /// Bytes per restart record (2 packed, else 12, or `Snapshot::WIDE_LEN`).
    pub entry_len: usize,
    /// Whether restart records are 2-byte packed states.
    pub packed: bool,
    /// Whether restart records are complete wide (`Snapshot::WIDE_LEN`) states.
    pub wide: bool,
}

impl CheckpointLayout {
    /// Total payload length (`header_len + count * entry_len`), or `None` on
    /// overflow.
    pub fn payload_len(&self) -> Option<u64> {
        self.count
            .checked_mul(self.entry_len as u64)
            .and_then(|payload| payload.checked_add(self.header_len as u64))
    }
}

/// Parse and validate the header of a checkpoint-set payload.
///
/// Accepts both the compact container written by this build and the legacy
/// `interval u32 ++ count u64` layout, so old files still read. `bytes` must hold
/// at least the header (12 legacy bytes, or [`CKPT_FIXED_LEN`] compact bytes).
/// Fails closed on an unsupported interval, a zero count or an overflowing span.
pub fn parse_checkpoint_layout(bytes: &[u8]) -> Result<CheckpointLayout> {
    if bytes.len() >= CKPT_MAGIC.len() && bytes[..8] == CKPT_MAGIC {
        if bytes.len() < CKPT_FIXED_LEN {
            return Err(EkbloomError::Truncated {
                needed: CKPT_FIXED_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        if bytes[8] != CKPT_VERSION {
            return Err(EkbloomError::BadSection {
                reason: format!("unsupported checkpoint version {}", bytes[8]),
            });
        }
        let (packed, wide) = match bytes[9] {
            CKPT_FORMAT_PACKED => (true, false),
            CKPT_FORMAT_FULL => (false, false),
            CKPT_FORMAT_WIDE => (false, true),
            other => {
                return Err(EkbloomError::BadSection {
                    reason: format!("unknown checkpoint format {other}"),
                })
            }
        };
        let interval = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
        if !CheckpointSet::supported_interval(interval) {
            return Err(EkbloomError::BadInterval { interval });
        }
        let count = u64::from_le_bytes([
            bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
        ]);
        if count == 0 {
            return Err(EkbloomError::BadSection {
                reason: "checkpoint set count is zero".to_string(),
            });
        }
        let k = u64::from(interval);
        (count - 1).checked_mul(k).ok_or(EkbloomError::Overflow {
            what: "checkpoint span",
        })?;
        Ok(CheckpointLayout {
            interval,
            count,
            header_len: CKPT_FIXED_LEN,
            entry_len: if packed {
                2
            } else if wide {
                Snapshot::WIDE_LEN
            } else {
                12
            },
            packed,
            wide,
        })
    } else {
        if bytes.len() < HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let interval = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if !CheckpointSet::supported_interval(interval) {
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
        (count - 1).checked_mul(k).ok_or(EkbloomError::Overflow {
            what: "checkpoint span",
        })?;
        Ok(CheckpointLayout {
            interval,
            count,
            header_len: HEADER_LEN,
            entry_len: 12,
            packed: false,
            wide: false,
        })
    }
}

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

    /// Whether this set is stored in the compact 2-byte packed form: every restart
    /// state fits in `u16`, the interval is a power of two, and every cursor equals
    /// the implicit logical position `j*K` (a machine-1 checkpoint).
    pub fn packed(&self) -> bool {
        self.interval.is_power_of_two()
            && self.resume.iter().enumerate().all(|(j, s)| {
                s.state <= u32::from(u16::MAX)
                    && s.lane == 0
                    && s.lanes == [0u32; MAX_LANES]
                    && (j as u64)
                        .checked_mul(u64::from(self.interval))
                        .is_some_and(|pos| s.cursor == pos)
            })
    }

    /// Whether the set carries any multi-word (interleaved) resume state and must
    /// therefore be stored in the wide container.
    pub fn wide(&self) -> bool {
        self.resume
            .iter()
            .any(|s| s.lane != 0 || s.lanes.iter().any(|&x| x != 0))
    }

    /// Bytes per restart record in the compact container: `2` when [`Self::packed`],
    /// `Snapshot::WIDE_LEN` when [`Self::wide`], otherwise `12`.
    #[inline]
    pub fn entry_bytes(&self) -> u64 {
        if self.packed() {
            2
        } else if self.wide() {
            Snapshot::WIDE_LEN as u64
        } else {
            12
        }
    }

    /// Packed physical cost of the resume points: `count * entry_bytes()`.
    ///
    /// A machine-1 restart set is cursor==position, so it manifests at 2 bytes per
    /// restart instead of 12 (see `docs/BLOCKS.md` §"packed checkpoint format").
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.count.saturating_mul(self.entry_bytes())
    }

    /// Serialize into the compact, versioned container.
    ///
    /// The container is `magic(8) ++ version(1) ++ format(1) ++ interval(u32) ++
    /// count(u64)`, followed by `count` restart records: 2-byte machine-1 states
    /// when [`Self::packed`] (cursor derived as `j*K`), else 12-byte complete
    /// restarts. Old (legacy, magic-less) files remain readable via
    /// [`CheckpointSet::from_bytes_with`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.resume.len() as u64;
        let packed = self.packed();
        let wide = !packed && self.wide();
        let entry_len = if packed {
            2
        } else if wide {
            Snapshot::WIDE_LEN
        } else {
            12
        };
        let capacity = CKPT_FIXED_LEN.saturating_add(self.resume.len().saturating_mul(entry_len));
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(&CKPT_MAGIC);
        out.push(CKPT_VERSION);
        out.push(if packed {
            CKPT_FORMAT_PACKED
        } else if wide {
            CKPT_FORMAT_WIDE
        } else {
            CKPT_FORMAT_FULL
        });
        out.extend_from_slice(&self.interval.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if packed {
            for s in &self.resume {
                out.extend_from_slice(&(s.state as u16).to_le_bytes());
            }
        } else if wide {
            for s in &self.resume {
                out.extend_from_slice(&s.pack_wide());
            }
        } else {
            for s in &self.resume {
                out.extend_from_slice(&s.pack());
            }
        }
        out
    }

    /// Parse a checkpoint payload: the compact container written by this build, or
    /// the legacy `interval u32 ++ count u64 ++ 12-byte snapshots` layout.
    ///
    /// Validates the interval, a non-zero count, the arithmetic span, truncation,
    /// trailing bytes and (for machine 1) each resume point's implicit cursor.
    pub fn from_bytes(bytes: &[u8]) -> Result<CheckpointSet> {
        Self::from_bytes_with(&NL2, bytes)
    }

    /// Machine-general [`CheckpointSet::from_bytes`].
    ///
    /// The `cursor == j*K` invariant is only enforced for machine 1; a
    /// cursor-divergent machine stores the true backing cursor verbatim.
    pub fn from_bytes_with(machine: &dyn OrMachine, bytes: &[u8]) -> Result<CheckpointSet> {
        let layout = parse_checkpoint_layout(bytes)?;
        let expected = layout.payload_len().ok_or(EkbloomError::Overflow {
            what: "checkpoint byte length",
        })?;
        if (bytes.len() as u64) < expected {
            return Err(EkbloomError::Truncated {
                needed: expected,
                available: bytes.len() as u64,
            });
        }
        if (bytes.len() as u64) > expected {
            return Err(EkbloomError::BadSection {
                reason: "checkpoint set has trailing bytes".to_string(),
            });
        }

        let k = u64::from(layout.interval);
        let enforce_cursor = machine.machine_id() == MACHINE_NL2 && !layout.packed;
        let count_usize = usize::try_from(layout.count).map_err(|_| EkbloomError::Overflow {
            what: "checkpoint count",
        })?;
        let mut resume = Vec::with_capacity(count_usize);
        let mut off = layout.header_len;
        for j in 0..layout.count {
            if layout.packed {
                let state = u16::from_le_bytes([bytes[off], bytes[off + 1]]) as u32;
                let cursor = j.checked_mul(k).ok_or(EkbloomError::Overflow {
                    what: "checkpoint cursor",
                })?;
                resume.push(Snapshot::new(state, cursor));
                off += 2;
            } else if layout.wide {
                let s = Snapshot::unpack_wide(&bytes[off..off + Snapshot::WIDE_LEN])?;
                resume.push(s);
                off += Snapshot::WIDE_LEN;
            } else {
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
        }

        Ok(CheckpointSet {
            interval: layout.interval,
            count: layout.count,
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
                assert!(cps.packed());
                assert_eq!(cps.entry_bytes(), 2);
                assert_eq!(cps.physical_bytes(), cps.count * 2);

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
        assert_eq!(&bytes[..8], &CKPT_MAGIC);
        assert_eq!(bytes[8], CKPT_VERSION);
        for cut in 0..bytes.len() {
            assert!(
                CheckpointSet::from_bytes(&bytes[..cut]).is_err(),
                "expected error at cut {cut}"
            );
        }

        // Zero count.
        let mut b = bytes.clone();
        b[14..22].copy_from_slice(&0u64.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::BadSection { .. })
        ));

        // Span overflow.
        let mut b = bytes.clone();
        b[14..22].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::Overflow { .. })
        ));

        // Unsupported interval.
        let mut b = bytes.clone();
        b[10..14].copy_from_slice(&7u32.to_le_bytes());
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::BadInterval { .. })
        ));

        // Unknown version and format are rejected.
        let mut b = bytes.clone();
        b[8] = 99;
        assert!(CheckpointSet::from_bytes(&b).is_err());
        let mut b = bytes.clone();
        b[9] = 9;
        assert!(CheckpointSet::from_bytes(&b).is_err());

        // Trailing bytes are rejected.
        let mut b = bytes.clone();
        b.push(0);
        assert!(matches!(
            CheckpointSet::from_bytes(&b),
            Err(EkbloomError::BadSection { .. })
        ));
    }

    /// The legacy magic-less layout still reads (backward compatibility), and its
    /// resume points match the compact re-serialization.
    #[test]
    fn legacy_layout_is_backward_compatible() {
        let core = NL2.encode(&lcg(0x1122_3344, 260)).unwrap();
        let cps = CheckpointSet::build(&core, 260, 16).unwrap();

        // Hand-build the legacy layout: interval u32, count u64, 12-byte snapshots.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&cps.interval.to_le_bytes());
        legacy.extend_from_slice(&(cps.resume.len() as u64).to_le_bytes());
        for s in &cps.resume {
            legacy.extend_from_slice(&s.pack());
        }
        let back = CheckpointSet::from_bytes(&legacy).unwrap();
        assert_eq!(back, cps);

        // The compact form written by this build is smaller (2 vs 12 bytes/entry).
        assert!(cps.to_bytes().len() < legacy.len());
    }

    /// A cursor-divergent (machine-2) checkpoint uses the full 12-byte restarts and
    /// round-trips with its true backing cursor.
    #[test]
    fn machine2_checkpoints_keep_full_cursors() {
        use crate::machine_rans::RANS0;
        let src: Vec<u8> = (0..1024u32).map(|i| (i * 7 % 5) as u8).collect();
        let core = RANS0.encode(&src).unwrap();
        let n = src.len() as u64;
        let cps = CheckpointSet::build_with(&RANS0, &core, n, 64).unwrap();
        assert!(!cps.packed());
        assert_eq!(cps.entry_bytes(), 12);
        assert_eq!(cps.physical_bytes(), cps.count * 12);
        let bytes = cps.to_bytes();
        assert_eq!(bytes[9], CKPT_FORMAT_FULL);
        let back = CheckpointSet::from_bytes_with(&RANS0, &bytes).unwrap();
        assert_eq!(back, cps);
        for i in (0..=n).step_by(97) {
            let want = cps.restore_with(&RANS0, &core, i).unwrap();
            assert_eq!(back.restore_with(&RANS0, &core, i).unwrap(), want);
        }
    }
}
