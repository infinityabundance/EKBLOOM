//! `OrANS-PACK` (`machine_id = 5`): a **directly-addressable, fixed-width packed**
//! representation.
//!
//! Machines 2 and 4 compress skewed sources, but their decode step is a dependent
//! chain: a symbol cannot be revealed before its predecessor, and a positional
//! read from a cold restart pays the block skip plus the requested reveals. Machine
//! 5 gives that up deliberately: it stores each symbol as a fixed `bits`-wide index
//! into the source's alphabet, so a position `i` is an **O(1) shift/mask** with no
//! division, no modulo, no replay and no allocation — at the cost of fixed-width
//! size (`ceil(N * bits / 8)` bytes) instead of order-0 entropy coding.
//!
//! # Core layout (`core = encode(source)`)
//!
//! ```text
//! [bits: u8][k: u8][alphabet: k bytes][logical_len: u64 LE][payload ...]
//! ```
//!
//! * `bits ∈ [1, 8]` — the fixed symbol width.
//! * `k` — the alphabet size, or `0` for the **identity alphabet** (all 256 byte
//!   values present; no alphabet bytes are stored and `symbol = index`).
//! * `alphabet` — the `k` distinct source symbols, ascending (absent when `k == 0`).
//! * `logical_len` — `N`, the number of symbols the core reveals.
//! * `payload` — `ceil(N * bits / 8)` bytes, symbols packed **little-endian**
//!   within the bit stream. The special renderings are:
//!   * `k == 1` (constant source): the payload is **empty** — the symbol is
//!     `alphabet[0]` for every position (zero payload).
//!   * `k == 0` (identity): `bits == 8` and the payload is the source verbatim.
//!
//! For position `i`:
//!
//! ```text
//! bit  = i * bits
//! byte = bit >> 3
//! off  = bit & 7
//! idx  = (payload[byte] >> off) & mask          # one byte when off+bits <= 8
//!      = ((payload[byte] >> off) | (payload[byte+1] << (8-off))) & mask
//! symbol = if k == 0 { idx } else { alphabet[idx] }
//! ```
//!
//! # Semantics
//!
//! Genesis `Omega_0 = (0, 0)`; the cursor **equals** the logical position
//! (`p_i = i`), exactly as machines 1 and 3, and `state` carries the last revealed
//! symbol (for continuity with machine 3). A complete resumable snapshot is
//! therefore trivial and exact: `Snapshot::new(state, i)`. Every general
//! checkpoint / block / tile / bloom path applies through the machine-general code
//! because the cursor is the position.
//!
//! `encode` discovers the alphabet, picks the smallest fixed width from
//! `{1, 2, 4, 8}` that fits, and packs the symbols; when that does **not** beat raw
//! (`packed.len() >= source.len()`) it falls back to the identity rendering
//! (`bits = 8`, `k = 0`, payload `= source`), which is the raw source plus a
//! 10-byte header. A general `bits` header (e.g. `3`, `5`, `6`, `7`) is fully
//! decodable — [`PackContext`] handles every `bits ∈ [1, 8]` — and the encoder can
//! be steered to one with [`Pack::encode_with_bits`].

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_PACK};
use crate::state::Snapshot;

/// The minimum packed symbol width.
const MIN_BITS: u8 = 1;
/// The maximum packed symbol width.
const MAX_BITS: u8 = 8;
/// Bytes preceding the `alphabet` field (`bits` ++ `k`).
const ALPHABET_OFFSET: usize = 2;
/// Bytes preceding the `logical_len` field (`bits` ++ `k` ++ `alphabet`).
const LEN_TRAILER: usize = 8;

/// The fixed mask for a `bits`-wide index.
#[inline]
const fn bit_mask(bits: u8) -> u8 {
    if bits >= 8 {
        0xFF
    } else {
        (1u8 << bits) - 1
    }
}

/// The payload length `ceil(n * bits / 8)` for a general packed core, or `0` for
/// the constant (`k == 1`) rendering. Overflow-safe: the intermediate is `u128`.
fn payload_len(n: u64, bits: u8, k: usize) -> Result<usize> {
    if k == 1 || n == 0 {
        return Ok(0);
    }
    let total = u128::from(n) * u128::from(bits);
    let bytes = total.div_ceil(8);
    usize::try_from(bytes).map_err(|_| EkbloomError::Overflow {
        what: "packed payload length",
    })
}

