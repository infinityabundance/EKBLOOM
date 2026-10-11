//! Parallel-prefix reconstruction of the `OrANS-NL2` encoded core.
//!
//! The one-step transition of `OrANS-NL2` is **affine over `GF(2)`**. Writing the
//! 16-bit state as `x = (h, l)` (high byte `h`, low byte `l`),
//!
//! ```text
//! x' = A x ^ B r,   A(h, l) = (l, S(h) ^ l),   B(r) = (0, r),
//! ```
//!
//! where `S(v) = v ^ (v << 1) ^ (v << 3)` is `GF(2)`-linear (so `A` is a linear
//! map) and `B r` is the affine offset. Over a block of `D` input bytes,
//!
//! ```text
//! x_{i+D} = A^D x_i ^ b_{i,D},   b_{i,D} = XOR_{j=0..D-1} A^{D-1-j} B r_{i+j},
//! ```
//!
//! and block transforms compose affinely and **associatively**:
//! `(P2, b2) o (P1, b1) = (P2 P1, P2 b1 ^ b2)`. Associativity is exactly what lets
//! a single prefix scan turn `ceil(n/D)` independent block summaries into every
//! block's starting state, which in turn lets every block be decoded independently
//! and in parallel, byte-exact to the sequential [`crate::machine::Nl2`] decode.
//!
//! See `docs/PREFIX.md` for the proof sketch and the measured keep/reject verdict.

use std::thread;

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, NL2, SBOX};
use crate::state::Snapshot;

/// A 16-bit linear map `P` over `GF(2)`, stored as the images of the 16 basis
/// vectors: `cols[k] = P(1 << k)`.
///
/// Applying `P` to `x` is the `GF(2)` sum of the columns selected by the set bits
/// of `x`, which is the definition of a linear map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinMap {
    /// `cols[k] = P(1 << k)`.
    pub cols: [u16; 16],
}

impl LinMap {
    /// The identity map.
    pub fn identity() -> Self {
        let mut cols = [0u16; 16];
        let mut k = 0usize;
        while k < 16 {
            cols[k] = 1u16 << k;
            k += 1;
        }
        LinMap { cols }
    }

    /// Apply the map: the `GF(2)` sum of the columns for the set bits of `x`.
    pub fn apply(&self, x: u16) -> u16 {
        let mut y = 0u16;
        let mut bits = x;
        let mut k = 0usize;
        while bits != 0 {
            if bits & 1 != 0 {
                y ^= self.cols[k];
            }
            bits >>= 1;
            k += 1;
        }
        y
    }

    /// Compose: `self ∘ inner`, i.e. apply `inner` first, then `self`.
    pub fn compose(&self, inner: &LinMap) -> LinMap {
        let mut cols = [0u16; 16];
        let mut k = 0usize;
        while k < 16 {
            cols[k] = self.apply(inner.cols[k]);
            k += 1;
        }
        LinMap { cols }
    }

    /// `self^n` by square-and-multiply. `power(0)` is the identity.
    pub fn power(&self, n: u64) -> LinMap {
        let mut result = LinMap::identity();
        let mut base = *self;
        let mut e = n;
        while e != 0 {
            if e & 1 != 0 {
                result = base.compose(&result);
            }
            e >>= 1;
            if e != 0 {
                base = base.compose(&base);
            }
        }
        result
    }
}

/// One block's affine summary: `x_out = map(x_in) ^ add`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Affine {
    /// Linear part.
    pub map: LinMap,
    /// Affine offset, applied after `map`.
    pub add: u16,
}

impl Affine {
    /// The identity affine transform `x -> x`.
    pub fn identity() -> Self {
        Affine {
            map: LinMap::identity(),
            add: 0,
        }
    }

    /// Apply the affine transform: `map(x) ^ add`.
    pub fn apply(&self, x: u16) -> u16 {
        self.map.apply(x) ^ self.add
    }

    /// Compose: `self ∘ inner`, i.e. apply `inner` first, then `self`.
    pub fn compose(&self, inner: &Affine) -> Affine {
        Affine {
            map: self.map.compose(&inner.map),
            add: self.map.apply(inner.add) ^ self.add,
        }
    }
}

/// The linear part `A` of the `OrANS-NL2` step, built from the machine's own
/// `SBOX`: `cols[k] = A(1 << k)` with `A(h, l) = (l, S(h) ^ l)`.
fn linear_step() -> LinMap {
    let mut cols = [0u16; 16];
    let mut k = 0usize;
    while k < 16 {
        let e = 1u16 << k;
        let h = (e >> 8) as u8;
        let l = (e & 0xFF) as u8;
        let new_h = l;
        let new_l = SBOX[h as usize] ^ l;
        cols[k] = (u16::from(new_h) << 8) | u16::from(new_l);
        k += 1;
    }
    LinMap { cols }
}

/// Exact block summary for `r` (the block's `D` input bytes): `(A^D, b)`.
///
/// `b` is accumulated incrementally by the machine's own recurrence applied to a
/// state initialised to zero, and the linear part is `A^D` via square-and-multiply,
/// so the whole summary is `O(D)` time and `O(log D)` map multiplications rather
/// than `O(D)` map multiplications.
pub fn block_summary(r: &[u8]) -> Affine {
    let a = linear_step();
    let mut add = 0u16;
    for &byte in r {
        let h = (add >> 8) as u8;
        let l = (add & 0xFF) as u8;
        let next_low = SBOX[h as usize] ^ l ^ byte;
        add = (u16::from(l) << 8) | u16::from(next_low);
    }
    Affine {
        map: a.power(r.len() as u64),
        add,
    }
}

