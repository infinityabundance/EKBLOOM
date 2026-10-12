//! The authoritative byte machines.
//!
//! EKBLOOM 0.3 ships four concrete machines. `OrANS-NL2` (`machine_id = 1`) is a
//! deterministic, exactly invertible state-*expansion* machine (`V = N`); see
//! `docs/DESIGN.md` §1 for the frozen contract. `OrANS-RANS0` (`machine_id = 2`)
//! additionally *compresses* skewed sources (`V < N`); see `docs/CODEC.md` and
//! [`crate::machine_rans`]. `OrANS-Store` (`machine_id = 3`) is the identity
//! (verbatim) codec, `encode(source) = source` and `reveal = core[cursor]`, right
//! at `V = N` with no model header; the `auto` representation policy falls back
//! to it whenever order-0 rANS cannot shrink the source, so the base
//! representation is never larger than raw. See [`crate::machine_store`].
//! `OrANS-RANS0I` (`machine_id = 4`) is the **interleaved** order-0 rANS: the same
//! model and `V < N` as machine 2, but `K` independent lane states whose per-group
//! steps are independent, so decode throughput escapes machine 2's serial chain.
//! See [`crate::machine_rans_i`].

use crate::error::{EkbloomError, Result};
use crate::machine_rans::{RansContext, RANS0};
use crate::machine_rans_i::{RansIContext, RANS0I};
use crate::machine_store::STORE;
use crate::state::Snapshot;

/// Machine identifier for `OrANS-NL2`.
pub const MACHINE_NL2: u32 = 1;

/// Machine identifier for `OrANS-RANS0`.
pub const MACHINE_RANS0: u32 = 2;

/// Machine identifier for `OrANS-Store` (the identity / verbatim codec).
pub const MACHINE_STORE: u32 = 3;

/// Machine identifier for `OrANS-RANS0I` (interleaved order-0 rANS).
pub const MACHINE_RANS0I: u32 = 4;

const fn build_sbox() -> [u8; 256] {
    let mut s = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        let v = i as u8;
        // Linear over GF(2)^8 by polynomial 1 + x + x^3, invertible.
        s[i] = v ^ (v << 1) ^ (v << 3);
        i += 1;
    }
    s
}

/// Fixed nonlinear byte permutation `S`. Bijective (verified in tests).
pub static SBOX: [u8; 256] = build_sbox();

/// A reproducible, resumable symbol-generating machine.
pub trait OrMachine: Send + Sync {
    /// Stable machine identifier stored in the `.ekb` header.
    fn machine_id(&self) -> u32;

    /// The legal initial executable state `Omega_0` for `core`.
    ///
    /// Machines whose genesis state depends only on the machine ignore `core`
    /// (machine 1 returns `(0, 0)`); a machine whose model is stored in the core
    /// header (machine 2) parses it here.
    fn init(&self, core: &[u8]) -> Result<Snapshot>;

    /// Reveal one symbol and advance `omega` by one step.
    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8>;

    /// The symbol that would be revealed next, without advancing.
    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8>;

    /// Encode a source into its visible core.
    fn encode(&self, source: &[u8]) -> Result<Vec<u8>>;

    /// Validate that `core` can produce `logical_len` symbols.
    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()>;

    /// Whether a *complete* resumable snapshot of this machine requires its
    /// per-lane state (`lanes`/`lane`), i.e. `(state, cursor)` alone cannot
    /// resume.
    ///
    /// The single-state machines (`machine_id = 1|2|3`) return `false`; the
    /// interleaved order-0 rANS machine (`OrANS-RANS0I`, `machine_id = 4`)
    /// returns `true`. Manifested successor/restart records (regional tables,
    /// block indexes, checkpoints) consult this predicate to decide whether to
    /// store the complete wide [`Snapshot`] rather than the compact
    /// `(state, cursor)` form, so no site needs to hardcode a machine id.
    fn snapshot_needs_lanes(&self) -> bool {
        false
    }

    /// Decode `logical_len` symbols from `core` (authoritative reference path).
    fn decode(&self, core: &[u8], logical_len: u64) -> Result<Vec<u8>> {
        self.check_core(core, logical_len)?;
        let mut omega = self.init(core)?;
        let mut out = Vec::with_capacity(logical_len as usize);
        for _ in 0..logical_len {
            out.push(self.reveal(core, &mut omega)?);
        }
        Ok(out)
    }

    /// The full state log `[x_0, ..., x_n]` for `core` and `logical_len`.
    ///
    /// For machine 1 this is simultaneously the generation-order history and the
    /// logical-position projection. Machines may override this to parse their
    /// model once rather than once per revealed symbol.
    fn state_log(&self, core: &[u8], logical_len: u64) -> Result<Vec<u32>> {
        self.check_core(core, logical_len)?;
        let mut omega = self.init(core)?;
        let n = usize::try_from(logical_len).map_err(|_| EkbloomError::Overflow {
            what: "state log length",
        })?;
        let mut log = Vec::with_capacity(n + 1);
        log.push(omega.state);
        for _ in 0..logical_len {
            self.reveal(core, &mut omega)?;
            log.push(omega.state);
        }
        Ok(log)
    }
}