/// The `OrANS-PACK` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pack;

/// The single process-wide `OrANS-PACK` instance.
pub static PACK: Pack = Pack;

impl Pack {
    /// Construct the machine.
    pub const fn new() -> Self {
        Pack
    }

    /// Encode `source` with an explicit symbol width `bits ∈ [1, 8]`.
    ///
    /// The alphabet is discovered and the payload is packed at the requested
    /// width. The raw fallback is **not** applied, so this is the entry point for
    /// exercising a general (non-power-of-two) `bits` header; it errors with
    /// [`EkbloomError::InvalidArgument`] for `bits ∉ [1, 8]` and with
    /// [`EkbloomError::BadSection`] when the source's alphabet does not fit
    /// (`k > 2^bits`).
    pub fn encode_with_bits(&self, source: &[u8], bits: u8) -> Result<Vec<u8>> {
        if !(MIN_BITS..=MAX_BITS).contains(&bits) {
            return Err(EkbloomError::InvalidArgument {
                what: format!("packed symbol width {bits} out of range [1, 8]"),
            });
        }
        let layout = Layout::discover(source);
        if layout.k == 0 {
            // The empty source has no symbols; its width is irrelevant.
            return Ok(identity_core(source));
        }
        let cap = 1usize << bits;
        if layout.k > cap {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "source alphabet size {} does not fit a {bits}-bit index (max {cap})",
                    layout.k
                ),
            });
        }
        pack_core(source, bits, &layout)
    }

    /// Reveal the symbol at logical position `i` directly (O(1), no state).
    pub fn byte_at(&self, core: &[u8], i: u64) -> Result<u8> {
        PackContext::from_core(core)?.byte_at(core, i)
    }

    /// Append every revealed symbol of `core` to `out` (sequential bulk read).
    pub fn bulk_unpack(&self, core: &[u8], out: &mut Vec<u8>) -> Result<()> {
        PackContext::from_core(core)?.bulk_unpack(core, out)
    }

    /// The number of logical symbols `core` declares.
    pub fn logical_len(&self, core: &[u8]) -> Result<u64> {
        Ok(PackContext::from_core(core)?.logical_len)
    }
}

/// The discovered alphabet of a source.
struct Layout {
    /// Number of distinct symbols (`0` for an empty source, meaning "no payload").
    k: usize,
    /// The distinct symbols ascending (`k` valid entries).
    alphabet: [u8; 256],
}

impl Layout {
    /// Discover the distinct symbols of `source`, ascending.
    fn discover(source: &[u8]) -> Layout {
        let mut seen = [false; 256];
        let mut k = 0usize;
        for &b in source {
            if !seen[b as usize] {
                seen[b as usize] = true;
                k += 1;
            }
        }
        let mut alphabet = [0u8; 256];
        let mut idx = 0usize;
        for (v, present) in seen.iter().enumerate() {
            if *present {
                alphabet[idx] = v as u8;
                idx += 1;
            }
        }
        Layout { k, alphabet }
    }
}

/// Build a packed core for `source` at width `bits` with `layout`'s alphabet.
///
/// `k == 0` (empty source) and `k == 1` (constant) render with an empty payload;
/// otherwise the symbols are packed little-endian at `bits` bits each.
fn pack_core(source: &[u8], bits: u8, layout: &Layout) -> Result<Vec<u8>> {
    let k = layout.k;
    let n = source.len() as u64;
    let header_len = ALPHABET_OFFSET + k + LEN_TRAILER;
    let payload = payload_len(n, bits, k)?;
    let mut core = Vec::with_capacity(header_len.saturating_add(payload));
    core.push(bits);
    core.push(u8::try_from(k).map_err(|_| EkbloomError::BadSection {
        reason: format!("packed alphabet size {k} exceeds 255"),
    })?);
    if k > 0 {
        core.extend_from_slice(&layout.alphabet[..k]);
    }
    core.extend_from_slice(&n.to_le_bytes());
    if k >= 2 {
        let mut index_of = [0u8; 256];
        for (i, &sym) in layout.alphabet.iter().take(k).enumerate() {
            index_of[sym as usize] = i as u8;
        }
        let mut acc: u32 = 0;
        let mut held: u32 = 0;
        for &s in source {
            acc |= u32::from(index_of[s as usize]) << held;
            held += u32::from(bits);
            while held >= 8 {
                core.push((acc & 0xFF) as u8);
                acc >>= 8;
                held -= 8;
            }
        }
        if held > 0 {
            core.push((acc & 0xFF) as u8);
        }
    }
    debug_assert_eq!(core.len(), header_len + payload);
    Ok(core)
}

