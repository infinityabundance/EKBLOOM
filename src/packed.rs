//! Packed 16-bit restart and history storage for `OrANS-NL2`.
//!
//! # Why 16 bits are enough (machine 1 only)
//!
//! The contract (`docs/DESIGN.md` §1) fixes the `OrANS-NL2` state log as
//! `x_0 = 0`, `x_1 = s_0`, `x_i = (s_{i-2} << 8) | s_{i-1}` for `i >= 2`:
//! the state *is* the previous two revealed symbols, so every state is exactly a
//! `u16`. The design also fixes the backing cursor to `p_i = i` for machine 1, so
//! a complete resumable state at a known logical position is fully described by
//! the 16-bit state alone — the cursor is *derived from the position*.
//!
//! The existing code stores `u32` states (`4*(N+1)` history bytes) and 12-byte
//! checkpoints (`u32` state ++ `u64` cursor). Both are redundant for machine 1:
//! the high 16 bits of every state are zero, and the cursor is the position.
//! These structures store the same information in 2 bytes per state.
//!
//! **Scope.** Packed-by-position is a *machine-1 property*. For a cursor-divergent
//! machine (e.g. `OrANS-RANS0`, `machine_id = 2`) the backing cursor is independent
//! of the logical position, so a state alone does not resume; the packed forms do
//! not apply and the general `Snapshot`/`CheckpointSet` forms must be used.
//!
//! # Structures
//!
//! * [`PackedHistory`] — `x_0 ..= x_N`, 2 bytes each.
//! * [`PackedRestarts`] — `x_{j*stride}`, 2 bytes each; `position = index << stride_shift`.
//! * [`state_from_hot`] — reconstruct `x_i` from a manifested output surface (a
//!   `hot` byte array covering positions `i-2, i-1`), avoiding a separate state array.
//!
//! All wire forms are little-endian, bounds-checked, and reject truncation and
//! trailing bytes with [`EkbloomError`].

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, NL2};
use crate::state::Snapshot;

/// Fixed wire-header size for both packed forms: a `u64` logical length plus
/// reserved bytes (a `u8` stride shift for restarts, all-zero reserved otherwise).
const HEADER_LEN: usize = 16;

/// Largest accepted stride shift: `1 << 63` still fits a `u64`.
const MAX_STRIDE_SHIFT: u8 = 63;

/// A state log `[x_0, ..., x_N]` stored as 16-bit machine states.
///
/// Each entry is exactly one `OrANS-NL2` state (`< 2^16`); the wire form is
/// `logical_len u64` ++ `8` reserved bytes ++ `(N+1)` little-endian `u16` states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackedHistory {
    /// `states[i] = x_i`, the machine state before revealing symbol `i`.
    pub states: Vec<u16>,
}

impl PackedHistory {
    /// Build the packed history for `core` producing `logical_len` symbols.
    ///
    /// Machine-1 only: uses [`NL2`]. Errors with
    /// [`EkbloomError::CoreTooShort`] if `core` cannot produce `logical_len`
    /// symbols, and with [`EkbloomError::BadSection`] if a state does not fit in
    /// `u16` (which cannot happen for machine 1 and is therefore a hard error).
    pub fn build(core: &[u8], logical_len: u64) -> Result<Self> {
        NL2.check_core(core, logical_len)?;
        let n = usize::try_from(logical_len).map_err(|_| EkbloomError::Overflow {
            what: "packed history length",
        })?;
        let capacity = n.checked_add(1).ok_or(EkbloomError::Overflow {
            what: "packed history length",
        })?;
        let mut states: Vec<u16> = Vec::with_capacity(capacity);

        let mut omega = NL2.init(core)?;
        states.push(pack_state(omega.state)?);
        for _ in 0..logical_len {
            NL2.reveal(core, &mut omega)?;
            states.push(pack_state(omega.state)?);
        }

        Ok(PackedHistory { states })
    }

