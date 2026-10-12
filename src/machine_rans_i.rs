//! `OrANS-RANS0I` (`machine_id = 4`): **interleaved** static order-0 range-ANS.
//!
//! Machine 2 (`OrANS-RANS0`) is a single 32-bit rANS state: its decode step is an
//! irreducible dependent chain `x -> slot -> table load -> multiply -> x'`, so a
//! byte can never be decoded before its predecessor. This machine keeps `K`
//! independent rANS states — lane `i % K` codes symbol `i` — so the K steps of one
//! group are **independent** and the CPU can overlap their table loads, multiplies
//! and adds (instruction-level parallelism), which is what makes the cold decode
//! competitive with a copy-based LZ4 block decompress.
//!
//! # Core layout (`core = encode(source)`)
//!
//! ```text
//! [freq: 256 * u16 LE = 512 B][K: u32 LE][K initial states: K * u32 LE][interleaved renorm bytes ...]
//! ```
//!
//! The model is byte-for-byte the machine-2 model (largest-remainder scaling,
//! `freq[s] >= 1`, `sum(freq) == M = 1 << 12`), so the same 512-byte header is
//! reused. `K ∈ [1, MAX_LANES]` (default `4`) is stored explicitly so a core is
//! self-describing and any K round-trips. The `K` initial states are the encoder's
//! final per-lane states. The renormalisation bytes are the encoder's whole
//! emission sequence **in reverse**, exactly as machine 2: the decoder reads them
//! forward from the cursor, and because the encoder emitted them lane-tagged in
//! descending symbol order, the forward read is the precise per-lane order the
//! decoder needs.
//!
//! # Semantics
//!
//! Genesis `Omega_0 = (lanes = initial states, cursor = header_len, lane = 0)`.
//! One `reveal` decodes one symbol on `lane`, renormalising from the shared core
//! cursor, then advances `lane` cyclically. The complete resumable state is
//! `(lane_states, cursor, lane)`; it lives in a [`Snapshot`] (see `state.rs`), so
//! the state log, checkpoints, blocks, tiles, blooms and the engine all work
//! through the machine-general code paths.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_RANS0I};
use crate::machine_rans::{
    Model, RansContext, M, MAX_STEPS_PER_BYTE, MODEL_BYTES, RANS_L, SCALE_BITS,
};
use crate::state::{Snapshot, MAX_LANES};

/// Default lane count for `encode`.
pub const DEFAULT_K: usize = 4;
/// Smallest accepted lane count.
const MIN_K: usize = 1;
/// Number of bytes before the `K` field (`256 * u16` frequencies).
const K_FIELD_OFFSET: usize = MODEL_BYTES;
/// Number of bytes before the initial lane states (`freq` ++ `K`).
const STATES_OFFSET: usize = MODEL_BYTES + 4;

/// The interleaved `OrANS-RANS0I` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rans0I;

/// The single process-wide `OrANS-RANS0I` instance (encodes with [`DEFAULT_K`]).
pub static RANS0I: Rans0I = Rans0I;

impl Rans0I {
    /// Construct the machine.
    pub const fn new() -> Self {
        Rans0I
    }
}

/// The step budget for `core`: `MAX_STEPS_PER_BYTE` per byte plus slack. A genuine
/// stream never approaches it; a crafted one is cut off with
/// [`EkbloomError::NonTermination`].
fn step_cap(core: &[u8]) -> u64 {
    (core.len() as u64)
        .saturating_mul(MAX_STEPS_PER_BYTE)
        .saturating_add(1 << 20)
}

/// The validated, reusable interleaved decoder for one core: the machine-2 fused
/// `slot -> (freq, cum, symbol)` table plus the header geometry.
pub struct RansIContext {
    table: RansContext,
    k: usize,
    header_bytes: usize,
}