/// The identity (raw) rendering: `bits = 8`, `k = 0`, payload `= source`.
fn identity_core(source: &[u8]) -> Vec<u8> {
    let mut core = Vec::with_capacity(ALPHABET_OFFSET + LEN_TRAILER + source.len());
    core.push(8);
    core.push(0);
    core.extend_from_slice(&(source.len() as u64).to_le_bytes());
    core.extend_from_slice(source);
    core
}

/// A parsed, validated packed core: the fixed symbol width, the alphabet mapping
/// and the header/payload geometry. Built once by [`crate::machine::prepare`] and
/// reused for bulk decode and positional reads.
#[derive(Clone, Debug)]
pub struct PackContext {
    /// Symbol width `bits ∈ [1, 8]`.
    bits: u8,
    /// Alphabet size (`0` identity, `1` constant, else `2..=255`).
    k: usize,
    /// The alphabet mapping (`k` valid entries when `k >= 2`; `alphabet[0]` holds
    /// the constant symbol when `k == 1`).
    alphabet: [u8; 256],
    /// Logical length `N`.
    logical_len: u64,
    /// Header length in bytes.
    header_len: usize,
    /// Payload length in bytes.
    payload_len: usize,
}

impl PackContext {
    /// Parse and validate `core`'s packed header, geometry and alphabet.
    ///
    /// Every malformed shape is a typed error: a width outside `[1, 8]`, an
    /// alphabet that does not fit the width (`k > 2^bits`), an identity alphabet
    /// without `bits == 8`, a duplicated alphabet entry, a truncated payload, or
    /// trailing bytes after it.
    pub fn from_core(core: &[u8]) -> Result<PackContext> {
        if core.len() < ALPHABET_OFFSET {
            return Err(EkbloomError::Truncated {
                needed: ALPHABET_OFFSET as u64,
                available: core.len() as u64,
            });
        }
        let bits = core[0];
        if !(MIN_BITS..=MAX_BITS).contains(&bits) {
            return Err(EkbloomError::BadSection {
                reason: format!("packed symbol width {bits} out of range [1, 8]"),
            });
        }
        let k = core[1] as usize;
        if k > (1usize << bits) {
            return Err(EkbloomError::BadSection {
                reason: format!("packed alphabet size {k} exceeds 2^{bits}"),
            });
        }
        if k == 0 && bits != 8 {
            return Err(EkbloomError::BadSection {
                reason: format!("identity packed alphabet requires bits = 8, got {bits}"),
            });
        }
        let alpha_len = if k == 0 { 0 } else { k };
        let n_off = ALPHABET_OFFSET + alpha_len;
        let header_len = n_off + LEN_TRAILER;
        if core.len() < header_len {
            return Err(EkbloomError::Truncated {
                needed: header_len as u64,
                available: core.len() as u64,
            });
        }
        let mut alphabet = [0u8; 256];
        if k >= 2 {
            let mut seen = [false; 256];
            for (i, &sym) in core[ALPHABET_OFFSET..n_off].iter().enumerate() {
                if seen[sym as usize] {
                    return Err(EkbloomError::BadSection {
                        reason: format!("packed alphabet entry {i} duplicates symbol {sym}"),
                    });
                }
                seen[sym as usize] = true;
                alphabet[i] = sym;
            }
        } else if k == 1 {
            alphabet[0] = core[ALPHABET_OFFSET];
        }
        let logical_len = u64::from_le_bytes([
            core[n_off],
            core[n_off + 1],
            core[n_off + 2],
            core[n_off + 3],
            core[n_off + 4],
            core[n_off + 5],
            core[n_off + 6],
            core[n_off + 7],
        ]);
        let payload_len = payload_len(logical_len, bits, k)?;
        let need = header_len
            .checked_add(payload_len)
            .ok_or(EkbloomError::Overflow {
                what: "packed core length",
            })?;
        if core.len() < need {
            return Err(EkbloomError::Truncated {
                needed: need as u64,
                available: core.len() as u64,
            });
        }
        if core.len() > need {
            return Err(EkbloomError::BadSection {
                reason: format!("packed core has {} trailing bytes", core.len() - need),
            });
        }
        Ok(PackContext {
            bits,
            k,
            alphabet,
            logical_len,
            header_len,
            payload_len,
        })
    }

