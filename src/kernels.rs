//! Reconstruction kernels (directive §6).
//!
//! Two reconstruction jobs are investigated here, honestly and to a keep/reject
//! verdict:
//!
//! * **Kernel A — short restart windows.** From a known 16-bit start state,
//!   reconstruct 16/32/64 symbols from the block's input bytes. A window is a
//!   *serial* dependent chain (`s_i` depends on `s_{i-1}` and `s_{i-2}`), so the
//!   only parallelism is **across independent windows**. Three variants are
//!   provided: a straight scalar loop, an unrolled scalar loop, a 2-symbol
//!   table-assisted loop, and a vector kernel that reconstructs sixteen windows at
//!   once (one 16-bit lane per window) via [`crate::simd`].
//! * **Kernel B — independent long blocks.** Use [`crate::prefix`] block summaries
//!   to establish every block's start state (one associative prefix scan), then
//!   reconstruct the blocks: once on the pure-parallel-threads path and once
//!   vectorised across sixteen blocks per AVX2 register.
//!
//! Every kernel is **byte-exact** against the authoritative
//! [`crate::machine::Nl2`] machine, proven in `tests/kernels_planner.rs`. The
//! measured keep/reject verdicts are in `docs/KERNELS.md` and
//! `evidence/bench/kernels.json`; a measured reject is a valid outcome.
//!
//! No `unsafe` lives here: the only vector code is the sanctioned, runtime-gated
//! [`crate::simd`] module. Kernels call into it, and the scalar reference with the
//! *identical* step-major layout is the equal-residency control.

use std::sync::OnceLock;

use crate::error::{EkbloomError, Result};
use crate::machine::SBOX;
use crate::prefix::{block_summary, prefix_scan, Affine};
use crate::simd;

/// A 2-symbol table: `T2[x] = (S(x >> 8) ^ (x & 0xFF), S(x & 0xFF))`.
///
/// `x_{i+2}` depends on `(x_i, r_i, r_{i+1})`; the first component yields `s_i`
/// given `r_i`, and the second yields `s_{i+1}` given `s_i` and `r_{i+1}`.
fn pair_table() -> &'static [(u8, u8); 65536] {
    static T: OnceLock<[(u8, u8); 65536]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [(0u8, 0u8); 65536];
        for (x, slot) in t.iter_mut().enumerate() {
            let h = (x >> 8) as u8;
            let l = (x & 0xFF) as u8;
            *slot = (SBOX[h as usize] ^ l, SBOX[l as usize]);
        }
        t
    })
}

// ---------------------------------------------------------------------------
// Kernel A — short restart windows.
// ---------------------------------------------------------------------------

/// Kernel A, scalar: reveal `r.len()` symbols from `x0`, one step at a time.
///
/// This is the baseline every other Kernel A variant must beat.
pub fn kernel_a_scalar(x0: u16, r: &[u8], out: &mut Vec<u8>) {
    out.reserve(r.len());
    let mut x = x0;
    for &b in r {
        let (s, next) = simd::nl2_step(x, b);
        out.push(s);
        x = next;
    }
}

/// Kernel A, scalar unrolled by eight: same recurrence, loop overhead removed.
pub fn kernel_a_unrolled(x0: u16, r: &[u8], out: &mut Vec<u8>) {
    out.reserve(r.len());
    let mut x = x0;
    let mut i = 0usize;
    let mut buf = [0u8; 8];
    while i + 8 <= r.len() {
        let chunk = &r[i..i + 8];
        let mut k = 0usize;
        while k < 8 {
            let (s, next) = simd::nl2_step(x, chunk[k]);
            buf[k] = s;
            x = next;
            k += 1;
        }
        out.extend_from_slice(&buf);
        i += 8;
    }
    while i < r.len() {
        let (s, next) = simd::nl2_step(x, r[i]);
        out.push(s);
        x = next;
        i += 1;
    }
}

