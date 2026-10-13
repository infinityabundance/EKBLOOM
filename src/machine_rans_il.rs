//! `OrANS-RANS0IL` (`machine_id = 8`): order-0 rANS with **independent-lane
//! streams**.
//!
//! Machine 4 (`OrANS-RANS0I`) keeps `K` independent rANS *states* but shares one
//! renormalisation cursor across lanes: a group of `K` symbols is decoded with `K`
//! independent arithmetic chains, yet their renorm bytes are consumed from a
//! **single ordered stream**, so lane `t`'s reads still depend on how many bytes
//! lanes `0..t` consumed. This machine removes that ordering: each lane owns its
//! own encoded **payload range** and its own **cursor**, so lane `t`'s decode is
//! fully independent of every other lane (the property that would let a parallel
//! reconstructor rebuild lanes concurrently).
//!
//! # Core layout (`core = encode(source, K)`)
//!
//! ```text
//! [freq: 256 * u16 LE = 512 B][K: u32 LE]
//! [lane_len[0..K]: K * u32 LE][states[0..K]: K * u32 LE]
//! [lane 0 payload][lane 1 payload]...[lane K-1 payload]
//! ```
//!
//! `lane_len[t]` gives lane `t`'s payload length; lane `t`'s payload begins at
//! `header_bytes + sum(lane_len[0..t])`. Symbol `i` is coded in lane `i % K`.
//! Each lane's emitted bytes are stored reversed (as in machines 2/4) so the
//! decoder reads its payload forward.
//!
//! # Semantics and complete state
//!
//! Genesis `Omega_0 = (states, cursors = payload starts, lane 0)`. One `reveal`
//! decodes one symbol on the active lane using **that lane's cursor**, then
//! advances the lane cyclically. The complete resumable state is
//! `(states[K], cursors[K], lane)`; it is carried in a [`Snapshot`] (states in
//! `lanes`, cursors in `cursors`, active lane in `lane`). The per-symbol input
//! bound is machine 2's (at most two 8-bit reads per step), so the encoded-window
//! byte bound is `2 * (steps + symbols) + margin` per lane.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_RANS0IL};
use crate::machine_rans::{
    valid_state, Model, RansContext, M, MAX_STEPS_PER_BYTE, MODEL_BYTES, RANS_L, SCALE_BITS,
};
use crate::state::{Snapshot, MAX_LANES};

/// Default lane count for [`encode`].
pub const DEFAULT_IL_K: usize = 4;

/// Smallest accepted lane count.
const MIN_IL_K: usize = 1;

/// Largest accepted lane count (bounded by [`MAX_LANES`], which sizes every
/// stored restart record).
pub const MAX_IL_LANES: usize = MAX_LANES;

/// Bytes before the `K` field (`256 * u16` frequencies).
const IL_K_FIELD_OFFSET: usize = MODEL_BYTES;

/// Model header size for the fixed part (`freq` ++ `K`).
const IL_FIXED_HEADER: usize = MODEL_BYTES + 4;

/// Safety margin for the bounded window (mirrors machine 2's).
pub const IL_WINDOW_MARGIN: u64 = 64;

/// The `OrANS-RANS0IL` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rans0IL;

/// The single process-wide `OrANS-RANS0IL` instance (encodes with [`DEFAULT_IL_K`]).
pub static RANS0IL: Rans0IL = Rans0IL;

/// The complete resumable state of an independent-lane stream.
///
/// This is the module-level mirror of the [`Snapshot`] projection: the same
/// `(states, cursors, lane)` triple, usable without the global `Snapshot` type
/// (measurement and tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IlState {
    /// Per-lane coder states; `lanes[lane]` reveals the next symbol.
    pub lanes: [u32; MAX_IL_LANES],
    /// Per-lane encoded cursors (byte offsets into the core).
    pub cursors: [u64; MAX_IL_LANES],
    /// Index of the lane that reveals the next symbol.
    pub lane: u8,
}

/// Packed wire size of an [`IlState`]: `K` u32 states ++ `K` u64 cursors ++ u8 lane.
pub const IL_STATE_LEN: usize = 4 * MAX_IL_LANES + 8 * MAX_IL_LANES + 1;