    /// The logical length `N`.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The symbol width `bits`.
    #[inline]
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// The alphabet size (`0` identity, `1` constant, else `2..=255`).
    #[inline]
    pub fn alphabet_len(&self) -> usize {
        self.k
    }

    /// The payload bytes of `core` (after the header).
    #[inline]
    fn payload<'a>(&self, core: &'a [u8]) -> &'a [u8] {
        &core[self.header_len..self.header_len + self.payload_len]
    }

    /// The symbol at logical position `i` (O(1): shifts and one or two byte loads,
    /// no division, no modulo, no allocation).
    #[inline]
    pub fn byte_at(&self, core: &[u8], i: u64) -> Result<u8> {
        if i >= self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: i + 1,
                available: self.logical_len,
            });
        }
        if self.k == 1 {
            return Ok(self.alphabet[0]);
        }
        let bit = u128::from(i) * u128::from(self.bits);
        let byte = usize::try_from(bit >> 3).map_err(|_| EkbloomError::Overflow {
            what: "packed bit offset",
        })?;
        let off = (bit & 7) as u32;
        let payload = self.payload(core);
        let lo = u32::from(payload[byte]);
        let idx = if off + u32::from(self.bits) <= 8 {
            (lo >> off) & u32::from(bit_mask(self.bits))
        } else {
            let hi = u32::from(*payload.get(byte + 1).unwrap_or(&0));
            ((lo >> off) | (hi << (8 - off))) & u32::from(bit_mask(self.bits))
        } as u8;
        if self.k == 0 {
            Ok(idx)
        } else {
            self.alphabet
                .get(idx as usize)
                .copied()
                .filter(|_| (idx as usize) < self.k)
                .ok_or_else(|| EkbloomError::BadSection {
                    reason: format!("packed index {idx} outside alphabet of size {}", self.k),
                })
        }
    }

    /// Reveal one symbol, advancing `omega` by one position.
    #[inline]
    pub fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let s = self.byte_at(core, omega.cursor)?;
        omega.state = u32::from(s);
        omega.cursor += 1;
        Ok(s)
    }

    /// The symbol that would be revealed next, without advancing.
    #[inline]
    pub fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        self.byte_at(core, omega.cursor)
    }

    /// Advance `omega` by exactly `n` positions in O(1) (no per-symbol work).
    pub fn skip(&self, _core: &[u8], omega: &mut Snapshot, n: u64) -> Result<()> {
        let end = omega.cursor.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "packed skip length",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: end,
                available: self.logical_len,
            });
        }
        omega.cursor = end;
        Ok(())
    }

    /// Reveal `n` symbols from `omega`, appending them to `out`.
    pub fn decode_into(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let start = omega.cursor;
        self.unpack_into(core, start, n, out)?;
        omega.cursor = start + n;
        Ok(())
    }

    /// Append `len` symbols starting at logical position `start` to `out`.
    pub fn unpack_range(&self, core: &[u8], start: u64, len: u64, out: &mut Vec<u8>) -> Result<()> {
        self.unpack_into(core, start, len, out)
    }

    /// Append every revealed symbol to `out` (the sequential bulk read).
    pub fn bulk_unpack(&self, core: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.unpack_into(core, 0, self.logical_len, out)
    }

    /// The shared sequential unpack kernel: `n` symbols from logical position
    /// `start`, appended to `out`.
    ///
    /// The loop is a scalar, branch-lean, auto-vectorisable unpack (no `unsafe`
    /// SIMD): for the power-of-two widths `{1, 2, 4, 8}` the one-byte branch is
    /// always taken, so the body is a shift, a mask and a table load per symbol.
    fn unpack_into(&self, core: &[u8], start: u64, n: u64, out: &mut Vec<u8>) -> Result<()> {
        let end = start.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "packed unpack length",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: end,
                available: self.logical_len,
            });
        }
        if let Ok(cap) = usize::try_from(n) {
            out.reserve(cap);
        }
        if n == 0 {
            return Ok(());
        }
        if self.k == 1 {
            let count = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
                what: "packed unpack length",
            })?;
            out.resize(out.len() + count, self.alphabet[0]);
            return Ok(());
        }
        if self.k == 0 {
            // Identity: the payload is the source verbatim, so a range is a slice.
            let a = self.header_len
                + usize::try_from(start).map_err(|_| EkbloomError::Overflow {
                    what: "packed unpack offset",
                })?;
            let b = a + usize::try_from(n).map_err(|_| EkbloomError::Overflow {
                what: "packed unpack length",
            })?;
            out.extend_from_slice(&core[a..b]);
            return Ok(());
        }
        let payload = self.payload(core);
        let bits = self.bits;
        let mask = u32::from(bit_mask(bits));
        for j in 0..n {
            let bit = u128::from(start + j) * u128::from(bits);
            let byte = (bit >> 3) as usize;
            let off = (bit & 7) as u32;
            let lo = u32::from(payload[byte]);
            let idx = if off + u32::from(bits) <= 8 {
                (lo >> off) & mask
            } else {
                let hi = u32::from(*payload.get(byte + 1).unwrap_or(&0));
                ((lo >> off) | (hi << (8 - off))) & mask
            };
            let s = self
                .alphabet
                .get(idx as usize)
                .copied()
                .filter(|_| (idx as usize) < self.k)
                .ok_or_else(|| EkbloomError::BadSection {
                    reason: format!("packed index {idx} outside alphabet of size {}", self.k),
                })?;
            out.push(s);
        }
        Ok(())
    }
}