/// Prefix scan over block summaries.
///
/// Returns `blocks.len() + 1` transforms where `out[k]` is the composition of the
/// first `k` blocks: the **incoming** affine transform of block `k`. Thus
/// `out[0]` is the identity and `out[k].apply(x0)` is the exact machine state at
/// logical position `k * stride`; `out[blocks.len()]` is the final transform. The
/// scan is associative, so it could be computed with any (e.g. parallel) bracket.
pub fn prefix_scan(blocks: &[Affine]) -> Vec<Affine> {
    let mut out = Vec::with_capacity(blocks.len() + 1);
    out.push(Affine::identity());
    for block in blocks {
        let prev = out[out.len() - 1];
        out.push(block.compose(&prev));
    }
    out
}

/// Reveal `r.len()` symbols from starting state `x0` using the machine's reveal.
fn decode_block(x0: u16, r: &[u8]) -> Result<Vec<u8>> {
    let mut omega = Snapshot::new(u32::from(x0), 0);
    let mut out = Vec::with_capacity(r.len());
    for _ in 0..r.len() {
        out.push(NL2.reveal(r, &mut omega)?);
    }
    Ok(out)
}

/// Reconstruct the symbols of blocks `[lo, hi)` independently.
fn decode_range(
    start_states: &[u16],
    r_blocks: &[&[u8]],
    lo: usize,
    hi: usize,
) -> Result<Vec<Vec<u8>>> {
    let mut part = Vec::with_capacity(hi - lo);
    let mut i = lo;
    while i < hi {
        part.push(decode_block(start_states[i], r_blocks[i])?);
        i += 1;
    }
    Ok(part)
}

/// Reconstruct `D` symbols per block from per-block starting states, in parallel
/// across `threads`, byte-exact to the sequential [`crate::machine::Nl2`] decode.
///
/// `start_states[k]` must be the exact state at the start of `r_blocks[k]` (for
/// the genesis `OrANS-NL2` object that is `prefix_scan(&summaries)[k].apply(0)`).
/// `threads` is clamped to `1..=r_blocks.len()`; each block is decoded with the
/// machine's own [`OrMachine::reveal`], so the output is authoritative.
pub fn reconstruct_blocks(
    start_states: &[u16],
    r_blocks: &[&[u8]],
    threads: usize,
) -> Result<Vec<Vec<u8>>> {
    if start_states.len() != r_blocks.len() {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "start_states length {} != r_blocks length {}",
                start_states.len(),
                r_blocks.len()
            ),
        });
    }
    let nblocks = r_blocks.len();
    if nblocks == 0 {
        return Ok(Vec::new());
    }
    let workers = threads.max(1).min(nblocks);
    if workers == 1 {
        // Avoid a scoped-thread spawn for the single-threaded path; the thread
        // spawn dominates short inputs and is pure overhead here.
        return decode_range(start_states, r_blocks, 0, nblocks);
    }
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(nblocks);

    thread::scope(|scope| -> Result<()> {
        let chunk = nblocks.div_ceil(workers);
        let mut handles = Vec::new();
        let mut lo = 0usize;
        while lo < nblocks {
            let hi = lo.saturating_add(chunk).min(nblocks);
            handles.push(scope.spawn(move || decode_range(start_states, r_blocks, lo, hi)));
            lo = hi;
        }
        for handle in handles {
            let part = handle.join().map_err(|_| EkbloomError::InvalidArgument {
                what: "prefix reconstruction worker panicked".to_string(),
            })?;
            out.extend(part?);
        }
        Ok(())
    })?;

    Ok(out)
}

/// Scalar baseline: reveal `r.len()` symbols from `x0` sequentially, one block.
///
/// The `Err` arm is unreachable because exactly `r.len()` symbols are revealed
/// from an `r` of that length, but the machine's fallible interface is honoured
/// rather than unwrapped.
pub fn reconstruct_scalar(x0: u16, r: &[u8]) -> Vec<u8> {
    let mut omega = Snapshot::new(u32::from(x0), 0);
    let mut out = Vec::with_capacity(r.len());
    for _ in 0..r.len() {
        match NL2.reveal(r, &mut omega) {
            Ok(s) => out.push(s),
            Err(_) => break,
        }
    }
    out
}

/// Fast positional state at logical position `i` from precomputed block
/// summaries, with no genesis replay.
///
/// `summaries` is the [`prefix_scan`] output for a block size of `stride`;
/// `summaries[i / stride].apply(x0)` is the exact machine state at the block
/// boundary `floor(i / stride) * stride`. When `stride == 1` every position is a
/// boundary and the result is exact for all `i`. Out-of-range positions saturate
/// to the last available boundary (and to `x0` for an empty scan); this never
/// panics.
pub fn state_at_via_prefix(summaries: &[Affine], x0: u16, stride: u64, i: u64) -> u16 {
    if stride == 0 {
        return x0;
    }
    let j = i / stride;
    let idx = match usize::try_from(j) {
        Ok(v) => v,
        Err(_) => return x0,
    };
    match summaries.get(idx) {
        Some(a) => a.apply(x0),
        None => match summaries.last() {
            Some(a) => a.apply(x0),
            None => x0,
        },
    }
}