/// The `OrANS-NL2` machine.
///
/// Genesis `Omega_0 = (0, 0)`. Step at position `i` with `h = x>>8`, `l = x&0xFF`:
/// `s = S[h] ^ l ^ core[p]`, then `x' = (l<<8)|s`, `p' = p+1`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Nl2;

impl Nl2 {
    /// Construct the machine.
    pub const fn new() -> Self {
        Nl2
    }
}

/// The single process-wide `OrANS-NL2` instance.
pub static NL2: Nl2 = Nl2;

#[inline]
fn step_state(x: u32, s: u8) -> u32 {
    ((x & 0xFF) << 8) | u32::from(s)
}

impl OrMachine for Nl2 {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_NL2
    }

    #[inline]
    fn init(&self, _core: &[u8]) -> Result<Snapshot> {
        Ok(Snapshot::new(0, 0))
    }

    #[inline]
    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let p = omega.cursor;
        let r = *core.get(p as usize).ok_or(EkbloomError::CoreTooShort {
            needed: p + 1,
            available: core.len() as u64,
        })?;
        let h = (omega.state >> 8) as u8;
        let l = (omega.state & 0xFF) as u8;
        let s = SBOX[h as usize] ^ l ^ r;
        omega.state = step_state(omega.state, s);
        omega.cursor = p + 1;
        Ok(s)
    }

    #[inline]
    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let p = omega.cursor;
        let r = *core.get(p as usize).ok_or(EkbloomError::CoreTooShort {
            needed: p + 1,
            available: core.len() as u64,
        })?;
        let h = (omega.state >> 8) as u8;
        let l = (omega.state & 0xFF) as u8;
        Ok(SBOX[h as usize] ^ l ^ r)
    }

    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        let mut x: u32 = 0;
        let mut core = Vec::with_capacity(source.len());
        for &s in source {
            let h = (x >> 8) as u8;
            let l = (x & 0xFF) as u8;
            core.push(s ^ SBOX[h as usize] ^ l);
            x = step_state(x, s);
        }
        Ok(core)
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        if logical_len > core.len() as u64 {
            return Err(EkbloomError::CoreTooShort {
                needed: logical_len,
                available: core.len() as u64,
            });
        }
        Ok(())
    }
}

/// A prepared, validated decoder context for one core.
///
/// It is built once per bulk decode and then reveals symbols without repeating
/// per-symbol setup. For a machine whose `reveal` is already a single cheap step
/// (machine 1) it simply delegates back to the machine; for a machine whose step
/// needs a parsed model header (machine 2) it carries the validated frequencies,
/// prefix sums and the direct `4096`-entry symbol table so the parse is paid
/// **once** rather than once per symbol.
///
/// The revealed state/cursor sequence is identical to calling `OrMachine::reveal`
/// repeatedly, so every byte-exactness oracle in the crate still holds.
pub enum Prepared<'a> {
    /// Delegates each step to the machine's own `reveal`/`peek` (machine 1).
    Delegated(&'a dyn OrMachine),
    /// A prepared `OrANS-RANS0` decoder context (machine 2).
    Rans(Box<RansContext>),
    /// A prepared `OrANS-RANS0I` interleaved decoder context (machine 4).
    RansI(Box<RansIContext>),
}

impl Prepared<'_> {
    /// Reveal one symbol, advancing `omega` by one machine step.
    #[inline]
    pub fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        match self {
            Prepared::Delegated(machine) => machine.reveal(core, omega),
            Prepared::Rans(ctx) => ctx.reveal(core, omega),
            Prepared::RansI(ctx) => ctx.reveal(core, omega),
        }
    }

    /// The symbol that would be revealed next, without advancing.
    #[inline]
    pub fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        match self {
            Prepared::Delegated(machine) => machine.peek(core, omega),
            Prepared::Rans(ctx) => ctx.peek(core, omega),
            Prepared::RansI(ctx) => ctx.peek(core, omega),
        }
    }

    /// Advance `omega` by exactly `n` reveal steps, discarding the symbols.
    pub fn skip(&self, core: &[u8], omega: &mut Snapshot, n: u64) -> Result<()> {
        match self {
            Prepared::RansI(ctx) => ctx.skip(core, omega, n),
            _ => {
                for _ in 0..n {
                    self.reveal(core, omega)?;
                }
                Ok(())
            }
        }
    }

    /// Reveal `n` symbols from `omega`, appending them to `out`.
    pub fn decode_into(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if let Prepared::RansI(ctx) = self {
            return ctx.decode_into(core, omega, n, out);
        }
        if let Ok(cap) = usize::try_from(n) {
            out.reserve(cap);
        }
        for _ in 0..n {
            out.push(self.reveal(core, omega)?);
        }
        Ok(())
    }
}