impl IlState {
    /// Pack into the fixed little-endian resume form.
    pub fn pack(&self) -> [u8; IL_STATE_LEN] {
        let mut out = [0u8; IL_STATE_LEN];
        let mut o = 0;
        for &x in &self.lanes {
            out[o..o + 4].copy_from_slice(&x.to_le_bytes());
            o += 4;
        }
        for &c in &self.cursors {
            out[o..o + 8].copy_from_slice(&c.to_le_bytes());
            o += 8;
        }
        out[o] = self.lane;
        out
    }

    /// Unpack from the fixed little-endian resume form.
    pub fn unpack(bytes: &[u8]) -> Result<IlState> {
        if bytes.len() < IL_STATE_LEN {
            return Err(EkbloomError::Truncated {
                needed: IL_STATE_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let mut lanes = [0u32; MAX_IL_LANES];
        let mut o = 0;
        for slot in lanes.iter_mut() {
            *slot = u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
            o += 4;
        }
        let mut cursors = [0u64; MAX_IL_LANES];
        for slot in cursors.iter_mut() {
            *slot = u64::from_le_bytes([
                bytes[o],
                bytes[o + 1],
                bytes[o + 2],
                bytes[o + 3],
                bytes[o + 4],
                bytes[o + 5],
                bytes[o + 6],
                bytes[o + 7],
            ]);
            o += 8;
        }
        let lane = bytes[o];
        Ok(IlState {
            lanes,
            cursors,
            lane,
        })
    }

    /// Project onto the machine-general [`Snapshot`].
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::with_lanes_cursors(
            self.cursors[usize::from(self.lane)],
            self.lanes,
            self.lane,
            self.cursors,
        )
    }

    /// Recover an [`IlState`] from a [`Snapshot`].
    pub fn from_snapshot(s: &Snapshot) -> IlState {
        IlState {
            lanes: s.lanes,
            cursors: s.cursors,
            lane: s.lane,
        }
    }
}

/// A validated, reusable independent-lane decoder context for one core.
pub struct IlContext {
    table: RansContext,
    k: usize,
    header_bytes: usize,
    /// `starts[t]` = byte offset where lane `t`'s payload begins.
    starts: [u64; MAX_LANES],
    /// `ends[t]` = one past lane `t`'s last payload byte.
    ends: [u64; MAX_LANES],
}

impl IlContext {
    /// Parse and validate `core`'s model, `K`, per-lane lengths and lane states.
    pub fn from_core(core: &[u8]) -> Result<IlContext> {
        if core.len() < IL_FIXED_HEADER {
            return Err(EkbloomError::Truncated {
                needed: IL_FIXED_HEADER as u64,
                available: core.len() as u64,
            });
        }
        let k = u32::from_le_bytes([
            core[IL_K_FIELD_OFFSET],
            core[IL_K_FIELD_OFFSET + 1],
            core[IL_K_FIELD_OFFSET + 2],
            core[IL_K_FIELD_OFFSET + 3],
        ]) as usize;
        if !(MIN_IL_K..=MAX_IL_LANES).contains(&k) {
            return Err(EkbloomError::BadSection {
                reason: format!("independent-lane count {k} out of range [1, {MAX_IL_LANES}]"),
            });
        }
        let header_bytes = IL_FIXED_HEADER + 8 * k;
        if core.len() < header_bytes {
            return Err(EkbloomError::Truncated {
                needed: header_bytes as u64,
                available: core.len() as u64,
            });
        }
        // Reuse machine 2's validated model parse and fused `slot` table.
        let table = RansContext::from_core(core)?;

        let mut lane_len = [0u64; MAX_LANES];
        for (t, slot) in lane_len.iter_mut().enumerate().take(k) {
            let o = IL_FIXED_HEADER + 4 * t;
            *slot = u64::from(u32::from_le_bytes([
                core[o],
                core[o + 1],
                core[o + 2],
                core[o + 3],
            ]));
        }
        let mut starts = [0u64; MAX_LANES];
        let mut ends = [0u64; MAX_LANES];
        let mut acc = header_bytes as u64;
        for t in 0..k {
            starts[t] = acc;
            acc = acc.checked_add(lane_len[t]).ok_or(EkbloomError::Overflow {
                what: "independent-lane payload offset",
            })?;
            ends[t] = acc;
        }
        if acc > core.len() as u64 {
            return Err(EkbloomError::CoreTooShort {
                needed: acc,
                available: core.len() as u64,
            });
        }
        // Reject a declared total that does not match the core exactly: the
        // payload is the whole core after the header.
        if acc != core.len() as u64 {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "independent-lane payloads end at {acc}, core is {} bytes",
                    core.len()
                ),
            });
        }