    /// Packed payload size: `2 * (N+1)` bytes (the wire header is fixed).
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        (self.states.len() as u64).saturating_mul(2)
    }

    /// State `x_i` at logical position `i`, or `None` if out of range.
    #[inline]
    pub fn state_at(&self, i: u64) -> Option<u16> {
        usize::try_from(i)
            .ok()
            .and_then(|idx| self.states.get(idx).copied())
    }

    /// Serialize to `logical_len u64` ++ `8` reserved bytes ++ `(N+1)` `u16` states.
    pub fn to_bytes(&self) -> Vec<u8> {
        let logical_len = self.states.len().saturating_sub(1) as u64;
        let mut out = Vec::with_capacity(HEADER_LEN.saturating_add(self.states.len() * 2));
        out.extend_from_slice(&logical_len.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
        for &s in &self.states {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Parse the exact wire layout produced by [`PackedHistory::to_bytes`].
    ///
    /// Rejects truncation with [`EkbloomError::Truncated`] and any structural
    /// inconsistency (non-zero reserved bytes, trailing bytes, a count that does
    /// not equal `logical_len + 1`) with [`EkbloomError::BadSection`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let logical_len = read_u64(bytes, 0);
        if bytes[8..HEADER_LEN].iter().any(|&b| b != 0) {
            return Err(EkbloomError::BadSection {
                reason: "packed history reserved bytes are non-zero".to_string(),
            });
        }
        let count = logical_len.checked_add(1).ok_or(EkbloomError::Overflow {
            what: "packed history count",
        })?;
        let expected = (count)
            .checked_mul(2)
            .and_then(|payload| payload.checked_add(HEADER_LEN as u64))
            .ok_or(EkbloomError::Overflow {
                what: "packed history byte length",
            })?;
        if (bytes.len() as u64) < expected {
            return Err(EkbloomError::Truncated {
                needed: expected,
                available: bytes.len() as u64,
            });
        }
        if (bytes.len() as u64) > expected {
            return Err(EkbloomError::BadSection {
                reason: "packed history has trailing bytes".to_string(),
            });
        }
        let count_usize = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
            what: "packed history count",
        })?;
        let mut states = Vec::with_capacity(count_usize);
        let mut off = HEADER_LEN;
        for _ in 0..count_usize {
            states.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
            off += 2;
        }
        Ok(PackedHistory { states })
    }
}

/// A set of 16-bit complete restart states at a fixed power-of-two stride.
///
/// `states[j] = x_{j * stride}` with `stride = 1 << stride_shift`; the logical
/// position is implicit as `index << stride_shift`. This is the machine-1 analogue
/// of [`crate::checkpoint::CheckpointSet`], storing 2 bytes instead of 12.
///
/// The wire form is `stride_shift u8` ++ `7` reserved bytes ++ `logical_len u64`
/// ++ `count` little-endian `u16` states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackedRestarts {
    /// Power-of-two stride exponent: `stride = 1 << stride_shift`.
    pub stride_shift: u8,
    /// Logical length `N` this set was built for.
    pub logical_len: u64,
    /// `states[j] = x_{j * stride}`.
    pub states: Vec<u16>,
}

