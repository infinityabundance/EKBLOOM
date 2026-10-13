//! `OrANS-DICT` (`machine_id = 9`): the shared-dictionary record machine.
//!
//! Machine 2 stores a 512-byte order-0 model in **every** core, so a 100-byte
//! record pays a five-times-payload header. Machine 9 (`OrANS-DICT`) lets many
//! records share one trained [`Dictionary`](crate::dictionary::Dictionary): the
//! dictionary is stored once (embedded in a self-contained core, or referenced by
//! id and resolved from the process-wide registry), and each record is a compact
//! header-less rANS core in machine 2's exact model semantics.
//!
//! # Core layout (`core = encode(source)`)
//!
//! ```text
//! [dict_id: 16][dict_len: u32 LE]
//! [dict_bytes: dict_len]              # present iff dict_len > 0 (embedded)
//! [logical_len: u32 LE][initial_state: u32 LE][renorm bytes, reversed]
//! ```
//!
//! `dict_len == 0` means the core **references** `dict_id`: the dictionary is
//! resolved from [`crate::dictionary::lookup`], and an id that is not registered is
//! a typed error — a record is never decoded against the wrong model. `dict_len > 0`
//! embeds a serialized dictionary, which is validated against `dict_id` by
//! [`Dictionary::from_bytes`](crate::dictionary::Dictionary::from_bytes) (which
//! recomputes the content id), so a tampered dictionary is also refused.
//!
//! # Semantics
//!
//! Genesis `Omega_0 = (initial_state, record_offset + 8)`. The cursor is the byte
//! offset into `core` and **diverges** from the logical position (as machine 2), so
//! a read replays from genesis through the prepared [`DictContext`]. The step is
//! machine 2's prepared step exactly (the shared model drives the same fused
//! `slot -> (freq, cum, symbol)` table).

use crate::dictionary::{self, Dictionary, DICT_RECORD_HEADER_BYTES};
use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_DICT};
use crate::machine_rans::{valid_state, RansContext, MAX_STEPS_PER_BYTE};
use crate::state::Snapshot;

/// Fixed core framing before the (optional) embedded dictionary:
/// `[dict_id: 16][dict_len: u32]`.
pub const DICT_FRAME_PREFIX_BYTES: usize = 20;

/// The `OrANS-DICT` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct DictMachine;

/// The single process-wide `OrANS-DICT` instance.
pub static DICT: DictMachine = DictMachine;

impl DictMachine {
    /// Construct the machine.
    pub const fn new() -> Self {
        DictMachine
    }
}

/// Frame `core` so machine 9 **references** `dict` (no embedded bytes).
pub fn frame_reference(dict: &Dictionary, core: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DICT_FRAME_PREFIX_BYTES.saturating_add(core.len()));
    out.extend_from_slice(&dict.id());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(core);
    out
}