        for t in 0..k {
            let o = IL_FIXED_HEADER + 4 * k + 4 * t;
            let x = u32::from_le_bytes([core[o], core[o + 1], core[o + 2], core[o + 3]]);
            if !valid_state(x) {
                return Err(EkbloomError::BadSection {
                    reason: format!("independent-lane state {x} out of range"),
                });
            }
        }

        Ok(IlContext {
            table,
            k,
            header_bytes,
            starts,
            ends,
        })
    }

    /// The number of lanes `K`.
    #[inline]
    pub fn lanes(&self) -> usize {
        self.k
    }

    /// The model-header length in bytes.
    #[inline]
    pub fn header_bytes(&self) -> u64 {
        self.header_bytes as u64
    }

    /// The fused per-slot decode table (machine 2's `slot -> (freq, cum, symbol)`).
    #[inline]
    pub fn table(&self) -> &[u32] {
        self.table.table()
    }

    /// `(start, end)` byte range of lane `t`'s payload.
    #[inline]
    pub fn lane_range(&self, t: usize) -> (u64, u64) {
        (self.starts[t], self.ends[t])
    }

    /// The genesis complete state `(states, cursors = payload starts, lane 0)`.
    pub fn genesis(&self, core: &[u8]) -> IlState {
        let mut lanes = [0u32; MAX_IL_LANES];
        for (t, slot) in lanes.iter_mut().enumerate().take(self.k) {
            let o = IL_FIXED_HEADER + 4 * self.k + 4 * t;
            *slot = u32::from_le_bytes([core[o], core[o + 1], core[o + 2], core[o + 3]]);
        }
        let mut cursors = [0u64; MAX_IL_LANES];
        for (t, slot) in cursors.iter_mut().enumerate().take(self.k) {
            *slot = self.starts[t];
        }
        IlState {
            lanes,
            cursors,
            lane: 0,
        }
    }

    /// Validate a complete restart state for this context.
    pub fn validate(&self, s: &IlState) -> Result<()> {
        if usize::from(s.lane) >= self.k {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "independent-lane restart lane {} out of range [0, {})",
                    s.lane, self.k
                ),
            });
        }
        for t in 0..self.k {
            if !valid_state(s.lanes[t]) {
                return Err(EkbloomError::BadSection {
                    reason: format!("independent-lane restart lane {t} state out of range"),
                });
            }
            if s.cursors[t] < self.starts[t] || s.cursors[t] > self.ends[t] {
                return Err(EkbloomError::BadSection {
                    reason: format!("independent-lane restart lane {t} cursor out of range"),
                });
            }
        }
        if usize::from(s.lane) >= self.k {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "independent-lane restart lane {} disagrees with its lane count",
                    s.lane
                ),
            });
        }
        Ok(())
    }

    /// One lane step reading from the lane's own payload range.
    #[inline]
    fn step(&self, core: &[u8], table: &[u32], end: u64, x: u32, p: &mut u64) -> Result<(u8, u32)> {
        let slot = x & (M - 1);
        let e = table[slot as usize];
        let mut nx = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
        while nx < RANS_L {
            if *p >= end {
                return Err(EkbloomError::CoreTooShort {
                    needed: p.saturating_add(1),
                    available: core.len() as u64,
                });
            }
            let b = *core.get(*p as usize).ok_or(EkbloomError::CoreTooShort {
                needed: p.saturating_add(1),
                available: core.len() as u64,
            })?;
            nx = (nx << 8) | u32::from(b);
            *p += 1;
        }
        Ok(((e >> 24) as u8, nx))
    }

    /// Reveal one symbol from `s`, advancing the active lane.
    #[inline]
    pub fn reveal(&self, core: &[u8], s: &mut IlState) -> Result<u8> {
        let table = self.table.table();
        let lane = usize::from(s.lane) % self.k;
        let (sym, nx) = self.step(
            core,
            table,
            self.ends[lane],
            s.lanes[lane],
            &mut s.cursors[lane],
        )?;
        s.lanes[lane] = nx;
        s.lane = if lane + 1 == self.k {
            0
        } else {
            (lane + 1) as u8
        };
        Ok(sym)
    }

    /// Advance `s` by exactly `n` steps, discarding the symbols.
    pub fn skip(&self, core: &[u8], s: &mut IlState, n: u64) -> Result<()> {
        let table = self.table.table();
        let k = self.k;
        let mut lane = usize::from(s.lane) % k;
        let mut remaining = n;
        // Align to lane 0.
        while lane != 0 && remaining != 0 {
            let (_, nx) = self.step(
                core,
                table,
                self.ends[lane],
                s.lanes[lane],
                &mut s.cursors[lane],
            )?;
            s.lanes[lane] = nx;
            remaining -= 1;
            lane = if lane + 1 == k { 0 } else { lane + 1 };
        }
        while remaining >= k as u64 {
            for t in 0..k {
                let (_, nx) =
                    self.step(core, table, self.ends[t], s.lanes[t], &mut s.cursors[t])?;
                s.lanes[t] = nx;
            }
            remaining -= k as u64;
        }
        while remaining != 0 {
            let (_, nx) = self.step(
                core,
                table,
                self.ends[lane],
                s.lanes[lane],
                &mut s.cursors[lane],
            )?;
            s.lanes[lane] = nx;
            remaining -= 1;
            lane = if lane + 1 == k { 0 } else { lane + 1 };
        }
        s.lane = lane as u8;
        Ok(())
    }

    /// Decode exactly `out.len()` symbols from `s` — the independent-lane kernel.
    ///
    /// A full group of `K` symbols visits every lane once; because every lane has
    /// its **own** cursor there is no shared ordering constraint at all, so the
    /// `K` table loads, multiplies and renorm loops are free-running.
    pub fn decode_into_slice(&self, core: &[u8], s: &mut IlState, out: &mut [u8]) -> Result<()> {
        let table = self.table.table();
        let k = self.k;
        let n = out.len();
        let mut i = 0usize;
        let mut lane = usize::from(s.lane) % k;

        while lane != 0 && i < n {
            let (sym, nx) = self.step(
                core,
                table,
                self.ends[lane],
                s.lanes[lane],
                &mut s.cursors[lane],
            )?;
            s.lanes[lane] = nx;
            out[i] = sym;
            i += 1;
            lane = if lane + 1 == k { 0 } else { lane + 1 };
        }

        while i + k <= n {
            let mut nx = [0u32; MAX_LANES];
            let mut sym = [0u8; MAX_LANES];
            for t in 0..k {
                let x = s.lanes[t];
                let slot = x & (M - 1);
                let e = table[slot as usize];
                nx[t] = (x >> SCALE_BITS) * (e & 0x0FFF) + slot - ((e >> SCALE_BITS) & 0x0FFF);
                sym[t] = (e >> 24) as u8;
            }
            for (t, slot) in nx.iter_mut().enumerate().take(k) {
                let mut v = *slot;
                let end = self.ends[t];
                let mut p = s.cursors[t];
                while v < RANS_L {
                    if p >= end {
                        return Err(EkbloomError::CoreTooShort {
                            needed: p.saturating_add(1),
                            available: core.len() as u64,
                        });
                    }
                    let b = *core.get(p as usize).ok_or(EkbloomError::CoreTooShort {
                        needed: p.saturating_add(1),
                        available: core.len() as u64,
                    })?;
                    v = (v << 8) | u32::from(b);
                    p += 1;
                }
                s.cursors[t] = p;
                s.lanes[t] = v;
            }
            out[i..i + k].copy_from_slice(&sym[..k]);
            i += k;
        }

        while i < n {
            let (sym, nx) = self.step(
                core,
                table,
                self.ends[lane],
                s.lanes[lane],
                &mut s.cursors[lane],
            )?;
            s.lanes[lane] = nx;
            out[i] = sym;
            i += 1;
            lane = if lane + 1 == k { 0 } else { lane + 1 };
        }
        s.lane = lane as u8;
        Ok(())
    }

    /// Reveal `n` symbols, appending them to `out`.
    pub fn decode_into(
        &self,
        core: &[u8],
        s: &mut IlState,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let n = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
            what: "independent-lane decode length",
        })?;
        crate::resource::checked_default(n as u64)?;
        let start = out.len();
        let end = start.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "independent-lane decode buffer",
        })?;
        out.resize(end, 0);
        self.decode_into_slice(core, s, &mut out[start..end])
    }
}