impl PackedRestarts {
    /// Build restarts for `core` producing `logical_len` symbols at
    /// `stride = 1 << stride_shift`.
    ///
    /// Machine-1 only. Rejects `stride_shift > 63` with
    /// [`EkbloomError::BadSection`] and a short `core` with
    /// [`EkbloomError::CoreTooShort`].
    pub fn build(core: &[u8], logical_len: u64, stride_shift: u8) -> Result<Self> {
        if stride_shift > MAX_STRIDE_SHIFT {
            return Err(EkbloomError::BadSection {
                reason: format!("stride_shift {stride_shift} exceeds {MAX_STRIDE_SHIFT}"),
            });
        }
        let stride = 1u64 << stride_shift; // stride_shift <= 63, so this cannot overflow
        NL2.check_core(core, logical_len)?;

        let count = (logical_len / stride)
            .checked_add(1)
            .ok_or(EkbloomError::Overflow {
                what: "packed restart count",
            })?;
        let count_usize = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
            what: "packed restart count",
        })?;
        let mut states: Vec<u16> = Vec::with_capacity(count_usize);

        let mut omega = NL2.init(core)?;
        states.push(pack_state(omega.state)?);
        for i in 0..logical_len {
            NL2.reveal(core, &mut omega)?;
            let pos = i + 1; // i < logical_len, so this cannot overflow
            if pos.is_multiple_of(stride) {
                states.push(pack_state(omega.state)?);
            }
        }

        Ok(PackedRestarts {
            stride_shift,
            logical_len,
            states,
        })
    }

    /// Packed payload size: `2 * states.len()` bytes (the wire header is fixed).
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        (self.states.len() as u64).saturating_mul(2)
    }

    /// The stride `1 << stride_shift`, or `None` if `stride_shift > 63`.
    #[inline]
    fn stride_opt(&self) -> Option<u64> {
        1u64.checked_shl(u32::from(self.stride_shift))
    }

    /// The covering checkpoint `(position, index)` for logical position `i`:
    /// `position = index << stride_shift`. `None` if `i > logical_len` or the
    /// covering slot is absent.
    pub fn position_of(&self, i: u64) -> Option<(u64, usize)> {
        if i > self.logical_len {
            return None;
        }
        let stride = self.stride_opt()?;
        let idx = i / stride;
        let ckpt = idx.checked_mul(stride)?;
        let idx_usize = usize::try_from(idx).ok()?;
        if idx_usize >= self.states.len() {
            return None;
        }
        Some((ckpt, idx_usize))
    }

    /// Restore the complete resumable state `Omega_i = (x_i, i)` by loading the
    /// covering restart and advancing at most `stride - 1` steps.
    ///
    /// Errors with [`EkbloomError::OutOfBounds`] when `i > logical_len`.
    pub fn snapshot_at(&self, core: &[u8], i: u64) -> Result<Snapshot> {
        let (ckpt, idx) = self.position_of(i).ok_or(EkbloomError::OutOfBounds {
            offset: i,
            length: 1,
            logical_len: self.logical_len,
        })?;
        let mut omega = Snapshot::new(u32::from(self.states[idx]), ckpt);
        // `steps = i % stride <= stride - 1` by construction of `ckpt`.
        let steps = i - ckpt;
        for _ in 0..steps {
            NL2.reveal(core, &mut omega)?;
        }
        Ok(omega)
    }

    /// Maximum replay steps from any restart: `stride - 1`.
    #[inline]
    pub fn max_replay_steps(&self) -> u64 {
        self.stride_opt().unwrap_or(1).saturating_sub(1)
    }

    /// Serialize to `stride_shift u8` ++ `7` reserved bytes ++ `logical_len u64`
    /// ++ `count` `u16` states.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN.saturating_add(self.states.len() * 2));
        out.push(self.stride_shift);
        out.extend_from_slice(&[0u8; 7]);
        out.extend_from_slice(&self.logical_len.to_le_bytes());
        for &s in &self.states {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Parse the exact wire layout produced by [`PackedRestarts::to_bytes`].
    ///
    /// Rejects an out-of-range stride shift, non-zero reserved bytes, truncation,
    /// trailing bytes, and a state count inconsistent with `logical_len`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let stride_shift = bytes[0];
        if stride_shift > MAX_STRIDE_SHIFT {
            return Err(EkbloomError::BadSection {
                reason: format!("stride_shift {stride_shift} exceeds {MAX_STRIDE_SHIFT}"),
            });
        }
        if bytes[1..8].iter().any(|&b| b != 0) {
            return Err(EkbloomError::BadSection {
                reason: "packed restart reserved bytes are non-zero".to_string(),
            });
        }
        let logical_len = read_u64(bytes, 8);
        let stride = 1u64 << stride_shift;
        let count = (logical_len / stride)
            .checked_add(1)
            .ok_or(EkbloomError::Overflow {
                what: "packed restart count",
            })?;
        let expected = (count)
            .checked_mul(2)
            .and_then(|payload| payload.checked_add(HEADER_LEN as u64))
            .ok_or(EkbloomError::Overflow {
                what: "packed restart byte length",
            })?;
        if (bytes.len() as u64) < expected {
            return Err(EkbloomError::Truncated {
                needed: expected,
                available: bytes.len() as u64,
            });
        }
        if (bytes.len() as u64) > expected {
            return Err(EkbloomError::BadSection {
                reason: "packed restarts have trailing bytes".to_string(),
            });
        }
        let count_usize = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
            what: "packed restart count",
        })?;
        let mut states = Vec::with_capacity(count_usize);
        let mut off = HEADER_LEN;
        for _ in 0..count_usize {
            states.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
            off += 2;
        }
        Ok(PackedRestarts {
            stride_shift,
            logical_len,
            states,
        })
    }
}

/// Reconstruct `x_i` from a manifested output surface.
///
/// If a manifested `hot` byte array (e.g. the path bloom's `hot` symbols, or any
/// exact output surface) covers logical positions `i-2` and `i-1`, those two bytes
/// **are** `x_i = (s_{i-2} << 8) | s_{i-1}` — no separate state array is needed.
///
/// Returns `Some(0)` for `i = 0` (genesis, always zero), `Some(hot[0])` for
/// `i = 1`, and `Some((hot[i-2] << 8) | hot[i-1])` for `i >= 2`. Returns `None`
/// when the surface does not cover the required positions.
#[inline]
pub fn state_from_hot(hot: &[u8], i: u64) -> Option<u16> {
    match i {
        0 => Some(0),
        1 => hot.first().map(|&b| u16::from(b)),
        _ => {
            let hi = *hot.get(usize::try_from(i - 2).ok()?)?;
            let lo = *hot.get(usize::try_from(i - 1).ok()?)?;
            Some((u16::from(hi) << 8) | u16::from(lo))
        }
    }
}

/// Narrow a machine-1 state to its exact 16-bit form.
#[inline]
fn pack_state(state: u32) -> Result<u16> {
    u16::try_from(state).map_err(|_| EkbloomError::BadSection {
        reason: format!("machine-1 state {state} does not fit in u16"),
    })
}

/// Read a little-endian `u64` at `off` (the caller has bounds-checked `off + 8`).
#[inline]
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}