/// Kernel A, table-assisted: two symbols per iteration using [`pair_table`].
///
/// Only one 16-bit table lookup is needed for each *pair* of symbols, replacing
/// the two `S` computations the scalar loop performs.
pub fn kernel_a_table(x0: u16, r: &[u8], out: &mut Vec<u8>) {
    let t2 = pair_table();
    out.reserve(r.len());
    let mut x = x0;
    let mut i = 0usize;
    while i + 2 <= r.len() {
        let (p0, s_low) = t2[x as usize];
        let s0 = p0 ^ r[i];
        let s1 = s_low ^ s0 ^ r[i + 1];
        out.push(s0);
        out.push(s1);
        x = (u16::from(s0) << 8) | u16::from(s1);
        i += 2;
    }
    if i < r.len() {
        let (s, _) = simd::nl2_step(x, r[i]);
        out.push(s);
    }
}

/// Kernel A, scalar batch over many windows — the **equal-residency control** for
/// [`kernel_a_simd_windows`] (identical input/output layout and byte volume).
///
/// `states.len()` windows, each of `window_len` steps. The layout is group-major:
/// window `w = g*16 + k` reads step `i` from `cols[(g*window_len + i) * 16 + k]`,
/// and writes the same index in `out`.
pub fn kernel_a_scalar_windows(
    states: &mut [u16],
    cols: &[u8],
    window_len: usize,
    out: &mut [u8],
) -> Result<()> {
    validate_batch(states.len(), cols.len(), window_len, out.len())?;
    for (w, state) in states.iter_mut().enumerate() {
        let base = window_group_base(w, window_len);
        let lane = w % simd::NL2_WINDOW_LANES;
        let mut x = *state;
        for i in 0..window_len {
            let idx = base + i * simd::NL2_WINDOW_LANES + lane;
            let (s, next) = simd::nl2_step(x, cols[idx]);
            x = next;
            out[idx] = s;
        }
        *state = x;
    }
    Ok(())
}

/// Kernel A, vector: reconstruct many short windows sixteen at a time.
///
/// Windows are processed in groups of [`simd::NL2_WINDOW_LANES`] (16); the tail
/// `windows % 16` run scalar. Byte-for-byte identical to
/// [`kernel_a_scalar_windows`] for every input.
pub fn kernel_a_simd_windows(
    states: &mut [u16],
    cols: &[u8],
    window_len: usize,
    out: &mut [u8],
) -> Result<()> {
    let windows = states.len();
    validate_batch(windows, cols.len(), window_len, out.len())?;
    let lanes = simd::NL2_WINDOW_LANES;
    let groups = windows / lanes;
    let group_bytes = window_len
        .checked_mul(lanes)
        .ok_or(EkbloomError::Overflow {
            what: "window group length",
        })?;
    for g in 0..groups {
        let base = g * group_bytes;
        let mut xs = [0u16; simd::NL2_WINDOW_LANES];
        xs.copy_from_slice(&states[g * lanes..g * lanes + lanes]);
        simd::nl2_windows16_avx2(
            &mut xs,
            &cols[base..base + group_bytes],
            window_len,
            &mut out[base..base + group_bytes],
        )?;
        states[g * lanes..g * lanes + lanes].copy_from_slice(&xs);
    }
    // Scalar tail (fewer than sixteen windows remain).
    kernel_a_simd_tail(states, cols, window_len, out, groups);
    Ok(())
}

fn kernel_a_simd_tail(
    states: &mut [u16],
    cols: &[u8],
    window_len: usize,
    out: &mut [u8],
    groups: usize,
) {
    let lanes = simd::NL2_WINDOW_LANES;
    for (w, state) in states.iter_mut().enumerate().skip(groups * lanes) {
        let base = window_group_base(w, window_len);
        let lane = w - groups * lanes;
        let mut x = *state;
        for i in 0..window_len {
            let idx = base + i * lanes + lane;
            let (s, next) = simd::nl2_step(x, cols[idx]);
            x = next;
            out[idx] = s;
        }
        *state = x;
    }
}

/// Byte offset of the start of `window`'s 16-lane group in the padded group-major
/// layout. For a full group this is `(w/16) * window_len * 16`; the final partial
/// group is padded to a full sixteen lanes.
#[inline]
fn window_group_base(window: usize, window_len: usize) -> usize {
    (window / simd::NL2_WINDOW_LANES) * window_len * simd::NL2_WINDOW_LANES
}

/// Padded batch length: `ceil(windows / 16) * 16 * window_len` bytes.
#[inline]
fn padded_batch_bytes(windows: usize, window_len: usize) -> usize {
    windows.div_ceil(simd::NL2_WINDOW_LANES) * simd::NL2_WINDOW_LANES * window_len
}