impl RansIContext {
    /// Parse and validate `core`'s model, `K` and lane states.
    pub fn from_core(core: &[u8]) -> Result<RansIContext> {
        if core.len() < STATES_OFFSET {
            return Err(EkbloomError::Truncated {
                needed: STATES_OFFSET as u64,
                available: core.len() as u64,
            });
        }
        let k = u32::from_le_bytes([
            core[K_FIELD_OFFSET],
            core[K_FIELD_OFFSET + 1],
            core[K_FIELD_OFFSET + 2],
            core[K_FIELD_OFFSET + 3],
        ]) as usize;
        if !(MIN_K..=MAX_LANES).contains(&k) {
            return Err(EkbloomError::BadSection {
                reason: format!("interleaved rANS lane count {k} out of range [1, {MAX_LANES}]"),
            });
        }
        let header_bytes = STATES_OFFSET + 4 * k;
        if core.len() < header_bytes {
            return Err(EkbloomError::Truncated {
                needed: header_bytes as u64,
                available: core.len() as u64,
            });
        }
        // Reuse machine 2's validated model parse and fused `slot` table.
        let table = RansContext::from_core(core)?;
        let initial = read_states(core, k);
        for &x in initial[..k].iter() {
            if !(RANS_L..RANS_L << 8).contains(&x) {
                return Err(EkbloomError::BadSection {
                    reason: format!("interleaved rANS lane state {x} out of range"),
                });
            }
        }
        Ok(RansIContext {
            table,
            k,
            header_bytes,
        })
    }

    /// The number of lanes `K`.
    #[inline]
    pub fn lanes(&self) -> usize {
        self.k
    }

    /// The genesis snapshot `(initial_states, header_len, lane 0)`.
    fn genesis(&self, core: &[u8]) -> Snapshot {
        let mut lanes = [0u32; MAX_LANES];
        lanes[..self.k].copy_from_slice(&read_states(core, self.k)[..self.k]);
        Snapshot::with_lanes(self.header_bytes as u64, lanes, 0)
    }

    /// One decoder step on `x` (a single lane), consuming renormalisation bytes
    /// from `p`. The whole step is 32-bit-safe exactly as machine 2's is.
    #[inline]
    fn step(&self, core: &[u8], x: u32, p: &mut u64) -> Result<(u8, u32)> {
        step_at(self.table.table(), x, core, p)
    }

