//! `OrANS-RANS0` (`machine_id = 2`): a static order-0 range-ANS byte codec.
//!
//! Where `OrANS-NL2` (`machine_id = 1`) consumes exactly one core byte per
//! revealed symbol (`V = N`), this machine *compresses*: the visible core may be
//! smaller than the logical object (`V < N`) for low-entropy (skewed) sources.
//! It is a 32-bit range ANS coder over an order-0 byte model, with byte
//! renormalisation. For high-entropy (uniform) sources it offers no win and the
//! core carries the fixed model header as overhead (`V > N` by a constant).
//!
//! # Core layout (`core = encode(source)`)
//!
//! ```text
//! [freq: 256 * u16 little-endian = 512 B][initial_state: u32 LE][renorm bytes ...]
//! ```
//!
//! `freq[s]` is the scaled frequency of byte `s` (each `>= 1`, summing to
//! `M = 1 << 12 = 4096`); `cum[s]` is the prefix sum of `freq[0..s]`. The model
//! is a pure function of the *source* and is stored in the core so decoding is
//! self-contained. `initial_state` is the final coder state. The renormalisation
//! bytes are stored in reverse emission order, so the decoder reads them forward
//! from the cursor immediately after the header.
//!
//! # Semantics
//!
//! Genesis `Omega_0 = (initial_state, HEADER_BYTES)`. The cursor is the byte
//! offset into `core` — it diverges from the logical position (unlike machine 1),
//! so the state-only projection is not by itself resumable. One decoder step
//! reveals the next symbol and consumes zero or more renormalisation bytes.
//! `decode(encode(source)) == source` is exact and exhaustively tested.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_RANS0};
use crate::state::Snapshot;

/// Lower bound of the ANS state.
const RANS_L: u32 = 1 << 23;
/// Model scale: frequencies sum to `1 << SCALE_BITS`.
const SCALE_BITS: u32 = 12;
/// Model total.
const M: u32 = 1 << SCALE_BITS;
/// Number of byte symbols.
const SYMBOLS: usize = 256;
/// Model header size: 256 little-endian `u16` frequencies.
const MODEL_BYTES: usize = SYMBOLS * 2;
/// Header size: model ++ initial state `u32`.
const HEADER_BYTES: usize = MODEL_BYTES + 4;
/// Upper bound on decode steps per core byte. A genuine order-0 stream consumes
/// at most ~86 steps per byte (the entropy floor for the maximally skewed
/// supported model); this guards against crafted streams that renormalise too
/// rarely and would otherwise spin.
const MAX_STEPS_PER_BYTE: u64 = 4096;

/// The `OrANS-RANS0` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rans0;

/// The single process-wide `OrANS-RANS0` instance.
pub static RANS0: Rans0 = Rans0;

/// A derived order-0 model: scaled frequencies and prefix sums.
struct Model {
    freq: [u16; SYMBOLS],
    cum: [u32; SYMBOLS],
}

