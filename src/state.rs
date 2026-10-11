//! Complete resumable operational state and packed wire forms.
//!
//! `Omega_i = (x_i, p_i)`: `state` is the machine's internal state word,
//! `cursor` is the backing-input position. For `machine_id = 1` (`OrANS-NL2`)
//! the cursor always equals the logical position, but the field is retained so
//! the format and readers remain machine-general.

use crate::error::{EkbloomError, Result};

/// Maximum number of independent rANS lanes an interleaved machine may expose.
/// The single-state machines (`machine_id = 1|2|3`) never use any lane.
pub const MAX_LANES: usize = 8;

/// Complete resumable operational state `Omega`.
///
/// The single-state machines (`machine_id = 1|2|3`) fill only `state`/`cursor`;
/// the interleaved order-0 rANS machine (`machine_id = 4`) additionally carries
/// its `[u32; K]` lane states and the index of the lane that reveals the next
/// symbol. The extra fields default to zero, so a snapshot of a single-state
/// machine is unchanged and still packs to the frozen 12-byte resume form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Internal machine state word. For the interleaved machine this is the
    /// active lane's state (`lanes[lane]`).
    pub state: u32,
    /// Backing-input cursor (byte offset into `core`).
    pub cursor: u64,
    /// Interleaved-machine lane states; all zero for the single-state machines.
    /// `lanes[lane]` is the state of the lane that reveals the *next* symbol.
    pub lanes: [u32; MAX_LANES],
    /// Index of the lane that reveals the next symbol (interleaved machines);
    /// zero for single-state machines.
    pub lane: u8,
}

impl Snapshot {
    /// Construct a single-state snapshot (`lanes` all zero).
    #[inline]
    pub const fn new(state: u32, cursor: u64) -> Self {
        Self {
            state,
            cursor,
            lanes: [0; MAX_LANES],
            lane: 0,
        }
    }

    /// Construct an interleaved snapshot from the complete lane state.
    #[inline]
    pub const fn with_lanes(cursor: u64, lanes: [u32; MAX_LANES], lane: u8) -> Self {
        Self {
            state: lanes[lane as usize],
            cursor,
            lanes,
            lane,
        }
    }

    /// Packed wire size of a complete interleaved resume point:
    /// `u32` state `++` `u64` cursor `++` `u8` lane `++` `MAX_LANES * u32` lanes.
    pub const WIDE_LEN: usize = 4 + 8 + 1 + 4 * MAX_LANES;

    /// Pack into the fixed wide little-endian resume form (interleaved machines).
    pub fn pack_wide(&self) -> [u8; Self::WIDE_LEN] {
        let mut out = [0u8; Self::WIDE_LEN];
        out[0..4].copy_from_slice(&self.state.to_le_bytes());
        out[4..12].copy_from_slice(&self.cursor.to_le_bytes());
        out[12] = self.lane;
        for (i, &x) in self.lanes.iter().enumerate() {
            let o = 13 + i * 4;
            out[o..o + 4].copy_from_slice(&x.to_le_bytes());
        }
        out
    }

    /// Unpack from the fixed wide little-endian resume form.
    pub fn unpack_wide(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::WIDE_LEN {
            return Err(EkbloomError::Truncated {
                needed: Self::WIDE_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let state = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let cursor = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        let lane = bytes[12];
        let mut lanes = [0u32; MAX_LANES];
        for (i, slot) in lanes.iter_mut().enumerate() {
            let o = 13 + i * 4;
            *slot = u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        }
        Ok(Self {
            state,
            cursor,
            lanes,
            lane,
        })
    }

    /// Packed wire size of a resume point: `u32` state ++ `u64` cursor, no padding.
    pub const PACKED_LEN: usize = 12;

    /// Pack into the fixed 12-byte little-endian resume form.
    pub fn pack(&self) -> [u8; Self::PACKED_LEN] {
        let mut out = [0u8; Self::PACKED_LEN];
        out[0..4].copy_from_slice(&self.state.to_le_bytes());
        out[4..12].copy_from_slice(&self.cursor.to_le_bytes());
        out
    }

    /// Unpack from the fixed 12-byte little-endian resume form.
    pub fn unpack(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::PACKED_LEN {
            return Err(EkbloomError::Truncated {
                needed: Self::PACKED_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let state = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let cursor = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        Ok(Self {
            state,
            cursor,
            lanes: [0; MAX_LANES],
            lane: 0,
        })
    }

    /// A state-only projection entry (`u32`), used by the full state log.
    #[inline]
    pub fn state_only(&self) -> u32 {
        self.state
    }
}

/// A logically contiguous run of per-position state projections.
#[derive(Clone, Debug, Default)]
pub struct StateLog {
    /// `states[i]` is `x_i`, the machine state before revealing symbol `i`.
    pub states: Vec<u32>,
}

impl StateLog {
    /// Number of recorded states (`logical_len + 1` for a complete log).
    #[inline]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Whether the log is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// State at logical position `i`.
    #[inline]
    pub fn get(&self, i: usize) -> Option<u32> {
        self.states.get(i).copied()
    }

    /// Packed byte size of the `u32` state projection.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.states.len() as u64 * 4
    }
}