    /// Reveal one symbol against this context, advancing `omega` by one step.
    #[inline]
    pub fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let lane = usize::from(omega.lane) % self.k;
        let (s, nx) = self.step(core, omega.lanes[lane], &mut omega.cursor)?;
        omega.lanes[lane] = nx;
        omega.lane = if lane + 1 == self.k {
            0
        } else {
            (lane + 1) as u8
        };
        omega.state = omega.lanes[usize::from(omega.lane)];
        Ok(s)
    }

    /// The symbol that would be revealed next, without advancing `omega`.
    #[inline]
    pub fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let mut probe = *omega;
        self.reveal(core, &mut probe)
    }

    /// Advance `omega` by exactly `n` steps, discarding the symbols (the prepared
    /// bounded block-decoder skip). Uses the same lane-grouped structure as the
    /// decoder so the skip keeps the ILP of the full-group path.
    pub fn skip(&self, core: &[u8], omega: &mut Snapshot, n: u64) -> Result<()> {
        let k = self.k;
        let table = self.table.table();
        let mut lanes = omega.lanes;
        let mut lane = usize::from(omega.lane) % k;
        let mut p = omega.cursor;
        let mut remaining = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
            what: "interleaved rans skip length",
        })?;

        while lane != 0 && remaining != 0 {
            let (_, nx) = step_at(table, lanes[lane], core, &mut p)?;
            lanes[lane] = nx;
            remaining -= 1;
            lane = if lane + 1 == k { 0 } else { lane + 1 };
        }
        while remaining >= k {
            let mut nx = [0u32; MAX_LANES];
            for t in 0..k {
                let x = lanes[t];
                let slot = x & (M - 1);
                let e = table[slot as usize];
                nx[t] = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
            }
            for t in 0..k {
                let mut v = nx[t];
                while v < RANS_L {
                    let b = match core.get(p as usize) {
                        Some(&b) => b,
                        None => {
                            return Err(EkbloomError::CoreTooShort {
                                needed: p + 1,
                                available: core.len() as u64,
                            })
                        }
                    };
                    v = (v << 8) | u32::from(b);
                    p += 1;
                }
                lanes[t] = v;
            }
            remaining -= k;
        }
        while remaining != 0 {
            let (_, nx) = step_at(table, lanes[lane], core, &mut p)?;
            lanes[lane] = nx;
            remaining -= 1;
            lane += 1;
            if lane == k {
                lane = 0;
            }
        }

        omega.lanes = lanes;
        omega.lane = lane as u8;
        omega.cursor = p;
        omega.state = lanes[lane];
        Ok(())
    }

    /// Decode exactly `out.len()` symbols — the tight interleaved decoder. A full
    /// group of `K` consecutive symbols visits every lane once, so the group's
    /// table lookups and state updates are K **independent** scalar chains (the
    /// ILP that breaks machine 2's serial dependency); only the shared renormalising
    /// cursor is ordered, and renormalisation is rare on skewed sources.
    pub fn decode_into_slice(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        out: &mut [u8],
    ) -> Result<()> {
        let k = self.k;
        let table = self.table.table();
        let mut lanes = omega.lanes;
        let mut lane = usize::from(omega.lane) % k;
        let mut p = omega.cursor;
        let n = out.len();
        let mut i = 0usize;

        // Align to lane 0 so full groups use the lanes directly.
        while lane != 0 && i < n {
            let (s, nx) = step_at(table, lanes[lane], core, &mut p)?;
            lanes[lane] = nx;
            out[i] = s;
            i += 1;
            lane = if lane + 1 == k { 0 } else { lane + 1 };
        }

        // Full groups of K independent lanes.
        while i + k <= n {
            let mut e = [0u32; MAX_LANES];
            let mut nx = [0u32; MAX_LANES];
            for t in 0..k {
                let x = lanes[t];
                let slot = x & (M - 1);
                let ei = table[slot as usize];
                e[t] = ei;
                nx[t] = (x >> SCALE_BITS) * (ei & 0x0FFF) + slot - ((ei >> SCALE_BITS) & 0x0FFF);
            }
            for v_slot in nx.iter_mut().take(k) {
                let mut v = *v_slot;
                while v < RANS_L {
                    let b = match core.get(p as usize) {
                        Some(&b) => b,
                        None => {
                            return Err(EkbloomError::CoreTooShort {
                                needed: p + 1,
                                available: core.len() as u64,
                            })
                        }
                    };
                    v = (v << 8) | u32::from(b);
                    p += 1;
                }
                *v_slot = v;
            }
            for t in 0..k {
                out[i + t] = (e[t] >> 24) as u8;
                lanes[t] = nx[t];
            }
            i += k;
        }

        // Trailing partial group.
        while i < n {
            let (s, v) = step_at(table, lanes[lane], core, &mut p)?;
            lanes[lane] = v;
            out[i] = s;
            i += 1;
            lane += 1;
            if lane == k {
                lane = 0;
            }
        }

        omega.lanes = lanes;
        omega.lane = lane as u8;
        omega.cursor = p;
        omega.state = lanes[lane];
        Ok(())
    }

    /// Reveal `n` symbols from `omega`, appending them to `out` (the prepared
    /// block decoder).
    pub fn decode_into(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let n = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
            what: "interleaved rans decode length",
        })?;
        let start = out.len();
        let end = start.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "interleaved rans decode buffer",
        })?;
        out.resize(end, 0);
        self.decode_into_slice(core, omega, &mut out[start..end])
    }
}

/// One interleaved decoder step on lane state `x`, reading renormalisation bytes
/// forward from `*p`.
#[inline]
fn step_at(table: &[u32], x: u32, core: &[u8], p: &mut u64) -> Result<(u8, u32)> {
    let slot = x & (M - 1);
    let e = table[slot as usize];
    let mut nx = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
    while nx < RANS_L {
        let b = match core.get(*p as usize) {
            Some(&b) => b,
            None => {
                return Err(EkbloomError::CoreTooShort {
                    needed: *p + 1,
                    available: core.len() as u64,
                })
            }
        };
        nx = (nx << 8) | u32::from(b);
        *p += 1;
    }
    Ok(((e >> 24) as u8, nx))
}

/// Read the `k` initial lane states from the core header.
fn read_states(core: &[u8], k: usize) -> [u32; MAX_LANES] {
    let mut lanes = [0u32; MAX_LANES];
    for (i, slot) in lanes.iter_mut().enumerate().take(k) {
        let o = STATES_OFFSET + 4 * i;
        *slot = u32::from_le_bytes([core[o], core[o + 1], core[o + 2], core[o + 3]]);
    }
    lanes
}

