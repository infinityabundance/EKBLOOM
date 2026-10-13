//! `OrANS-RANS0W` (`machine_id = 7`): order-0 range-ANS with **16-bit
//! renormalisation input words**.
//!
//! Machine 2 (`OrANS-RANS0`) renormalises in 8-bit *bytes*; from a valid state its
//! renorm loop can run **twice** per decoded symbol (`docs/WINDOW.md` §1). This
//! machine keeps the identical order-0 model and the identical fused
//! `slot -> (freq, cum, symbol)` table but shifts the decoder state in **16-bit
//! words**, so a single step renormalises in **at most one** iteration. It is a
//! decode-throughput variant: the encoded core carries the same model header and
//! (for the same source) essentially the same payload length — it changes the
//! *shape* of the renorm step, not the compression.
//!
//! # Core layout (`core = encode(source)`)
//!
//! ```text
//! [freq: 256 * u16 LE = 512 B][initial_state: u32 LE][renorm words: u16 LE ...]
//! ```
//!
//! # State bound
//!
//! The word-renorm lower bound is `RANS_W_L = 1 << 15` (not machine 2's `1 << 23`),
//! so a genuine encoder keeps every state in `[RANS_W_L, RANS_W_L << 16)` which
//! fits a `u32`. The per-symbol input bound follows the standard argument: the
//! pre-renorm successor is `x' = floor(x / 2^12) * f + r >= RANS_W_L >> 12 = 2^3`,
//! so **one** 16-bit read gives `x' << 16 >= 2^19 > RANS_W_L` — the renorm loop
//! stops after at most one word (two bytes). The encoded-window byte bound is
//! therefore `2 * (steps + symbols) + margin`, identical in bytes to machine 2 but
//! with half the renorm iterations.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_RANS0W};
use crate::machine_rans::{
    Model, RansContext, HEADER_BYTES, M, MAX_STEPS_PER_BYTE, MODEL_BYTES, RANS_L, SCALE_BITS,
    SYMBOLS,
};
use crate::state::Snapshot;

/// Lower bound of the word-renorm ANS state.
pub(crate) const RANS_W_L: u32 = 1 << 15;

/// Exclusive upper bound of a legal word-renorm coder state: a genuine encoder
/// keeps every state in `[RANS_W_L, RANS_W_L << 16)`, which fits a `u32`.
pub(crate) const RANS_W_X_MAX: u32 = RANS_W_L << 16;

/// Maximum number of renormalisation **bytes** a single word-renorm step may
/// consume (one 16-bit word).
pub const RENORM_MAX_BYTES_PER_STEP_W: u64 = 2;

/// Safety margin for the word-renorm bounded window (mirrors machine 2's).
pub const WINDOW_MARGIN_W: u64 = 64;

/// Whether `x` is a legal word-renorm coder state (`[RANS_W_L, RANS_W_L << 16)`).
#[inline]
pub(crate) fn valid_state_w(x: u32) -> bool {
    (RANS_W_L..RANS_W_X_MAX).contains(&x)
}

/// Upper bound on the encoded bytes a bounded window must hold to run `steps`
/// positioning steps plus `symbols` output symbols from a valid word-renorm
/// restart state, with saturating arithmetic.
#[inline]
pub fn encoded_window_bound_w(steps: u64, symbols: u64) -> u64 {
    steps
        .saturating_add(symbols)
        .saturating_mul(RENORM_MAX_BYTES_PER_STEP_W)
        .saturating_add(WINDOW_MARGIN_W)
}

/// The `OrANS-RANS0W` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rans0W;

/// The single process-wide `OrANS-RANS0W` instance.
pub static RANS0W: Rans0W = Rans0W;

/// A validated, reusable word-renorm decoder context for one core: machine 2's
/// validated model and fused `slot` table plus the word-renorm step geometry.
pub struct RansWContext {
    table: RansContext,
}

impl RansWContext {
    /// Parse and validate `core`'s model header (exactly as machine 2).
    pub fn from_core(core: &[u8]) -> Result<RansWContext> {
        Ok(RansWContext {
            table: RansContext::from_core(core)?,
        })
    }

    /// The model-header length in bytes: the minimal legal encoded cursor.
    #[inline]
    pub fn header_bytes(&self) -> u64 {
        HEADER_BYTES as u64
    }