/// Encode `source` into an independent-lane core with `k` lanes.
pub fn encode_il(source: &[u8], k: usize) -> Result<Vec<u8>> {
    if !(MIN_IL_K..=MAX_IL_LANES).contains(&k) {
        return Err(EkbloomError::InvalidArgument {
            what: format!("independent-lane count {k} out of range [1, {MAX_IL_LANES}]"),
        });
    }
    let model = Model::from_source(source);
    let mut states = [RANS_L; MAX_IL_LANES];
    let mut emitted: Vec<Vec<u8>> = (0..k).map(|_| Vec::new()).collect();
    for i in (0..source.len()).rev() {
        let lane = i % k;
        let f = u64::from(model.freq[source[i] as usize]);
        let c = u64::from(model.cum[source[i] as usize]);
        let mut x = u64::from(states[lane]);
        let x_max = u64::from((RANS_L >> SCALE_BITS) << 8) * f;
        while x >= x_max {
            emitted[lane].push((x & 0xFF) as u8);
            x >>= 8;
        }
        states[lane] = (((x / f) << SCALE_BITS) + (x % f) + c) as u32;
    }

    let header = IL_FIXED_HEADER + 8 * k;
    let payload: usize = emitted.iter().map(Vec::len).sum();
    let mut core = Vec::with_capacity(header + payload);
    for &f in model.freq.iter() {
        core.extend_from_slice(&f.to_le_bytes());
    }
    core.extend_from_slice(&(k as u32).to_le_bytes());
    for e in &emitted {
        core.extend_from_slice(&(e.len() as u32).to_le_bytes());
    }
    for &x in states[..k].iter() {
        core.extend_from_slice(&x.to_le_bytes());
    }
    for e in &emitted {
        // Reverse lane emission order so the decoder reads the payload forward.
        core.extend(e.iter().rev());
    }
    Ok(core)
}