/// Build a reusable [`Prepared`] decoder context for `core` with `machine`.
///
/// Dispatches on the machine identifier: machine 2 parses and validates its model
/// header once and builds the direct symbol table, while any other machine (in
/// particular machine 1) keeps its own per-step `reveal` semantics unchanged.
pub fn prepare<'a>(machine: &'a dyn OrMachine, core: &[u8]) -> Result<Prepared<'a>> {
    match machine.machine_id() {
        MACHINE_RANS0 => Ok(Prepared::Rans(Box::new(RansContext::from_core(core)?))),
        MACHINE_RANS0I => Ok(Prepared::RansI(Box::new(RansIContext::from_core(core)?))),
        _ => Ok(Prepared::Delegated(machine)),
    }
}

/// Look up a machine by identifier.
pub fn machine_by_id(id: u32) -> Option<&'static dyn OrMachine> {
    match id {
        MACHINE_NL2 => Some(&NL2),
        MACHINE_RANS0 => Some(&RANS0),
        MACHINE_STORE => Some(&STORE),
        MACHINE_RANS0I => Some(&RANS0I),
        _ => None,
    }
}

/// Compute the full state log `[x_0, ..., x_n]` for `core` and `logical_len`
/// using `machine`.
pub fn state_log_with(machine: &dyn OrMachine, core: &[u8], logical_len: u64) -> Result<Vec<u32>> {
    machine.state_log(core, logical_len)
}

/// Record the complete resumable state `(x, p)` at the start of every logical
/// block of `block_bytes` symbols: entries `0 ..= floor(logical_len / block_bytes)`
/// at positions `k * block_bytes`.
///
/// This is the authoritative forward pass built on the machine's own `reveal`
/// (with a prepared context), used by [`crate::blocks::BlockIndex`]. It is a
/// read-only helper: no machine semantics are changed.
pub fn block_restarts_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    block_bytes: u32,
) -> Result<Vec<Snapshot>> {
    if block_bytes == 0 {
        return Err(EkbloomError::InvalidArgument {
            what: "block size must be non-zero".to_string(),
        });
    }
    machine.check_core(core, logical_len)?;
    let ctx = prepare(machine, core)?;
    let b = u64::from(block_bytes);
    let count = logical_len
        .checked_div(b)
        .and_then(|blocks| blocks.checked_add(1))
        .ok_or(EkbloomError::Overflow {
            what: "block restart count",
        })?;
    let capacity = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
        what: "block restart count",
    })?;
    let mut entries: Vec<Snapshot> = Vec::with_capacity(capacity);
    let mut omega = machine.init(core)?;
    entries.push(omega);
    let mut i: u64 = 0;
    while i < logical_len {
        ctx.reveal(core, &mut omega)?;
        i += 1;
        if i.is_multiple_of(b) {
            entries.push(omega);
        }
    }
    Ok(entries)
}

/// Compute the full state log `[x_0, ..., x_n]` for `core` and `logical_len`
/// with the authoritative machine 1.
///
/// Because generation order coincides with logical order for machine 1, this is
/// simultaneously the generation-order history and the logical-position projection.
pub fn state_log(core: &[u8], logical_len: u64) -> Result<Vec<u32>> {
    state_log_with(&NL2, core, logical_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbox_is_bijective() {
        let mut seen = [false; 256];
        for &v in SBOX.iter() {
            assert!(!seen[v as usize], "SBOX collision at {v}");
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }

    #[test]
    fn roundtrip_various() {
        let cases: [&[u8]; 6] = [
            b"",
            b"A",
            b"AB",
            b"hello, world",
            &[0u8; 300],
            &(0u8..=255).collect::<Vec<_>>(),
        ];
        for src in cases {
            let core = NL2.encode(src).unwrap();
            assert_eq!(core.len(), src.len());
            let out = NL2.decode(&core, src.len() as u64).unwrap();
            assert_eq!(&out[..], src, "roundtrip failed for {src:?}");
        }
    }

    #[test]
    fn genesis_matches_log0_zero() {
        assert_eq!(NL2.init(&[]).unwrap().state, 0);
    }

    #[test]
    fn state_is_last_two_symbols() {
        let src: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(7)).collect();
        let core = NL2.encode(&src).unwrap();
        let log = state_log(&core, src.len() as u64).unwrap();
        for i in 0..log.len() {
            let expected = match i {
                0 => 0u32,
                1 => u32::from(src[0]),
                _ => (u32::from(src[i - 2]) << 8) | u32::from(src[i - 1]),
            };
            assert_eq!(log[i], expected, "state log mismatch at {i}");
        }
    }

    #[test]
    fn peek_matches_reveal() {
        let src = b"peek test";
        let core = NL2.encode(src).unwrap();
        let mut omega = NL2.init(&core).unwrap();
        for &expect in src.iter() {
            let peeked = NL2.peek(&core, &omega).unwrap();
            assert_eq!(peeked, expect);
            let revealed = NL2.reveal(&core, &mut omega).unwrap();
            assert_eq!(revealed, expect);
        }
    }
}