fn validate_batch(
    windows: usize,
    cols_len: usize,
    window_len: usize,
    out_len: usize,
) -> Result<()> {
    let expected = padded_batch_bytes(windows, window_len);
    if cols_len != expected || out_len != expected {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "window batch expects {expected} padded bytes (cols={cols_len}, out={out_len})"
            ),
        });
    }
    Ok(())
}

/// Start states of every `window_len`-sized window of `core`, via one prefix
/// scan over the block summaries (byte-exact for the genesis object).
pub fn window_start_states(core: &[u8], window_len: usize) -> Result<Vec<u16>> {
    if window_len == 0 {
        return Err(EkbloomError::InvalidArgument {
            what: "window_len must be non-zero".to_string(),
        });
    }
    let mut summaries: Vec<Affine> = Vec::with_capacity(core.len().div_ceil(window_len));
    let mut lo = 0usize;
    while lo < core.len() {
        let hi = lo.saturating_add(window_len).min(core.len());
        summaries.push(block_summary(&core[lo..hi]));
        lo = hi;
    }
    let prefix = prefix_scan(&summaries);
    Ok(prefix[..summaries.len()]
        .iter()
        .map(|a| a.apply(0))
        .collect())
}

/// Build the group-major step-major input for `windows` equal-length windows whose
/// step `i` byte lives at `core[offset(k) + i]`.
fn step_major_input(
    windows: usize,
    window_len: usize,
    mut byte_at: impl FnMut(usize, usize) -> u8,
) -> Result<Vec<u8>> {
    let lanes = simd::NL2_WINDOW_LANES;
    let total = padded_batch_bytes(windows, window_len);
    let mut cols = vec![0u8; total];
    for w in 0..windows {
        let base = window_group_base(w, window_len);
        let lane = w % lanes;
        for i in 0..window_len {
            cols[base + i * lanes + lane] = byte_at(w, i);
        }
    }
    Ok(cols)
}

// ---------------------------------------------------------------------------
// Kernel B — independent long blocks.
// ---------------------------------------------------------------------------

/// The block summaries and block slices of `core`, `block` bytes each.
fn block_layout(core: &[u8], block: usize) -> Result<(Vec<Affine>, Vec<&[u8]>)> {
    if block == 0 {
        return Err(EkbloomError::InvalidArgument {
            what: "block size must be non-zero".to_string(),
        });
    }
    let mut summaries = Vec::with_capacity(core.len().div_ceil(block));
    let mut slices: Vec<&[u8]> = Vec::with_capacity(core.len().div_ceil(block));
    let mut lo = 0usize;
    while lo < core.len() {
        let hi = lo.saturating_add(block).min(core.len());
        summaries.push(block_summary(&core[lo..hi]));
        slices.push(&core[lo..hi]);
        lo = hi;
    }
    Ok((summaries, slices))
}

/// Every block's exact start state from one associative prefix scan.
pub fn block_start_states(core: &[u8], block: usize) -> Result<Vec<u16>> {
    let (summaries, _) = block_layout(core, block)?;
    let prefix = prefix_scan(&summaries);
    Ok(prefix[..summaries.len()]
        .iter()
        .map(|a| a.apply(0))
        .collect())
}

/// Kernel B, pure-parallel threads: summaries + prefix scan + per-block decode
/// across `threads` — the [`crate::prefix::reconstruct_blocks`] path.
pub fn kernel_b_threads(core: &[u8], block: usize, threads: usize) -> Result<Vec<u8>> {
    let (summaries, slices) = block_layout(core, block)?;
    let prefix = prefix_scan(&summaries);
    let starts: Vec<u16> = prefix[..summaries.len()]
        .iter()
        .map(|a| a.apply(0))
        .collect();
    let parts = crate::prefix::reconstruct_blocks(&starts, &slices, threads)?;
    Ok(parts.concat())
}