/// Frame `core` so machine 9 **embeds** the serialized `dict` (self-contained).
pub fn frame_embedded(dict: &Dictionary, core: &[u8]) -> Vec<u8> {
    let dict_bytes = dict.to_bytes();
    let mut out =
        Vec::with_capacity(DICT_FRAME_PREFIX_BYTES.saturating_add(dict_bytes.len() + core.len()));
    out.extend_from_slice(&dict.id());
    out.extend_from_slice(&(dict_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&dict_bytes);
    out.extend_from_slice(core);
    out
}

/// Read a little-endian `u32` at `off`, with a checked bounds error.
fn read_u32(core: &[u8], off: usize) -> Result<u32> {
    let end = off.checked_add(4).ok_or(EkbloomError::Overflow {
        what: "dictionary core offset",
    })?;
    let bytes = core.get(off..end).ok_or(EkbloomError::Truncated {
        needed: end as u64,
        available: core.len() as u64,
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Hex rendering of a dictionary id, for error messages.
fn hex16(id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in id.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The step budget for `core`: `MAX_STEPS_PER_BYTE` per byte plus slack, so a
/// crafted non-renormalising stream is cut off with [`EkbloomError::NonTermination`].
fn step_cap(core: &[u8]) -> u64 {
    (core.len() as u64)
        .saturating_mul(MAX_STEPS_PER_BYTE)
        .saturating_add(1 << 20)
}

/// A validated, reusable machine-9 decoder context for one core: the resolved
/// shared-model fused table plus the record geometry.
pub struct DictContext {
    rans: RansContext,
    dict_id: [u8; 16],
    initial_state: u32,
    record_offset: usize,
    logical_len: u64,
}

impl DictContext {
    /// Parse and validate a machine-9 core: the framing, the resolved dictionary
    /// (embedded, validated by content id, or referenced, resolved from the
    /// registry), and the record header (`logical_len` ++ `initial_state`).
    ///
    /// An unknown/wrong dictionary id, a bad embedded dictionary, a truncated core
    /// and an out-of-range record state are each a typed `Err`.
    pub fn from_core(core: &[u8]) -> Result<DictContext> {
        if core.len() < DICT_FRAME_PREFIX_BYTES {
            return Err(EkbloomError::Truncated {
                needed: DICT_FRAME_PREFIX_BYTES as u64,
                available: core.len() as u64,
            });
        }
        let mut dict_id = [0u8; 16];
        dict_id.copy_from_slice(&core[..16]);
        let dict_len = read_u32(core, 16)? as usize;
        let record_offset =
            DICT_FRAME_PREFIX_BYTES
                .checked_add(dict_len)
                .ok_or(EkbloomError::Overflow {
                    what: "dictionary core offset",
                })?;

        let dict = if dict_len == 0 {
            dictionary::lookup(&dict_id).ok_or_else(|| EkbloomError::BadSection {
                reason: format!("unknown or unregistered dictionary id {}", hex16(&dict_id)),
            })?
        } else {
            let bytes = core.get(DICT_FRAME_PREFIX_BYTES..record_offset).ok_or(
                EkbloomError::Truncated {
                    needed: record_offset as u64,
                    available: core.len() as u64,
                },
            )?;
            let embedded = Dictionary::from_bytes(bytes)?;
            if embedded.id() != dict_id {
                return Err(EkbloomError::BadSection {
                    reason: "embedded dictionary id does not match the core's dictionary id"
                        .to_string(),
                });
            }
            embedded
        };

        let record_end =
            record_offset
                .checked_add(DICT_RECORD_HEADER_BYTES)
                .ok_or(EkbloomError::Overflow {
                    what: "dictionary core offset",
                })?;
        if record_end > core.len() {
            return Err(EkbloomError::Truncated {
                needed: record_end as u64,
                available: core.len() as u64,
            });
        }
        let logical_len = u64::from(read_u32(core, record_offset)?);
        let state_off = record_offset.checked_add(4).ok_or(EkbloomError::Overflow {
            what: "dictionary core offset",
        })?;
        let initial_state = read_u32(core, state_off)?;
        if !valid_state(initial_state) {
            return Err(EkbloomError::BadSection {
                reason: format!("dictionary record state {initial_state} out of range"),
            });
        }
        crate::resource::checked_default(logical_len)?;

        let header = dict.model_header(initial_state);
        let rans = RansContext::from_core(&header)?;
        Ok(DictContext {
            rans,
            dict_id,
            initial_state,
            record_offset,
            logical_len,
        })
    }

    /// The dictionary content id this core resolved against.
    #[inline]
    pub fn dict_id(&self) -> [u8; 16] {
        self.dict_id
    }

    /// The record's genesis coder state.
    #[inline]
    pub fn initial_state(&self) -> u32 {
        self.initial_state
    }

    /// The number of symbols the record reveals (carried in the core).
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The minimal legal encoded cursor: the start of the record's renorm bytes.
    #[inline]
    pub fn header_bytes(&self) -> u64 {
        (self.record_offset + DICT_RECORD_HEADER_BYTES) as u64
    }

    /// Reveal one symbol against this prepared context.
    #[inline]
    pub fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        self.rans.reveal(core, omega)
    }

    /// The symbol that would be revealed next, without advancing `omega`.
    #[inline]
    pub fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        self.rans.peek(core, omega)
    }

    /// Advance `omega` by exactly `n` decoder steps, discarding the symbols.
    #[inline]
    pub fn skip(&self, core: &[u8], omega: &mut Snapshot, n: u64) -> Result<()> {
        self.rans.skip(core, omega, n)
    }

    /// Reveal `n` symbols from `omega`, appending them to `out`.
    #[inline]
    pub fn decode_into(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.rans.decode_into(core, omega, n, out)
    }

    /// Validate a restart snapshot: the single coder state must be legal.
    pub fn validate_restart(&self, omega: &Snapshot) -> Result<()> {
        if valid_state(omega.state) {
            Ok(())
        } else {
            Err(EkbloomError::BadSection {
                reason: format!("dictionary restart state {} out of range", omega.state),
            })
        }
    }
}

impl OrMachine for DictMachine {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_DICT
    }

    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        let ctx = DictContext::from_core(core)?;
        Ok(Snapshot::new(ctx.initial_state, ctx.header_bytes()))
    }

    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        DictContext::from_core(core)?.reveal(core, omega)
    }

    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        DictContext::from_core(core)?.peek(core, omega)
    }

    /// Self-contained encode: train a dictionary from `source` and embed it.
    ///
    /// This is the single-record path; for a corpus the caller should train one
    /// [`Dictionary`] and reference it from every record
    /// ([`crate::manifest::create_dictionary`]).
    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        let dict = dictionary::train_dictionary(&[source])?;
        let record = dict.encode(source)?;
        Ok(frame_embedded(&dict, &record))
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = DictContext::from_core(core)?;
        if logical_len != ctx.logical_len() {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "dictionary core declares {} symbols, but {logical_len} were expected",
                    ctx.logical_len()
                ),
            });
        }
        let cap = step_cap(core);
        let mut omega = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
        let mut steps = 0u64;
        while steps < logical_len {
            if steps >= cap {
                return Err(EkbloomError::NonTermination);
            }
            ctx.reveal(core, &mut omega)?;
            steps += 1;
        }
        Ok(())
    }

    fn decode(&self, core: &[u8], logical_len: u64) -> Result<Vec<u8>> {
        let ctx = DictContext::from_core(core)?;
        if logical_len != ctx.logical_len() {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "dictionary core declares {} symbols, but {logical_len} were expected",
                    ctx.logical_len()
                ),
            });
        }
        crate::resource::checked_default(logical_len)?;
        let cap = step_cap(core);
        let mut omega = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
        let mut out: Vec<u8> = Vec::new();
        let mut steps = 0u64;
        while steps < logical_len {
            if steps >= cap {
                return Err(EkbloomError::NonTermination);
            }
            out.push(ctx.reveal(core, &mut omega)?);
            steps += 1;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_prefix_is_dict_id_plus_len() {
        let dict = dictionary::train_dictionary(&[b"abcabc"]).expect("train");
        let core = dict.encode(b"abcabc").expect("encode");
        let referenced = frame_reference(&dict, &core);
        assert_eq!(referenced.len(), DICT_FRAME_PREFIX_BYTES + core.len());
        assert_eq!(&referenced[..16], &dict.id());
        assert_eq!(&referenced[16..20], &0u32.to_le_bytes());
        let embedded = frame_embedded(&dict, &core);
        assert_eq!(
            embedded.len(),
            DICT_FRAME_PREFIX_BYTES + dict.to_bytes().len() + core.len()
        );
        assert_eq!(
            &embedded[16..20],
            &(dict.to_bytes().len() as u32).to_le_bytes()
        );
    }
}