impl Model {
    /// Build the scaled model for `source` by largest-remainder scaling.
    ///
    /// Every symbol is reserved a frequency of `1`, then the remaining
    /// `M - SYMBOLS` mass is distributed proportionally to the clamped counts,
    /// with any rounding remainder handed to the largest fractional parts. The
    /// result always has `freq[s] >= 1` and sums to exactly `M`.
    fn from_source(source: &[u8]) -> Model {
        let mut count = [0u64; SYMBOLS];
        for &b in source {
            count[b as usize] += 1;
        }
        let clamped: [u64; SYMBOLS] = std::array::from_fn(|i| count[i].max(1));
        let total: u64 = clamped.iter().sum();
        let budget: u64 = u64::from(M) - SYMBOLS as u64;

        let mut rem = [0u64; SYMBOLS];
        let mut frac = [0u64; SYMBOLS];
        let mut assigned: u64 = 0;
        for (i, &c) in clamped.iter().enumerate() {
            let num = u128::from(c) * u128::from(budget);
            rem[i] = (num / u128::from(total)) as u64;
            frac[i] = (num % u128::from(total)) as u64;
            assigned += rem[i];
        }
        // The fractional parts sum to `total * (budget - assigned)`, and each is
        // `< total`, so at most `SYMBOLS - 1` units remain to hand out.
        let leftover = budget - assigned;
        let mut order: Vec<usize> = (0..SYMBOLS).collect();
        order.sort_by(|&a, &b| frac[b].cmp(&frac[a]).then(a.cmp(&b)));
        for &i in order.iter().take(leftover as usize) {
            rem[i] += 1;
        }

        let mut freq = [0u16; SYMBOLS];
        let mut cum = [0u32; SYMBOLS];
        let mut acc: u32 = 0;
        for ((slot_freq, slot_cum), &r) in freq.iter_mut().zip(cum.iter_mut()).zip(rem.iter()) {
            let f = (r + 1) as u16;
            *slot_freq = f;
            *slot_cum = acc;
            acc += u32::from(f);
        }
        debug_assert_eq!(acc, M);
        Model { freq, cum }
    }

    /// Parse and validate the model stored in `core`'s header.
    fn from_header(core: &[u8]) -> Result<Model> {
        if core.len() < HEADER_BYTES {
            return Err(EkbloomError::Truncated {
                needed: HEADER_BYTES as u64,
                available: core.len() as u64,
            });
        }
        let mut freq = [0u16; SYMBOLS];
        let mut sum: u32 = 0;
        for (i, f) in freq.iter_mut().enumerate() {
            let v = u16::from_le_bytes([core[i * 2], core[i * 2 + 1]]);
            *f = v;
            sum += u32::from(v);
        }
        if sum != M {
            return Err(EkbloomError::BadSection {
                reason: format!("rans model frequencies sum to {sum}, expected {M}"),
            });
        }
        if freq.contains(&0) {
            return Err(EkbloomError::BadSection {
                reason: "rans model contains a zero frequency".to_string(),
            });
        }
        let mut cum = [0u32; SYMBOLS];
        let mut acc: u32 = 0;
        for (slot_cum, &f) in cum.iter_mut().zip(freq.iter()) {
            *slot_cum = acc;
            acc += u32::from(f);
        }
        Ok(Model { freq, cum })
    }

    /// The initial coder state stored in the header.
    fn initial_state(core: &[u8]) -> u32 {
        u32::from_le_bytes([
            core[MODEL_BYTES],
            core[MODEL_BYTES + 1],
            core[MODEL_BYTES + 2],
            core[MODEL_BYTES + 3],
        ])
    }
}

/// A validated, reusable order-0 decoder context for one core.
///
/// The model header is parsed and validated **once** (frequencies summing to `M`,
/// every frequency non-zero), the prefix sums are derived, and a direct
/// `M`-entry `slot -> symbol` table is built so each decoder step is a table
/// lookup instead of a binary search over `cum`. Sequential decoders, history /
/// state-log construction, tile certification/reconstruction and the bloom
/// builders share one context rather than re-parsing the header per symbol.
///
/// The context is a pure function of the core header; it holds no cursor and is
/// immutable, so it may be shared across threads (it is `Send + Sync`).
pub struct RansContext {
    freq: [u16; SYMBOLS],
    cum: [u32; SYMBOLS],
    /// `table[slot]` is the unique symbol `s` with `cum[s] <= slot < cum[s] + freq[s]`.
    table: Vec<u8>,
}

impl RansContext {
    /// Parse and validate `core`'s model header, then build the direct table.
    ///
    /// Errors exactly as [`Model::from_header`]: a truncated header, frequencies
    /// that do not sum to `M`, or a zero frequency.
    pub fn from_core(core: &[u8]) -> Result<RansContext> {
        let model = Model::from_header(core)?;
        let mut table = vec![0u8; M as usize];
        for s in 0..SYMBOLS {
            let start = model.cum[s] as usize;
            let end = start + usize::from(model.freq[s]);
            table[start..end].fill(s as u8);
        }
        Ok(RansContext {
            freq: model.freq,
            cum: model.cum,
            table,
        })
    }