/// Decode a core produced by [`encode_il`] from genesis into `out`, byte-exact to
/// the stored machine.
pub fn decode_il(core: &[u8], out: &mut [u8]) -> Result<()> {
    let ctx = IlContext::from_core(core)?;
    let mut s = ctx.genesis(core);
    ctx.decode_into_slice(core, &mut s, out)
}

/// Record the complete resumable state at the start of every logical block of
/// `block_bytes` symbols (lane states, per-lane cursors, active lane).
pub fn il_block_restarts(core: &[u8], logical_len: u64, block_bytes: u32) -> Result<Vec<IlState>> {
    if block_bytes == 0 {
        return Err(EkbloomError::InvalidArgument {
            what: "block size must be non-zero".to_string(),
        });
    }
    let ctx = IlContext::from_core(core)?;
    let b = u64::from(block_bytes);
    let count = logical_len
        .checked_div(b)
        .and_then(|blocks| blocks.checked_add(1))
        .ok_or(EkbloomError::Overflow {
            what: "il block restart count",
        })?;
    let capacity = usize::try_from(count).map_err(|_| EkbloomError::Overflow {
        what: "il block restart count",
    })?;
    let mut entries: Vec<IlState> = Vec::with_capacity(capacity);
    let mut s = ctx.genesis(core);
    entries.push(s);
    let mut i: u64 = 0;
    while i < logical_len {
        ctx.reveal(core, &mut s)?;
        i += 1;
        if i.is_multiple_of(b) {
            entries.push(s);
        }
    }
    Ok(entries)
}