    /// One word-renorm decoder step on `omega`.
    #[inline]
    pub fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        self.step(core, omega)
    }

    /// The symbol that would be revealed next, without advancing `omega`.
    #[inline]
    pub fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let mut probe = *omega;
        self.step(core, &mut probe)
    }

    #[inline]
    fn step(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let x = omega.state;
        let slot = x & (M - 1);
        let e = self.table.table()[slot as usize];
        let mut nx = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
        let mut p = omega.cursor;
        if nx < RANS_W_L {
            let word = read_word(core, p)?;
            nx = (nx << 16) | u32::from(word);
            p += 2;
        }
        omega.state = nx;
        omega.cursor = p;
        Ok((e >> 24) as u8)
    }

    /// Advance `omega` by exactly `n` steps, discarding the symbols.
    pub fn skip(&self, core: &[u8], omega: &mut Snapshot, n: u64) -> Result<()> {
        let table = self.table.table();
        let mut x = omega.state;
        let mut p = omega.cursor;
        for _ in 0..n {
            let slot = x & (M - 1);
            let e = table[slot as usize];
            let mut nx = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
            if nx < RANS_W_L {
                let word = read_word(core, p)?;
                nx = (nx << 16) | u32::from(word);
                p += 2;
            }
            x = nx;
        }
        omega.state = x;
        omega.cursor = p;
        Ok(())
    }

    /// Decode exactly `out.len()` symbols from `omega` into `out`.
    pub fn decode_into_slice(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        out: &mut [u8],
    ) -> Result<()> {
        let table = self.table.table();
        let mut x = omega.state;
        let mut p = omega.cursor;
        for byte in out.iter_mut() {
            let slot = x & (M - 1);
            let e = table[slot as usize];
            let mut nx = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
            if nx < RANS_W_L {
                let word = read_word(core, p)?;
                nx = (nx << 16) | u32::from(word);
                p += 2;
            }
            x = nx;
            *byte = (e >> 24) as u8;
        }
        omega.state = x;
        omega.cursor = p;
        Ok(())
    }

    /// Reveal `n` symbols, appending them to `out`.
    pub fn decode_into(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let n = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
            what: "rans word decode length",
        })?;
        crate::resource::checked_default(n as u64)?;
        let start = out.len();
        let end = start.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "rans word decode buffer",
        })?;
        out.resize(end, 0);
        self.decode_into_slice(core, omega, &mut out[start..end])
    }
}

/// Read one 16-bit little-endian word at byte offset `p`.
#[inline]
fn read_word(core: &[u8], p: u64) -> Result<u16> {
    let a = core
        .get(p as usize)
        .copied()
        .ok_or(EkbloomError::CoreTooShort {
            needed: p.saturating_add(1),
            available: core.len() as u64,
        })?;
    let b = core
        .get(p as usize + 1)
        .copied()
        .ok_or(EkbloomError::CoreTooShort {
            needed: p.saturating_add(2),
            available: core.len() as u64,
        })?;
    Ok(u16::from_le_bytes([a, b]))
}

impl Rans0W {
    /// The genesis snapshot `(initial_state, HEADER_BYTES)`.
    fn genesis(core: &[u8]) -> Snapshot {
        Snapshot::new(initial_state(core), HEADER_BYTES as u64)
    }

    /// The step budget for `core` (a crafted stream is cut off, never spun).
    fn step_cap(core: &[u8]) -> u64 {
        (core.len() as u64)
            .saturating_mul(MAX_STEPS_PER_BYTE)
            .saturating_add(1 << 20)
    }
}

/// The encoder's final word-renorm state stored in the header.
fn initial_state(core: &[u8]) -> u32 {
    u32::from_le_bytes([
        core[MODEL_BYTES],
        core[MODEL_BYTES + 1],
        core[MODEL_BYTES + 2],
        core[MODEL_BYTES + 3],
    ])
}

