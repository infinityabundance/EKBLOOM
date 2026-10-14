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
    use crate::dictionary::{register, unregister};

    /// A tiny deterministic corpus. Kept small on purpose: these tests are also run
    /// under Miri, which is an interpreter (~100–1000× slower), so every input is a
    /// few bytes and every loop is bounded.
    const RECORDS: [&[u8]; 4] = [
        b"abcabc",
        b"user=alice|region=us-east-1",
        b"zzzzzzzzzz",
        b"ab",
    ];

    /// A dictionary trained from the shared corpus plus a test-unique salt, so every
    /// test that touches the process-wide registry gets a *distinct* content id. The
    /// registry is shared by the whole test process and tests run in parallel, so a
    /// single shared id would let one test's `unregister` break another test's
    /// `from_core` (or make its "unregistered" assertion race).
    fn trained(salt: &[u8]) -> Dictionary {
        let mut records: Vec<&[u8]> = RECORDS.to_vec();
        records.push(salt);
        dictionary::train_dictionary(&records).expect("train")
    }

    /// The little-endian `u32` embedded at `off` (the frame's `dict_len`).
    fn u32_at(core: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([core[off], core[off + 1], core[off + 2], core[off + 3]])
    }

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

    #[test]
    fn machine_is_registered_under_its_id() {
        assert!(crate::machine::machine_by_id(MACHINE_DICT).is_some());
    }

    /// The self-contained path: `OrMachine::encode` trains a dictionary from the
    /// source and embeds it, so decoding must not depend on the process-wide
    /// registry at all.
    #[test]
    fn embedded_core_round_trips_without_the_registry() {
        for src in RECORDS {
            let core = DICT.encode(src).expect("encode");
            let ctx = DictContext::from_core(&core).expect("context");
            assert_eq!(ctx.logical_len(), src.len() as u64);
            // Embedded: `dict_len > 0` and the embedded model validates by content id.
            assert!(
                u32_at(&core, 16) > 0,
                "embedded frame carries the dictionary"
            );
            assert_eq!(DICT.decode(&core, src.len() as u64).expect("decode"), src);
            assert!(DICT.check_core(&core, src.len() as u64).is_ok());
        }
    }

    /// The referenced path: the core names a dictionary by id and the model is
    /// resolved from the registry. An unregistered id is a typed error — a record is
    /// never decoded against the wrong (or a missing) model.
    #[test]
    fn referenced_core_resolves_through_the_registry_or_is_refused() {
        let dict = trained(b"referenced");
        let src = RECORDS[1];
        let core = frame_reference(&dict, &dict.encode(src).expect("encode"));
        assert_eq!(u32_at(&core, 16), 0, "referenced frame embeds nothing");

        unregister(&dict.id());
        assert!(!dictionary::registered(&dict.id()));
        assert!(matches!(
            DictContext::from_core(&core),
            Err(EkbloomError::BadSection { .. })
        ));

        register(&dict).expect("register");
        let ctx = DictContext::from_core(&core).expect("context");
        assert_eq!(ctx.dict_id(), dict.id());
        assert_eq!(DICT.decode(&core, src.len() as u64).expect("decode"), src);
        unregister(&dict.id());
    }

    /// A truncation must never panic; every prefix shorter than the fixed frame is
    /// refused with a typed error.
    #[test]
    fn truncation_is_a_typed_error_and_never_panics() {
        let dict = trained(b"truncation");
        register(&dict).expect("register");
        let core = frame_reference(&dict, &dict.encode(RECORDS[1]).expect("encode"));
        for n in 0..core.len() {
            // Must not panic for any prefix: it either parses (a longer prefix can
            // still carry the header) or returns a typed error.
            if let Ok(ctx) = DictContext::from_core(&core[..n]) {
                // If it parsed, a decode is still bounded and never panics.
                let _ = DICT.decode(&core[..n], ctx.logical_len());
            }
        }
        for n in 0..DICT_FRAME_PREFIX_BYTES {
            assert!(
                DictContext::from_core(&core[..n]).is_err(),
                "frame prefix {n} must be refused"
            );
        }
        assert!(DictContext::from_core(&core).is_ok());
        unregister(&dict.id());
    }

    /// A tampered embedded dictionary is refused: `from_bytes` recomputes the
    /// content id, so a single flipped byte can no longer resolve.
    #[test]
    fn tampered_embedded_dictionary_is_refused() {
        let core = DICT.encode(RECORDS[1]).expect("encode");
        let dict_len = u32_at(&core, 16) as usize;
        assert!(dict_len > 0);
        let mut tampered = core.clone();
        let at = DICT_FRAME_PREFIX_BYTES + dict_len / 2;
        tampered[at] ^= 0x01;
        assert!(
            DictContext::from_core(&tampered).is_err(),
            "a flipped dictionary byte must not resolve"
        );
    }

    /// An out-of-range genesis state is a typed error, not a wrong byte.
    #[test]
    fn out_of_range_record_state_is_refused() {
        let core = DICT.encode(RECORDS[1]).expect("encode");
        let dict_len = u32_at(&core, 16) as usize;
        let state_off = DICT_FRAME_PREFIX_BYTES + dict_len + 4;
        let mut bad = core.clone();
        bad[state_off..state_off + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(!valid_state(0));
        assert!(matches!(
            DictContext::from_core(&bad),
            Err(EkbloomError::BadSection { .. })
        ));
    }

    /// A declared logical length that disagrees with the core is refused.
    #[test]
    fn logical_len_mismatch_is_refused() {
        let src = RECORDS[0];
        let core = DICT.encode(src).expect("encode");
        let ok = src.len() as u64;
        assert_eq!(DICT.decode(&core, ok).expect("decode"), src);
        assert!(matches!(
            DICT.decode(&core, ok + 1),
            Err(EkbloomError::BadSection { .. })
        ));
        assert!(DICT.check_core(&core, ok + 1).is_err());
    }

    /// Reveal / peek / skip all agree with the record, from genesis, for every
    /// corpus entry and at a mid-record skip boundary.
    #[test]
    fn reveal_peek_and_skip_reproduce_the_record() {
        let dict = trained(b"reveal-peek-skip");
        register(&dict).expect("register");
        for src in RECORDS {
            let core = frame_reference(&dict, &dict.encode(src).expect("encode"));
            let ctx = DictContext::from_core(&core).expect("context");

            let mut omega = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
            let mut out = Vec::new();
            ctx.decode_into(&core, &mut omega, ctx.logical_len(), &mut out)
                .expect("decode_into");
            assert_eq!(out, src);

            if !src.is_empty() {
                let mut omega = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
                let peeked = ctx.peek(&core, &omega).expect("peek");
                let revealed = ctx.reveal(&core, &mut omega).expect("reveal");
                assert_eq!(peeked, revealed);
                assert_eq!(peeked, src[0]);
            }

            if src.len() >= 2 {
                let n = src.len() / 2;
                let mut omega = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
                ctx.skip(&core, &mut omega, n as u64).expect("skip");
                let mut rest = Vec::new();
                ctx.decode_into(&core, &mut omega, (src.len() - n) as u64, &mut rest)
                    .expect("rest");
                assert_eq!(rest, &src[n..]);
            }
        }
        unregister(&dict.id());
    }

    /// Restart validation accepts the genesis snapshot and rejects an illegal state.
    #[test]
    fn restart_validation_is_bounded() {
        let core = DICT.encode(RECORDS[1]).expect("encode");
        let ctx = DictContext::from_core(&core).expect("context");
        let genesis = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
        assert!(ctx.validate_restart(&genesis).is_ok());
        let illegal = Snapshot::new(0, ctx.header_bytes());
        assert!(matches!(
            ctx.validate_restart(&illegal),
            Err(EkbloomError::BadSection { .. })
        ));
    }
}
