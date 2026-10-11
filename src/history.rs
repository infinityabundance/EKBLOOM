//! Full non-withdrawal state-projection history.
//!
//! A [`History`] records the complete per-position state projection
//! `L_full = [x_0, x_1, ..., x_n]` for a visible `core`, together with the
//! identifying metadata (`logical_len`, `core_len`, `core_hash`, `generation`)
//! needed to verify it against the authoritative machine.
//!
//! # Generation order vs. logical position
//!
//! For the sole v0.3 machine `OrANS-NL2` (`machine_id = 1`) the backing cursor
//! always satisfies `p_i = i`, so *generation order coincides with logical
//! order*. Both the generation-order history and the logical-position
//! projection are therefore the same sequence, and [`History`] stores a single
//! `states` vector (`states[i] = x_i`, one `u32` per logical position, cost
//! `4 * N` bytes). The cursor field of [`Snapshot`] is nonetheless retained and
//! the API kept general enough that a future machine whose cursor diverges from
//! the logical position could be represented without a wire-format change.
//!
//! Non-withdrawal: history is append-only. [`History::extend`] appends states
//! for additional positions without ever rewriting earlier committed states.

use crate::error::{EkbloomError, Result};
use crate::machine::{state_log_with, OrMachine, NL2};
use crate::state::Snapshot;

/// Fixed wire-header size: `u64` generation, `u64` logical_len, `u64` core_len,
/// `[u8; 32]` core_hash, `u64` count.
const HEADER_LEN: usize = 8 + 8 + 8 + 32 + 8;

/// A complete, non-withdrawal per-position state projection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Commit generation this history belongs to.
    pub generation: u64,
    /// Number of logical positions (revealed symbols).
    pub logical_len: u64,
    /// Length in bytes of the visible `core` this projection was built from.
    pub core_len: u64,
    /// BLAKE3 hash of the visible `core`.
    pub core_hash: [u8; 32],
    /// `states[i] = x_i`, the machine state before revealing symbol `i`.
    /// Contains `logical_len + 1` entries (`x_0 ..= x_n`).
    pub states: Vec<u32>,
}

impl History {
    /// Build the full state projection for `core` producing `logical_len`
    /// symbols, tagged with `generation`, using the authoritative machine 1.
    pub fn build(core: &[u8], logical_len: u64, generation: u64) -> Result<History> {
        Self::build_with(&NL2, core, logical_len, generation)
    }

    /// Machine-general [`History::build`].
    pub fn build_with(
        machine: &dyn OrMachine,
        core: &[u8],
        logical_len: u64,
        generation: u64,
    ) -> Result<History> {
        let states = state_log_with(machine, core, logical_len)?;
        Ok(History {
            generation,
            logical_len,
            core_len: core.len() as u64,
            core_hash: *blake3::hash(core).as_bytes(),
            states,
        })
    }

    /// State `x_i` at logical position `i`, or `None` if out of range.
    #[inline]
    pub fn state_at(&self, i: u64) -> Option<u32> {
        usize::try_from(i)
            .ok()
            .and_then(|idx| self.states.get(idx).copied())
    }

    /// Complete resumable state `Omega_i` at logical position `i`.
    ///
    /// For `machine_id = 1` the cursor equals the logical position, so the
    /// returned snapshot is `Snapshot::new(x_i, i)`.
    #[inline]
    pub fn snapshot_at(&self, i: u64) -> Option<Snapshot> {
        self.state_at(i).map(|state| Snapshot::new(state, i))
    }

    /// Recompute the projection from scratch and compare every position.
    ///
    /// Also confirms `core` has the recorded length and hash, so a tampered
    /// core is rejected even before the (symbol-equivalent) state comparison.
    pub fn verify_against(&self, core: &[u8]) -> Result<()> {
        self.verify_against_with(&NL2, core)
    }