/// Encode `source` into an `OrANS-RANS0W` core (word-renorm order-0 rANS).
pub fn encode(source: &[u8]) -> Vec<u8> {
    let model = Model::from_source(source);
    let mut x: u32 = RANS_W_L;
    let mut emitted: Vec<u16> = Vec::new();
    for &sym in source.iter().rev() {
        let f = u64::from(model.freq[sym as usize]);
        let c = u64::from(model.cum[sym as usize]);
        // Standard b-bit renorm threshold with `b = 16`.
        let x_max = (u64::from(RANS_W_L >> SCALE_BITS) << 16) * f;
        let mut xu = u64::from(x);
        while xu >= x_max {
            emitted.push((xu & 0xFFFF) as u16);
            xu >>= 16;
        }
        x = (((xu / f) << SCALE_BITS) + (xu % f) + c) as u32;
    }

    let mut core = Vec::with_capacity(HEADER_BYTES + 2 * emitted.len());
    for &f in model.freq.iter() {
        core.extend_from_slice(&f.to_le_bytes());
    }
    core.extend_from_slice(&x.to_le_bytes());
    // Reverse word emission order so the decoder reads words forward.
    for &w in emitted.iter().rev() {
        core.extend_from_slice(&w.to_le_bytes());
    }
    core
}

impl OrMachine for Rans0W {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_RANS0W
    }

    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        Model::from_header(core)?;
        Ok(Self::genesis(core))
    }

    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let ctx = RansWContext::from_core(core)?;
        ctx.reveal(core, omega)
    }

    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let ctx = RansWContext::from_core(core)?;
        ctx.peek(core, omega)
    }

    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        Ok(encode(source))
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = RansWContext::from_core(core)?;
        let mut omega = Self::genesis(core);
        let cap = Self::step_cap(core);
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
        crate::resource::checked_default(logical_len)?;
        let ctx = RansWContext::from_core(core)?;
        let mut omega = Self::genesis(core);
        let cap = Self::step_cap(core);
        let mut out: Vec<u8> = Vec::with_capacity(logical_len as usize);
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

    fn state_log(&self, core: &[u8], logical_len: u64) -> Result<Vec<u32>> {
        crate::resource::checked_default(logical_len.saturating_mul(4).saturating_add(4))?;
        let ctx = RansWContext::from_core(core)?;
        let mut omega = Self::genesis(core);
        let cap = Self::step_cap(core);
        let mut log: Vec<u32> = Vec::with_capacity(logical_len as usize + 1);
        log.push(omega.state);
        let mut steps = 0u64;
        while steps < logical_len {
            if steps >= cap {
                return Err(EkbloomError::NonTermination);
            }
            ctx.reveal(core, &mut omega)?;
            log.push(omega.state);
            steps += 1;
        }
        Ok(log)
    }
}

/// The model-header size in bytes (re-exported for windowed-read plumbing).
pub const RANS_W_MODEL_HEADER_BYTES: u64 = HEADER_BYTES as u64;

/// The number of symbol slots (for symmetry with machine 2's declaration).
pub const RANS_W_SYMBOLS: usize = SYMBOLS;

/// Machine 2's byte-renorm lower bound, referenced so the two machines' proofs
/// live side by side in one module.
pub const RANS_W_REFERENCE_L: u32 = RANS_L;

#[cfg(test)]
mod tests {
    use super::*;

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

    fn skewed(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed;
        let alpha = *b"abcd";
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = (s >> 33) % 100;
            let idx = if r < 60 {
                0
            } else if r < 85 {
                1
            } else if r < 95 {
                2
            } else {
                3
            };
            out.push(alpha[idx]);
        }
        out
    }

    #[test]
    fn roundtrip_every_category() {
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![7],
            vec![0, 0, 0, 0],
            b"hello, world".to_vec(),
            (0u8..=255).collect(),
            skewed(1, 1000),
            lcg(2, 4096),
        ];
        for src in cases {
            let core = encode(&src);
            assert!(RANS_W_L <= initial_state(&core));
            let out = RANS0W.decode(&core, src.len() as u64).unwrap();
            assert_eq!(out, src, "roundtrip failed for {} bytes", src.len());
        }
    }

    #[test]
    fn state_stays_in_word_range() {
        let src = skewed(3, 5000);
        let core = encode(&src);
        assert!(valid_state_w(initial_state(&core)));
        let log = RANS0W.state_log(&core, src.len() as u64).unwrap();
        for &x in &log {
            assert!(valid_state_w(x), "state {x} out of word-renorm range");
        }
    }

    #[test]
    fn header_errors_cleanly() {
        assert!(RansWContext::from_core(&[]).is_err());
        assert!(RansWContext::from_core(&[0u8; 100]).is_err());
    }
}