    /// One decoder step: resolve the symbol owning `x & 4095` from the table and
    /// advance the state, consuming zero or more renormalisation bytes.
    #[inline]
    fn step(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let x = omega.state;
        let slot = x & (M - 1);
        let s = self.table[slot as usize];
        let f = u64::from(self.freq[s as usize]);
        let c = u64::from(self.cum[s as usize]);
        let mut nx = f * u64::from(x >> SCALE_BITS) + u64::from(slot) - c;
        let mut p = omega.cursor;
        while nx < u64::from(RANS_L) {
            let idx = usize::try_from(p).map_err(|_| EkbloomError::Overflow {
                what: "rans cursor",
            })?;
            let byte = *core.get(idx).ok_or(EkbloomError::CoreTooShort {
                needed: p + 1,
                available: core.len() as u64,
            })?;
            nx = (nx << 8) | u64::from(byte);
            p += 1;
        }
        omega.state = nx as u32;
        omega.cursor = p;
        Ok(s)
    }

    /// Reveal one symbol against this prepared context.
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
}

impl Rans0 {
    /// The genesis snapshot `(initial_state, HEADER_BYTES)`.
    fn genesis(core: &[u8]) -> Snapshot {
        Snapshot::new(Model::initial_state(core), HEADER_BYTES as u64)
    }

    /// The step budget for `core`: `MAX_STEPS_PER_BYTE` per byte plus slack. A
    /// genuine stream never approaches it; a crafted one is cut off with
    /// [`EkbloomError::NonTermination`].
    fn step_cap(core: &[u8]) -> u64 {
        (core.len() as u64)
            .saturating_mul(MAX_STEPS_PER_BYTE)
            .saturating_add(1 << 20)
    }
}

impl OrMachine for Rans0 {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_RANS0
    }

    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        Model::from_header(core)?;
        Ok(Self::genesis(core))
    }

    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let ctx = RansContext::from_core(core)?;
        ctx.reveal(core, omega)
    }

    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let ctx = RansContext::from_core(core)?;
        ctx.peek(core, omega)
    }

    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        let model = Model::from_source(source);
        let mut x: u32 = RANS_L;
        let mut emitted: Vec<u8> = Vec::new();
        for &sym in source.iter().rev() {
            let f = u64::from(model.freq[sym as usize]);
            let c = u64::from(model.cum[sym as usize]);
            let x_max = u64::from((RANS_L >> SCALE_BITS) << 8) * f;
            while u64::from(x) >= x_max {
                emitted.push((x & 0xFF) as u8);
                x >>= 8;
            }
            let xu = u64::from(x);
            let next = ((xu / f) << SCALE_BITS) + (xu % f) + c;
            x = next as u32;
        }

        let mut core = Vec::with_capacity(HEADER_BYTES.saturating_add(emitted.len()));
        for &f in model.freq.iter() {
            core.extend_from_slice(&f.to_le_bytes());
        }
        core.extend_from_slice(&x.to_le_bytes());
        // Reverse emission order so the decoder reads renormalisation bytes
        // forward from the cursor.
        core.extend(emitted.iter().rev());
        Ok(core)
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = RansContext::from_core(core)?;
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
        let ctx = RansContext::from_core(core)?;
        let mut omega = Self::genesis(core);
        let cap = Self::step_cap(core);
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

    fn state_log(&self, core: &[u8], logical_len: u64) -> Result<Vec<u32>> {
        let ctx = RansContext::from_core(core)?;
        let mut omega = Self::genesis(core);
        let cap = Self::step_cap(core);
        let mut log: Vec<u32> = Vec::new();
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