impl OrMachine for Pack {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_PACK
    }

    #[inline]
    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        PackContext::from_core(core)?;
        Ok(Snapshot::new(0, 0))
    }

    #[inline]
    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let ctx = PackContext::from_core(core)?;
        ctx.reveal(core, omega)
    }

    #[inline]
    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let ctx = PackContext::from_core(core)?;
        ctx.peek(core, omega)
    }

    /// Encode `source`, discovering the alphabet and choosing the smallest fixed
    /// width from `{1, 2, 4, 8}` that fits; when packing does not beat raw the
    /// identity rendering (`bits = 8`, `k = 0`, payload `= source`) is used.
    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        let layout = Layout::discover(source);
        if layout.k == 0 || layout.k == 256 {
            // Empty source, or every byte value present: the identity rendering.
            return Ok(identity_core(source));
        }
        let bits = if layout.k <= 2 {
            1
        } else if layout.k <= 4 {
            2
        } else if layout.k <= 16 {
            4
        } else {
            8
        };
        let packed = pack_core(source, bits, &layout)?;
        if packed.len() < source.len() {
            Ok(packed)
        } else {
            Ok(identity_core(source))
        }
    }

    /// Validate that `core` is a complete packed core declaring exactly
    /// `logical_len` symbols.
    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = PackContext::from_core(core)?;
        if ctx.logical_len != logical_len {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "packed core declares {} symbols, expected {logical_len}",
                    ctx.logical_len
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_and_identity_renderings() {
        let constant = PACK.encode(&[7u8; 100]).unwrap();
        // bits=1, k=1, one alphabet byte, N, zero payload.
        assert_eq!(constant.len(), ALPHABET_OFFSET + 1 + LEN_TRAILER);
        assert_eq!(constant[0], 1);
        assert_eq!(constant[1], 1);
        assert_eq!(constant[2], 7);
        let all = (0u8..=255).collect::<Vec<_>>();
        let core = PACK.encode(&all).unwrap();
        assert_eq!(core.len(), ALPHABET_OFFSET + LEN_TRAILER + all.len());
        assert_eq!(core[0], 8);
        assert_eq!(core[1], 0);
    }

    #[test]
    fn bit_mask_covers_every_width() {
        assert_eq!(bit_mask(1), 1);
        assert_eq!(bit_mask(2), 3);
        assert_eq!(bit_mask(4), 15);
        assert_eq!(bit_mask(8), 255);
        assert_eq!(bit_mask(3), 7);
    }
}