    /// Machine-general [`History::verify_against`].
    pub fn verify_against_with(&self, machine: &dyn OrMachine, core: &[u8]) -> Result<()> {
        if core.len() as u64 != self.core_len {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "core length {} does not match history core_len {}",
                    core.len(),
                    self.core_len
                ),
            });
        }
        if *blake3::hash(core).as_bytes() != self.core_hash {
            return Err(EkbloomError::BadSection {
                reason: "core hash mismatch".to_string(),
            });
        }
        let expected = state_log_with(machine, core, self.logical_len)?;
        if expected.len() != self.states.len() {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "state log length {} does not match stored {}",
                    expected.len(),
                    self.states.len()
                ),
            });
        }
        for (i, (recomputed, stored)) in expected.iter().zip(self.states.iter()).enumerate() {
            if recomputed != stored {
                return Err(EkbloomError::BadSection {
                    reason: format!(
                        "state log mismatch at position {i}: recomputed {recomputed}, stored {stored}"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Packed physical cost of the `u32` state projection: `4 * states.len()`.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        (self.states.len() as u64).saturating_mul(4)
    }

    /// Serialize to the exact wire layout:
    /// `u64` generation, `u64` logical_len, `u64` core_len, `[u8; 32]` core_hash,
    /// `u64` count, then `count` little-endian `u32` states.
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.states.len();
        let capacity = HEADER_LEN.saturating_add(count.saturating_mul(4));
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.logical_len.to_le_bytes());
        out.extend_from_slice(&self.core_len.to_le_bytes());
        out.extend_from_slice(&self.core_hash);
        out.extend_from_slice(&(count as u64).to_le_bytes());
        for &x in &self.states {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out
    }

    /// Parse the exact wire layout produced by [`History::to_bytes`].
    ///
    /// Rejects truncated input with [`EkbloomError::Truncated`] and structurally
    /// inconsistent input with [`EkbloomError::BadSection`].
    pub fn from_bytes(bytes: &[u8]) -> Result<History> {
        if bytes.len() < HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let generation = read_u64(bytes, 0);
        let logical_len = read_u64(bytes, 8);
        let core_len = read_u64(bytes, 16);
        let mut core_hash = [0u8; 32];
        core_hash.copy_from_slice(&bytes[24..56]);
        let count = read_u64(bytes, 56);

        let expected = count
            .checked_mul(4)
            .and_then(|payload| payload.checked_add(HEADER_LEN as u64))
            .ok_or(EkbloomError::Overflow {
                what: "history byte length",
            })?;
        if (bytes.len() as u64) < expected {
            return Err(EkbloomError::Truncated {
                needed: expected,
                available: bytes.len() as u64,
            });
        }

        let want_count = logical_len.checked_add(1).ok_or(EkbloomError::Overflow {
            what: "history logical length",
        })?;
        if count != want_count {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "history count {count} inconsistent with logical_len {logical_len}"
                ),
            });
        }

        let count_usize = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
            what: "history count",
        })?;
        let mut states = Vec::with_capacity(count_usize);
        let mut off = HEADER_LEN;
        for _ in 0..count {
            states.push(read_u32(bytes, off));
            off += 4;
        }

        Ok(History {
            generation,
            logical_len,
            core_len,
            core_hash,
            states,
        })
    }

    /// Append states for positions `self.logical_len .. new_logical_len` without
    /// rewriting any previously committed state.
    ///
    /// The existing core prefix must be unchanged (verified against
    /// [`History::core_hash`]); `core` must be long enough for
    /// `new_logical_len` positions. A successful append advances
    /// [`History::generation`] by one. Errors with
    /// [`EkbloomError::InvalidArgument`] when `new_logical_len < self.logical_len`.
    pub fn extend(&mut self, core: &[u8], new_logical_len: u64) -> Result<()> {
        self.extend_with(&NL2, core, new_logical_len)
    }

    /// Machine-general [`History::extend`].
    pub fn extend_with(
        &mut self,
        machine: &dyn OrMachine,
        core: &[u8],
        new_logical_len: u64,
    ) -> Result<()> {
        if new_logical_len < self.logical_len {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "new_logical_len {new_logical_len} is less than existing {}",
                    self.logical_len
                ),
            });
        }
        if new_logical_len == self.logical_len {
            return Ok(());
        }
        machine.check_core(core, new_logical_len)?;

        let committed = self.logical_len;
        let committed_len = usize::try_from(committed).map_err(|_| EkbloomError::Overflow {
            what: "history logical length",
        })?;
        if self.states.len() != committed_len + 1 {
            return Err(EkbloomError::BadSection {
                reason: "history state count inconsistent with logical length".to_string(),
            });
        }
        // Non-withdrawal: the already-committed core prefix must be unchanged so
        // the appended tail continues the same projection. Recompute the prefix
        // projection and require an exact match with the committed states.
        let prefix = state_log_with(machine, &core[..committed_len], committed)?;
        if prefix != self.states {
            return Err(EkbloomError::BadSection {
                reason: "existing core prefix changed".to_string(),
            });
        }

        let last = *self.states.last().ok_or_else(|| EkbloomError::BadSection {
            reason: "empty history".to_string(),
        })?;
        let mut omega = Snapshot::new(last, committed);
        // Build the appended tail first so a failure cannot partially mutate.
        let mut tail: Vec<u32> = Vec::new();
        let mut i = committed;
        while i < new_logical_len {
            machine.reveal(core, &mut omega)?;
            tail.push(omega.state);
            i += 1;
        }

        self.states.extend_from_slice(&tail);
        self.logical_len = new_logical_len;
        self.core_len = core.len() as u64;
        self.core_hash = *blake3::hash(core).as_bytes();
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(EkbloomError::Overflow {
                what: "history generation",
            })?;
        Ok(())
    }
}

#[inline]
fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