fn step_cap(core: &[u8]) -> u64 {
    (core.len() as u64)
        .saturating_mul(MAX_STEPS_PER_BYTE)
        .saturating_add(1 << 20)
}

impl OrMachine for Rans0IL {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_RANS0IL
    }

    #[inline]
    fn snapshot_needs_lanes(&self) -> bool {
        true
    }

    #[inline]
    fn snapshot_needs_lane_cursors(&self) -> bool {
        true
    }

    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        let ctx = IlContext::from_core(core)?;
        Ok(ctx.genesis(core).snapshot())
    }

    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let ctx = IlContext::from_core(core)?;
        let mut s = IlState::from_snapshot(omega);
        let sym = ctx.reveal(core, &mut s)?;
        *omega = s.snapshot();
        Ok(sym)
    }

    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let ctx = IlContext::from_core(core)?;
        let mut s = IlState::from_snapshot(omega);
        ctx.reveal(core, &mut s)
    }

    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        encode_il(source, DEFAULT_IL_K)
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = IlContext::from_core(core)?;
        let mut s = ctx.genesis(core);
        let cap = step_cap(core);
        let mut steps = 0u64;
        while steps < logical_len {
            if steps >= cap {
                return Err(EkbloomError::NonTermination);
            }
            ctx.reveal(core, &mut s)?;
            steps += 1;
        }
        Ok(())
    }

    fn decode(&self, core: &[u8], logical_len: u64) -> Result<Vec<u8>> {
        crate::resource::checked_default(logical_len)?;
        let ctx = IlContext::from_core(core)?;
        let mut s = ctx.genesis(core);
        let cap = step_cap(core);
        let mut out: Vec<u8> = Vec::with_capacity(logical_len as usize);
        let mut steps = 0u64;
        while steps < logical_len {
            if steps >= cap {
                return Err(EkbloomError::NonTermination);
            }
            out.push(ctx.reveal(core, &mut s)?);
            steps += 1;
        }
        Ok(out)
    }

    fn state_log(&self, core: &[u8], logical_len: u64) -> Result<Vec<u32>> {
        crate::resource::checked_default(logical_len.saturating_mul(4).saturating_add(4))?;
        let ctx = IlContext::from_core(core)?;
        let mut s = ctx.genesis(core);
        let cap = step_cap(core);
        let mut log: Vec<u32> = Vec::with_capacity(logical_len as usize + 1);
        log.push(s.lanes[0]);
        let mut steps = 0u64;
        while steps < logical_len {
            if steps >= cap {
                return Err(EkbloomError::NonTermination);
            }
            ctx.reveal(core, &mut s)?;
            log.push(s.lanes[usize::from(s.lane) % ctx.lanes()]);
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
    fn roundtrip_every_lane_count() {
        let src = skewed(11, 4096);
        for k in MIN_IL_K..=MAX_IL_LANES {
            let core = encode_il(&src, k).unwrap();
            let mut out = vec![0u8; src.len()];
            decode_il(&core, &mut out).unwrap();
            assert_eq!(out, src, "k={k}");
        }
    }

    #[test]
    fn complete_state_roundtrips() {
        let src = skewed(12, 2000);
        let core = encode_il(&src, 4).unwrap();
        let ctx = IlContext::from_core(&core).unwrap();
        let mut s = ctx.genesis(&core);
        ctx.skip(&core, &mut s, 777).unwrap();
        let packed = s.pack();
        let back = IlState::unpack(&packed).unwrap();
        assert_eq!(back, s);
        let restored = IlState::from_snapshot(&back.snapshot());
        assert_eq!(restored, s);
    }

    #[test]
    fn truncated_headers_error() {
        let src = skewed(13, 100);
        let core = encode_il(&src, 4).unwrap();
        assert!(IlContext::from_core(&core[..10]).is_err());
        let mut bad = core.clone();
        bad[IL_K_FIELD_OFFSET] = 200; // k out of range
        assert!(IlContext::from_core(&bad).is_err());
    }
}