/// Kernel B, vectorised across sixteen blocks per AVX2 register, optionally across
/// `threads`.
///
/// For each group of sixteen full blocks the input is transposed to the group-major
/// step-major layout, reconstructed with [`crate::simd::nl2_windows16_avx2`], and
/// scattered back; the `< 16`-block tail and the final partial block are decoded
/// scalar. Byte-exact to [`kernel_b_threads`].
pub fn kernel_b_simd(core: &[u8], block: usize, threads: usize) -> Result<Vec<u8>> {
    let starts = block_start_states(core, block)?;
    let n = core.len();
    let lanes = simd::NL2_WINDOW_LANES;
    let full = n / block;
    let rem = n % block;
    let full_groups = full / lanes;

    let mut out = vec![0u8; n];

    // Reconstruct one group's sixteen blocks and scatter the result into
    // `out_slice` (which covers exactly that group's contiguous core range).
    let run_group = |g: usize, out_slice: &mut [u8]| -> Result<()> {
        let first_block = g * lanes;
        let cols = step_major_input(lanes, block, |k, i| core[(first_block + k) * block + i])?;
        let mut states = [0u16; simd::NL2_WINDOW_LANES];
        states.copy_from_slice(&starts[first_block..first_block + lanes]);
        let mut group_out = vec![0u8; block * lanes];
        simd::nl2_windows16_avx2(&mut states, &cols, block, &mut group_out)?;
        for i in 0..block {
            for k in 0..lanes {
                out_slice[k * block + i] = group_out[i * lanes + k];
            }
        }
        Ok(())
    };

    let workers = threads.max(1).min(full_groups.max(1));
    if full_groups > 0 && workers <= 1 {
        for g in 0..full_groups {
            let lo = g * lanes * block;
            run_group(g, &mut out[lo..lo + lanes * block])?;
        }
    } else if full_groups > 0 {
        // Disjoint group ranges, one per worker.
        let chunk = full_groups.div_ceil(workers);
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut lo_g = 0usize;
        while lo_g < full_groups {
            let hi_g = lo_g.saturating_add(chunk).min(full_groups);
            ranges.push((lo_g, hi_g));
            lo_g = hi_g;
        }
        // Split the output into the matching disjoint mutable slices up front, so
        // each worker owns its bytes and no `&mut` is shared.
        let mut rest: &mut [u8] = &mut out[..];
        let mut slices: Vec<&mut [u8]> = Vec::with_capacity(ranges.len());
        for &(lo_g, hi_g) in &ranges {
            let take = (hi_g - lo_g) * lanes * block;
            let (head, tail) = rest.split_at_mut(take);
            rest = tail;
            slices.push(head);
        }
        std::thread::scope(|scope| -> Result<()> {
            let mut handles = Vec::new();
            for (slice, &(lo_g, hi_g)) in slices.into_iter().zip(ranges.iter()) {
                let run = &run_group;
                handles.push(scope.spawn(move || -> Result<()> {
                    for g in lo_g..hi_g {
                        let local = g - lo_g;
                        let lo = local * lanes * block;
                        run(g, &mut slice[lo..lo + lanes * block])?;
                    }
                    Ok(())
                }));
            }
            for handle in handles {
                handle.join().map_err(|_| EkbloomError::InvalidArgument {
                    what: "kernel B vector worker panicked".to_string(),
                })??;
            }
            Ok(())
        })?;
    }

    // Tail: the remaining full blocks (`full_groups*16 .. full`) plus the final
    // partial block, decoded scalar from their exact start states.
    for (b, &start) in starts
        .iter()
        .enumerate()
        .take(full)
        .skip(full_groups * lanes)
    {
        let lo = b * block;
        let mut x = start;
        for i in 0..block {
            let (s, next) = simd::nl2_step(x, core[lo + i]);
            x = next;
            out[lo + i] = s;
        }
    }
    if rem > 0 {
        let lo = full * block;
        let mut x = starts[full];
        for i in 0..rem {
            let (s, next) = simd::nl2_step(x, core[lo + i]);
            x = next;
            out[lo + i] = s;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{OrMachine, NL2};

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

    /// Authoritative symbols for one window from start state `x0`.
    fn oracle(x0: u16, r: &[u8]) -> Vec<u8> {
        let mut x = x0;
        let mut out = Vec::with_capacity(r.len());
        for &b in r {
            let (s, next) = simd::nl2_step(x, b);
            out.push(s);
            x = next;
        }
        out
    }

    #[test]
    fn kernel_a_variants_are_byte_exact() {
        for len in [0usize, 1, 2, 3, 15, 16, 17, 31, 32, 33, 64, 65, 100, 255] {
            let r = lcg(0x0A0A ^ len as u64, len);
            for &x0 in &[0u16, 1, 0x1234, 0xFFFF, 0x8000, 0x00FF] {
                let want = oracle(x0, &r);
                let mut a = Vec::new();
                kernel_a_scalar(x0, &r, &mut a);
                let mut b = Vec::new();
                kernel_a_unrolled(x0, &r, &mut b);
                let mut c = Vec::new();
                kernel_a_table(x0, &r, &mut c);
                assert_eq!(a, want, "scalar len={len} x0={x0:#06x}");
                assert_eq!(b, want, "unrolled len={len} x0={x0:#06x}");
                assert_eq!(c, want, "table len={len} x0={x0:#06x}");
            }
        }
    }

    #[test]
    fn kernel_a_window_batches_match_scalar_and_oracle() {
        for window_len in [1usize, 2, 16, 31, 32, 64] {
            for windows in [1usize, 15, 16, 17, 32, 33] {
                let padded =
                    windows.div_ceil(simd::NL2_WINDOW_LANES) * simd::NL2_WINDOW_LANES * window_len;
                let mut cols = vec![0u8; padded];
                for (i, b) in cols.iter_mut().enumerate() {
                    *b = lcg(0xBEEF ^ (i as u64), 1)[0];
                }
                let mut s = 0x1357_9BDFu64;
                let mut base_states: Vec<u16> = (0..windows)
                    .map(|_| {
                        s = s
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        (s >> 33) as u16
                    })
                    .collect();
                let mut base_states2 = base_states.clone();
                let mut scale = vec![0u8; padded];
                let mut simd_out = vec![0u8; padded];
                kernel_a_scalar_windows(&mut base_states, &cols, window_len, &mut scale).unwrap();
                kernel_a_simd_windows(&mut base_states2, &cols, window_len, &mut simd_out).unwrap();
                assert_eq!(simd_out, scale, "windows={windows} len={window_len}");
                // Cross-check the group-major layout against the per-window oracle.
                for w in 0..windows {
                    let g = w / simd::NL2_WINDOW_LANES;
                    let lane = w % simd::NL2_WINDOW_LANES;
                    let mut r = Vec::with_capacity(window_len);
                    for i in 0..window_len {
                        r.push(cols[(g * window_len + i) * simd::NL2_WINDOW_LANES + lane]);
                    }
                    let want = oracle(
                        {
                            // Recompute the original start state.
                            let mut s = 0x1357_9BDFu64;
                            for _ in 0..=w {
                                s = s
                                    .wrapping_mul(6364136223846793005)
                                    .wrapping_add(1442695040888963407);
                            }
                            (s >> 33) as u16
                        },
                        &r,
                    );
                    for (i, &w_byte) in want.iter().enumerate().take(window_len) {
                        let idx = (g * window_len + i) * simd::NL2_WINDOW_LANES + lane;
                        assert_eq!(simd_out[idx], w_byte, "w={w} i={i}");
                    }
                }
            }
        }
    }

    #[test]
    fn kernel_b_matches_sequential_decode() {
        for n in [0usize, 1, 15, 16, 63, 64, 65, 257, 1000, 4097] {
            let core = lcg(0xC0DE ^ n as u64, n);
            let want = NL2.decode(&core, n as u64).unwrap();
            for &block in &[1usize, 7, 16, 64, 256, 1024] {
                if block == 0 {
                    continue;
                }
                let threads_seq = kernel_b_threads(&core, block, 1).unwrap();
                assert_eq!(threads_seq, want, "threads n={n} block={block}");
                for &threads in &[2usize, 4, 8] {
                    let got = kernel_b_threads(&core, block, threads).unwrap();
                    assert_eq!(got, want, "threads={threads} n={n} block={block}");
                }
                for &threads in &[1usize, 2, 4, 8] {
                    let got = kernel_b_simd(&core, block, threads).unwrap();
                    assert_eq!(got, want, "simd threads={threads} n={n} block={block}");
                }
            }
        }
    }

    #[test]
    fn start_states_match_the_prefix_scan() {
        let core = lcg(0x5151, 2048);
        for &window in &[1usize, 7, 16, 64, 100] {
            let starts = window_start_states(&core, window).unwrap();
            let block_starts = block_start_states(&core, window).unwrap();
            assert_eq!(starts, block_starts);
        }
    }
}