#[inline]
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[off..off + 8]);
    u64::from_le_bytes(b)
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

    fn core_for(src: &[u8]) -> Vec<u8> {
        NL2.encode(src).unwrap()
    }

    fn sample_sources() -> Vec<Vec<u8>> {
        vec![
            Vec::new(),
            vec![0x00],
            vec![0xFF],
            vec![0x42],
            (0u8..=255).collect(),
            vec![7u8; 129],
            (0..100u32)
                .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
                .collect(),
            lcg(0x1234_5678_9ABC_DEF0, 257),
            lcg(1, 1),
            lcg(2, 2),
            lcg(3, 1000),
        ]
    }

    #[test]
    fn build_and_positions() {
        for src in sample_sources() {
            let core = core_for(&src);
            let n = src.len() as u64;
            let h = History::build(&core, n, 0).unwrap();
            assert_eq!(h.logical_len, n);
            assert_eq!(h.core_len, core.len() as u64);
            assert_eq!(h.states.len() as u64, n + 1);
            assert_eq!(h.state_at(0), Some(0));
            assert_eq!(h.state_at(n), h.states.get(n as usize).copied());
            assert_eq!(h.state_at(n + 1), None);
            assert_eq!(h.snapshot_at(n + 1), None);
            let logs = state_log(&core, n).unwrap();
            for i in 0..=n {
                assert_eq!(h.state_at(i), Some(logs[i as usize]));
                assert_eq!(
                    h.snapshot_at(i),
                    Some(Snapshot::new(logs[i as usize], i)),
                    "snapshot mismatch at i={i} for src len {n}"
                );
            }
            assert_eq!(h.physical_bytes(), 4 * (n + 1));
        }
    }

    #[test]
    fn roundtrip_bytes() {
        for src in sample_sources() {
            let core = core_for(&src);
            let h = History::build(&core, src.len() as u64, 42).unwrap();
            let bytes = h.to_bytes();
            let back = History::from_bytes(&bytes).unwrap();
            assert_eq!(back.generation, h.generation);
            assert_eq!(back.logical_len, h.logical_len);
            assert_eq!(back.core_len, h.core_len);
            assert_eq!(back.core_hash, h.core_hash);
            assert_eq!(back.states, h.states);
            assert_eq!(back, h);
            assert_eq!(back.to_bytes(), bytes, "to_bytes/from_bytes not inverse");
        }
    }

    #[test]
    fn truncation_rejected() {
        let core = core_for(&lcg(9, 40));
        let h = History::build(&core, 40, 1).unwrap();
        let bytes = h.to_bytes();
        for cut in 0..bytes.len() {
            assert!(
                History::from_bytes(&bytes[..cut]).is_err(),
                "expected error at cut {cut}"
            );
        }
    }

    #[test]
    fn malformed_rejected() {
        let core = core_for(&lcg(11, 10));
        let h = History::build(&core, 10, 1).unwrap();

        let mut bytes = h.to_bytes();
        // A smaller-than-declared count is structurally inconsistent.
        bytes[56..64].copy_from_slice(&h.logical_len.to_le_bytes());
        assert!(matches!(
            History::from_bytes(&bytes),
            Err(EkbloomError::BadSection { .. })
        ));

        let mut bytes = h.to_bytes();
        bytes[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            History::from_bytes(&bytes),
            Err(EkbloomError::Overflow { .. })
        ));
    }

    #[test]
    fn verify_valid_and_tampered() {
        for src in sample_sources() {
            let core = core_for(&src);
            let h = History::build(&core, src.len() as u64, 0).unwrap();
            h.verify_against(&core).unwrap();

            if !core.is_empty() {
                let mut tampered = core.clone();
                tampered[0] ^= 0x01;
                assert!(
                    h.verify_against(&tampered).is_err(),
                    "tampered core accepted (len {})",
                    core.len()
                );
            }
            // Length change must also be rejected.
            let mut shorter = core.clone();
            shorter.push(0);
            assert!(h.verify_against(&shorter).is_err());
        }
    }

    #[test]
    fn extend_appends_without_rewriting() {
        let src = lcg(123, 200);
        let core = core_for(&src);
        let mut h = History::build(&core, 50, 0).unwrap();
        let prefix = h.states.clone();

        h.extend(&core, 150).unwrap();
        assert_eq!(h.logical_len, 150);
        assert_eq!(h.generation, 1);
        assert_eq!(&h.states[..prefix.len()], &prefix[..]);

        let full = History::build(&core, 150, 1).unwrap();
        assert_eq!(h.states, full.states);
        assert_eq!(h.core_hash, full.core_hash);
        assert_eq!(h.core_len, full.core_len);

        // No-op growth does not bump the generation.
        h.extend(&core, 150).unwrap();
        assert_eq!(h.generation, 1);
    }

    #[test]
    fn extend_rejects_shortening_short_core_and_changed_prefix() {
        let src = lcg(321, 100);
        let core = core_for(&src);
        let mut h = History::build(&core, 80, 0).unwrap();
        assert!(matches!(
            h.extend(&core, 79),
            Err(EkbloomError::InvalidArgument { .. })
        ));
        assert!(h.extend(&core[..60], 90).is_err());
        assert_eq!(h.logical_len, 80, "failed extend must not mutate");

        let mut h2 = History::build(&core, 80, 0).unwrap();
        let mut bad = core.clone();
        bad[0] ^= 0xFF;
        assert!(matches!(
            h2.extend(&bad, 90),
            Err(EkbloomError::BadSection { .. })
        ));
    }
}