/// Encode `source` with `k` interleaved lanes.
///
/// Symbol `i` is coded in lane `i % K`. The encoder walks the symbols in reverse
/// (as rANS requires), renormalising each lane independently and appending the
/// emitted bytes to one shared stream, which is then stored reversed so the
/// decoder reads exactly those bytes forward.
pub fn encode_with_k(source: &[u8], k: usize) -> Result<Vec<u8>> {
    if !(MIN_K..=MAX_LANES).contains(&k) {
        return Err(EkbloomError::InvalidArgument {
            what: format!("interleaved rans lane count {k} out of range [1, {MAX_LANES}]"),
        });
    }
    let model = Model::from_source(source);
    let mut states = [RANS_L; MAX_LANES];
    let mut emitted: Vec<u8> = Vec::new();
    for i in (0..source.len()).rev() {
        let lane = i % k;
        let f = u64::from(model.freq[source[i] as usize]);
        let c = u64::from(model.cum[source[i] as usize]);
        let mut x = u64::from(states[lane]);
        let x_max = u64::from((RANS_L >> SCALE_BITS) << 8) * f;
        while x >= x_max {
            emitted.push((x & 0xFF) as u8);
            x >>= 8;
        }
        states[lane] = (((x / f) << SCALE_BITS) + (x % f) + c) as u32;
    }

    let header = STATES_OFFSET + 4 * k;
    let mut core = Vec::with_capacity(header.saturating_add(emitted.len()));
    for &f in model.freq.iter() {
        core.extend_from_slice(&f.to_le_bytes());
    }
    core.extend_from_slice(&(k as u32).to_le_bytes());
    for &x in states[..k].iter() {
        core.extend_from_slice(&x.to_le_bytes());
    }
    // Reverse emission order so the decoder reads renormalisation bytes forward.
    core.extend(emitted.iter().rev());
    Ok(core)
}

impl OrMachine for Rans0I {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_RANS0I
    }

    #[inline]
    fn snapshot_needs_lanes(&self) -> bool {
        true
    }

    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        let ctx = RansIContext::from_core(core)?;
        Ok(ctx.genesis(core))
    }

    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let ctx = RansIContext::from_core(core)?;
        ctx.reveal(core, omega)
    }

    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let ctx = RansIContext::from_core(core)?;
        ctx.peek(core, omega)
    }

    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        encode_with_k(source, DEFAULT_K)
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = RansIContext::from_core(core)?;
        let mut omega = ctx.genesis(core);
        let cap = step_cap(core);
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
        let ctx = RansIContext::from_core(core)?;
        let mut omega = ctx.genesis(core);
        let cap = step_cap(core);
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
        let ctx = RansIContext::from_core(core)?;
        let mut omega = ctx.genesis(core);
        let cap = step_cap(core);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn skewed(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = (s >> 33) % 100;
            out.push(if r < 60 {
                b'a'
            } else if r < 85 {
                b'b'
            } else if r < 95 {
                b'c'
            } else {
                b'd'
            });
        }
        out
    }

    #[test]
    fn roundtrip_every_lane_count() {
        let src = skewed(0x1234_5678, 4096);
        for k in 1..=MAX_LANES {
            let core = encode_with_k(&src, k).unwrap();
            let machine = RANS0I;
            assert_eq!(machine.decode(&core, src.len() as u64).unwrap(), src);
            assert!(machine.check_core(&core, src.len() as u64).is_ok());
        }
    }

    #[test]
    fn lane_counts_decode_identically() {
        let src = skewed(0xDEAD_BEEF, 2048);
        let mut reference: Option<Vec<u8>> = None;
        for k in [1usize, 2, 4, 8] {
            let core = encode_with_k(&src, k).unwrap();
            let got = RANS0I.decode(&core, src.len() as u64).unwrap();
            match &reference {
                None => reference = Some(got),
                Some(r) => assert_eq!(&got, r),
            }
        }
    }

    #[test]
    fn empty_and_tiny() {
        for src in [&b""[..], b"A", b"AB", b"abc"] {
            let core = encode_with_k(src, 4).unwrap();
            assert_eq!(RANS0I.decode(&core, src.len() as u64).unwrap(), src);
        }
    }

    #[test]
    fn invalid_header_errors_cleanly() {
        assert!(RansIContext::from_core(&[]).is_err());
        assert!(RansIContext::from_core(&[0u8; 100]).is_err());
        // A good core with K forced out of range.
        let mut core = encode_with_k(b"hello world hello world", 4).unwrap();
        core[K_FIELD_OFFSET..K_FIELD_OFFSET + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(RansIContext::from_core(&core).is_err());
        core[K_FIELD_OFFSET..K_FIELD_OFFSET + 4].copy_from_slice(&9u32.to_le_bytes());
        assert!(RansIContext::from_core(&core).is_err());
    }
}
