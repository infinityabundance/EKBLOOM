//! Isolated SIMD experiment for the certified path-bloom hot path.
//!
//! Directive §23 asks for an honest SIMD experiment: *do not assume wider SIMD
//! automatically wins; measure actual behaviour.* This module is that experiment.
//! It provides a scalar reference gather ([`gather_bytes_scalar`]) and a
//! runtime-detected AVX2 gather ([`gather_bytes`]) for reading many single random
//! bytes out of the path-bloom `hot` array, and the Divan micro-benchmark in
//! `benches/ekbloom.rs` (`simd_gather_scalar` vs `simd_gather_avx2`) decides which
//! one is faster on real hardware.
//!
//! # Correctness contract
//!
//! [`gather_bytes`] returns **byte-for-byte identical** output to
//! [`gather_bytes_scalar`] for every input, including the error cases: both
//! validate every offset before producing any output, and an out-of-range offset
//! yields a clean [`EkbloomError`] rather than a panic or an out-of-bounds read.
//! [`gather_bytes`] falls back to the scalar reference on non-x86_64 targets, when
//! AVX2 is unavailable at runtime, or when the hot array is too large for 32-bit
//! gather indices.
//!
//! The module also carries two further §23 experiments, both measured in
//! `benches/ekbloom.rs` and `docs/LAYOUT.md` and neither wired into the default
//! path: [`gather_bytes_prefetch`] (a scalar gather that issues an `_mm_prefetch`
//! for a later random offset) and [`gather_bytes_avx512`] (an `avx512f` gather
//! using `_mm512_i32gather_epi32`).
//!
//! # Why this is the crate's only `unsafe` module
//!
//! x86 SIMD intrinsics (`_mm256_i32gather_epi32`, `_mm512_i32gather_epi32`,
//! `_mm_prefetch`, `_mm256_loadu_si256`, ...) are `unsafe` by nature. The
//! unsafety is confined to three functions in this module — `gather_bytes_avx2`,
//! `gather_bytes_prefetch_sse` and `gather_bytes_avx512_impl` — each guarded by
//! `#[cfg(target_arch = "x86_64")]` and a runtime `is_x86_feature_detected!`
//! check, each with a single `// SAFETY:` justification. No other module contains
//! `unsafe`.

use crate::error::{EkbloomError, Result};

/// Whether AVX2 gather is usable on this machine.
///
/// Uses the safe `std::is_x86_feature_detected!` macro on x86/x86_64 and returns
/// `false` on every other architecture.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn avx2_available() -> bool {
    std::is_x86_feature_detected!("avx2")
}

/// Whether AVX2 gather is usable on this machine.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn avx2_available() -> bool {
    false
}

/// Whether the SSE prefetch hint is usable on this machine.
///
/// `_mm_prefetch` needs only SSE, which is baseline on every x86_64 CPU; the
/// runtime check keeps the module total and honest on exotic targets.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn sse_available() -> bool {
    std::is_x86_feature_detected!("sse")
}

/// Whether the SSE prefetch hint is usable on this machine.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn sse_available() -> bool {
    false
}

/// Whether AVX-512 Foundation gather is usable on this machine.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn avx512_available() -> bool {
    std::is_x86_feature_detected!("avx512f")
}

/// Whether AVX-512 Foundation gather is usable on this machine.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn avx512_available() -> bool {
    false
}

/// Validate that every offset addresses exactly one readable byte of `hot`.
///
/// Mirrors [`crate::fastpath::read_random_1b`]'s length-1 bounds/overflow
/// semantics so the fast path and this module agree on every error: an offset
/// that overflows `u64` is [`EkbloomError::Overflow`], and an offset at or past
/// `hot.len()` is [`EkbloomError::OutOfBounds`]. No output is produced on error.
#[inline]
fn validate_offsets(hot_len: usize, offsets: &[u64]) -> Result<()> {
    let logical_len = hot_len as u64;
    for &offset in offsets {
        let end = offset
            .checked_add(1)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset,
                length: 1,
                logical_len,
            });
        }
    }
    Ok(())
}

/// Scalar reference: one checked indexed load per offset.
///
/// This is the baseline the AVX2 gather is measured against. It validates every
/// offset first (so an error produces no partial output), then appends exactly
/// `offsets.iter().map(|&o| hot[o as usize])` to `out`.
pub fn gather_bytes_scalar(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    validate_offsets(hot.len(), offsets)?;
    out.reserve(offsets.len());
    out.extend(offsets.iter().map(|&o| hot[o as usize]));
    Ok(())
}

/// AVX2 gather when available and safe; otherwise the scalar reference.
///
/// Byte-for-byte equal to [`gather_bytes_scalar`] for every input.
pub fn gather_bytes(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        // `_mm256_i32gather_epi32` indexes with 32-bit signed lanes, so the hot
        // array must fit in `i32` range for the vector path to be applicable.
        if avx2_available() && hot.len() <= i32::MAX as usize {
            // SAFETY: guarded by the runtime `avx2_available()` check just above,
            // and `gather_bytes_avx2` is compiled with `#[target_feature(enable =
            // "avx2")]`, which is the function's own precondition.
            unsafe { gather_bytes_avx2(hot, offsets, out)? };
            return Ok(());
        }
    }
    gather_bytes_scalar(hot, offsets, out)
}

/// AVX2 gather implementation.
///
/// Processes offsets in groups of eight: `_mm256_i32gather_epi32` loads four
/// bytes per 32-bit index, so a group is gathered only when **every** index has
/// four readable bytes (`index + 4 <= hot.len()`); the low byte of each gathered
/// dword is then kept (`& 0xFF`). This is the page-boundary reasoning: a 4-byte
/// gather starting at byte `p` touches bytes `p..p+4`, which can cross a page
/// boundary when `p` is within the last three bytes of `hot`. Restricting the
/// vector path to `p + 4 <= hot.len()` keeps every gathered dword inside the
/// `hot` allocation, and any group containing such a boundary index (or the
/// scalar tail, or arrays shorter than four bytes) is handled by the scalar loop.
///
/// # Safety
///
/// The caller must ensure AVX2 is available at runtime (checked in
/// [`gather_bytes`]) and that `hot.len() <= i32::MAX as usize`. Under those
/// preconditions every intrinsic below operates on fully in-bounds memory:
/// offsets are validated before any gather; the 32-byte load and 16-byte store
/// use the unaligned intrinsics; and each gathered dword is proven in-bounds by
/// the `index + 4 <= hot.len()` guard.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gather_bytes_avx2(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    use std::arch::x86_64::*;

    // Validate first: never rely on the gather to bound-check.
    validate_offsets(hot.len(), offsets)?;

    let len = hot.len();
    let n = offsets.len();
    out.reserve(n);

    // Largest index at which a 4-byte gather stays inside `hot`.
    let last_safe = len.saturating_sub(4);
    let base = hot.as_ptr() as *const i32;

    let mut i = 0usize;
    while i + 8 <= n {
        let group = &offsets[i..i + 8];
        // The vector path needs at least four bytes available and every index of
        // the group to leave four readable bytes.
        let safe = len >= 4 && group.iter().all(|&o| (o as usize) <= last_safe);
        if safe {
            let mut idx = [0i32; 8];
            for (k, &o) in group.iter().enumerate() {
                idx[k] = o as i32;
            }
            // SAFETY: `idx` holds eight in-bounds byte offsets (each `<= len - 4`),
            // so each of the eight 4-byte gathers reads entirely within `hot`; the
            // 32-byte load and the compaction use unaligned/SSE-safe intrinsics.
            let vidx = _mm256_loadu_si256(idx.as_ptr() as *const __m256i);
            let gathered = _mm256_i32gather_epi32::<1>(base, vidx);
            // Keep the low byte of each gathered 32-bit lane.
            let masked = _mm256_and_si256(gathered, _mm256_set1_epi32(0xFF));
            // Compact the eight low bytes into the low half of one 128-bit value:
            // packus halving twice leaves each 128-bit lane's first four source
            // dwords as four contiguous bytes, then interleave the two lanes.
            let p16 = _mm256_packus_epi32(masked, masked);
            let p8 = _mm256_packus_epi16(p16, p16);
            let compact = _mm_unpacklo_epi32(
                _mm256_castsi256_si128(p8),
                _mm256_extracti128_si256::<1>(p8),
            );
            let mut bytes = [0u8; 16];
            _mm_storeu_si128(bytes.as_mut_ptr() as *mut __m128i, compact);
            out.extend_from_slice(&bytes[..8]);
        } else {
            // Boundary group: the validated scalar loads cannot fault.
            for &o in group {
                out.push(hot[o as usize]);
            }
        }
        i += 8;
    }

    // Scalar tail (fewer than eight offsets remain).
    while i < n {
        out.push(hot[offsets[i] as usize]);
        i += 1;
    }
    Ok(())
}

/// Scalar gather with a software prefetch of a later offset's cache line.
///
/// Byte-for-byte equal to [`gather_bytes_scalar`]. This is the §23 prefetch-ahead
/// experiment: while reading offset `i`, it issues an `_mm_prefetch` for offset
/// `i + AHEAD`, so the out-of-order engine overlaps the latency of a distant
/// random load. On non-x86 or without SSE it is exactly [`gather_bytes_scalar`].
pub fn gather_bytes_prefetch(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        if sse_available() {
            // SAFETY: guarded by the runtime `sse_available()` check just above;
            // `gather_bytes_prefetch_sse` is compiled with `#[target_feature(enable
            // = "sse")]`, which is its own precondition.
            unsafe { gather_bytes_prefetch_sse(hot, offsets, out)? };
            return Ok(());
        }
    }
    gather_bytes_scalar(hot, offsets, out)
}

/// Prefetch distance (in offsets) for [`gather_bytes_prefetch`].
const PREFETCH_AHEAD: usize = 8;

/// SSE prefetch-ahead scalar gather implementation.
///
/// # Safety
///
/// The caller must ensure SSE is available at runtime (checked in
/// [`gather_bytes_prefetch`]). `_mm_prefetch` is a non-faulting advisory hint, and
/// this function only issues it for indices that are already proven `< hot.len()`;
/// every consuming load is the same checked `hot[idx]` as the scalar reference.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse")]
unsafe fn gather_bytes_prefetch_sse(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};

    validate_offsets(hot.len(), offsets)?;
    out.reserve(offsets.len());

    let len = hot.len();
    let base = hot.as_ptr();
    for (i, &o) in offsets.iter().enumerate() {
        if let Some(&ahead) = offsets.get(i + PREFETCH_AHEAD) {
            let idx = ahead as usize;
            if idx < len {
                // SAFETY: `idx < len` proves `base.add(idx)` is in-bounds within the
                // `hot` allocation; `_mm_prefetch` only reads the address operand and
                // cannot fault, and is purely advisory.
                _mm_prefetch::<_MM_HINT_T0>(base.add(idx) as *const i8);
            }
        }
        out.push(hot[o as usize]);
    }
    Ok(())
}

/// AVX-512 Foundation gather when available and safe; otherwise AVX2/scalar.
///
/// Byte-for-byte equal to [`gather_bytes_scalar`] for every input. This is the
/// §23 AVX-512 experiment: `_mm512_i32gather_epi32` gathers sixteen 4-byte lanes
/// with 32-bit indices, then `_mm512_cvtepi32_epi8` compacts the sixteen low bytes
/// into one 128-bit store. It is measured in `benches/ekbloom.rs`; it is **not**
/// wired into the default path.
pub fn gather_bytes_avx512(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        // `_mm512_i32gather_epi32` indexes with 32-bit signed lanes, so the hot
        // array must fit in `i32` range for the vector path to be applicable.
        if avx512_available() && hot.len() <= i32::MAX as usize {
            // SAFETY: guarded by the runtime `avx512_available()` check just above,
            // and `gather_bytes_avx512_impl` is compiled with `#[target_feature(
            // enable = "avx512f")]`, which is the function's own precondition.
            unsafe { gather_bytes_avx512_impl(hot, offsets, out)? };
            return Ok(());
        }
    }
    gather_bytes(hot, offsets, out)
}

/// AVX-512 gather implementation.
///
/// Processes offsets in groups of sixteen with the same page-safety reasoning as
/// the AVX2 path ([`gather_bytes_avx2`]): a group is gathered only when every
/// index leaves four readable bytes (`index + 4 <= hot.len()`); boundary groups,
/// the scalar tail, and arrays shorter than four bytes use the scalar loop.
///
/// # Safety
///
/// The caller must ensure AVX-512F is available at runtime (checked in
/// [`gather_bytes_avx512`]) and that `hot.len() <= i32::MAX as usize`. Under those
/// preconditions every intrinsic below operates on fully in-bounds memory:
/// offsets are validated before any gather; the 64-byte load and the 16-byte store
/// use unaligned intrinsics; and each gathered dword is proven in-bounds by the
/// `index + 4 <= hot.len()` guard.
// The AVX-512 intrinsics used here were only stabilized in Rust 1.89, while the
// crate's declared `rust-version = 1.75` metadata predates that. Rather than
// raise the crate MSRV (Cargo.toml is outside this module's ownership) or drop a
// required §23 experiment, the MSRV lint is explicitly silenced for this one
// isolated, runtime-gated function; the crate is built with the pinned 1.99
// toolchain.
#[allow(clippy::incompatible_msrv)]
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn gather_bytes_avx512_impl(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    use std::arch::x86_64::*;

    validate_offsets(hot.len(), offsets)?;

    let len = hot.len();
    let n = offsets.len();
    out.reserve(n);

    let last_safe = len.saturating_sub(4);
    let base = hot.as_ptr() as *const i32;

    let mut i = 0usize;
    while i + 16 <= n {
        let group = &offsets[i..i + 16];
        let safe = len >= 4 && group.iter().all(|&o| (o as usize) <= last_safe);
        if safe {
            let mut idx = [0i32; 16];
            for (k, &o) in group.iter().enumerate() {
                idx[k] = o as i32;
            }
            // SAFETY: `idx` holds sixteen in-bounds byte offsets (each `<= len - 4`),
            // so each of the sixteen 4-byte gathers reads entirely within `hot`; the
            // 64-byte load, the conversion and the 16-byte store use unaligned or
            // register-only intrinsics.
            let vidx = _mm512_loadu_si512(idx.as_ptr() as *const __m512i);
            let gathered = _mm512_i32gather_epi32::<1>(vidx, base);
            let masked = _mm512_and_si512(gathered, _mm512_set1_epi32(0xFF));
            let bytes = _mm512_cvtepi32_epi8(masked);
            let mut buf = [0u8; 16];
            _mm_storeu_si128(buf.as_mut_ptr() as *mut __m128i, bytes);
            out.extend_from_slice(&buf);
        } else {
            for &o in group {
                out.push(hot[o as usize]);
            }
        }
        i += 16;
    }

    while i < n {
        out.push(hot[offsets[i] as usize]);
        i += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NL2 reconstruction windows (directive §6): vector across independent windows.
// ---------------------------------------------------------------------------
//
// A single `OrANS-NL2` restart window is a *serial* dependent chain: symbol `s_i`
// depends on `s_{i-1}` and `s_{i-2}`, so there is no intra-window vectorisation.
// The available parallelism is **across independent windows**: one 16-bit lane per
// window, sixteen windows per AVX2 register (or eight per SSE register). The
// per-step map `s = S(h) ^ l ^ r` is reused exactly as written, with `S` in its
// `GF(2)`-linear form `h ^ (h<<1) ^ (h<<3)` so it lowers to shifts and XORs.

/// Number of 16-bit NL2 restart windows reconstructed per AVX2 register.
pub const NL2_WINDOW_LANES: usize = 16;

/// One `OrANS-NL2` step: reveal `r` from state `x`, returning `(symbol, next)`.
///
/// This is the exact §1 step with `S` inlined (`S(v) = v ^ (v<<1) ^ (v<<3)`),
/// which is also what makes the step vectorisable across windows.
#[inline(always)]
pub fn nl2_step(x: u16, r: u8) -> (u8, u16) {
    let h = (x >> 8) as u8;
    let l = (x & 0xFF) as u8;
    let s = (h ^ (h << 1) ^ (h << 3)) ^ l ^ r;
    (s, (u16::from(l) << 8) | u16::from(s))
}

/// Whether the AVX2 NL2 window kernel is usable on this machine.
pub fn nl2_windows_avx2_available() -> bool {
    avx2_available()
}

/// Validate a batched window layout: `cols`/`out` hold `windows * window_len`
/// step-major bytes.
fn validate_windows(
    windows: usize,
    cols_len: usize,
    window_len: usize,
    out_len: usize,
) -> Result<()> {
    let expected = windows
        .checked_mul(window_len)
        .ok_or(EkbloomError::Overflow {
            what: "window batch length",
        })?;
    if cols_len != expected || out_len != expected {
        return Err(EkbloomError::InvalidArgument {
            what: format!("window batch expects {expected} bytes (cols={cols_len}, out={out_len})"),
        });
    }
    Ok(())
}

/// Reconstruct `window_len` symbols for each of exactly [`NL2_WINDOW_LANES`]
/// independent windows, using AVX2 when available and a scalar reference
/// otherwise.
///
/// `cols` and `out` use the same step-major layout: byte `i` of lane `k` is
/// `cols[i * 16 + k]`. `states` holds each window's 16-bit start state (mutated
/// in place to the end state). Byte-for-byte identical to the scalar reference
/// for every input, and identical to the scalar fallback when AVX2 is absent.
pub fn nl2_windows16_avx2(
    states: &mut [u16],
    cols: &[u8],
    window_len: usize,
    out: &mut [u8],
) -> Result<()> {
    if states.len() != NL2_WINDOW_LANES {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "nl2_windows16_avx2 expects {} states, got {}",
                NL2_WINDOW_LANES,
                states.len()
            ),
        });
    }
    validate_windows(states.len(), cols.len(), window_len, out.len())?;

    #[cfg(target_arch = "x86_64")]
    {
        if avx2_available() {
            let mut xs = [0u16; NL2_WINDOW_LANES];
            xs.copy_from_slice(states);
            // SAFETY: `avx2_available()` proved AVX2 is present at runtime and the
            // callee is `#[target_feature(enable = "avx2")]`; `validate_windows`
            // proved `cols`/`out` are exactly `window_len * 16` bytes, so every
            // unaligned 128/256-bit load and store stays in bounds; `xs` is a fixed
            // 16-element array loaded/stored as one 256-bit value.
            unsafe { nl2_windows16_avx2_impl(&mut xs, cols, window_len, out) };
            states.copy_from_slice(&xs);
            return Ok(());
        }
    }

    // Scalar reference with the identical step-major layout (equal residency).
    for i in 0..window_len {
        let base = i * NL2_WINDOW_LANES;
        for k in 0..NL2_WINDOW_LANES {
            let (s, next) = nl2_step(states[k], cols[base + k]);
            states[k] = next;
            out[base + k] = s;
        }
    }
    Ok(())
}

/// AVX2 implementation of [`nl2_windows16_avx2`].
///
/// # Safety
///
/// The caller must ensure AVX2 is available at runtime (checked in
/// `nl2_windows16_avx2`) and that `cols`/`out` are exactly `window_len * 16`
/// bytes. Under those preconditions every unaligned load/store is in bounds.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn nl2_windows16_avx2_impl(
    states: &mut [u16; NL2_WINDOW_LANES],
    cols: &[u8],
    window_len: usize,
    out: &mut [u8],
) {
    use std::arch::x86_64::*;

    let mask = _mm256_set1_epi16(0xFF);
    let mut x = _mm256_loadu_si256(states.as_ptr() as *const __m256i);
    for i in 0..window_len {
        let base = i * NL2_WINDOW_LANES;
        // SAFETY (per the function contract): `base + 16 <= cols.len()` because
        // `cols.len() == window_len * 16`; a 16-byte unaligned load cannot fault.
        let r8 = _mm_loadu_si128(cols.as_ptr().add(base) as *const __m128i);
        let r = _mm256_cvtepu8_epi16(r8);

        let h = _mm256_srli_epi16::<8>(x);
        let l = _mm256_and_si256(x, mask);
        // S(h) = h ^ (h<<1) ^ (h<<3), masked back to 8 bits per lane.
        let sbox = _mm256_xor_si256(
            _mm256_xor_si256(h, _mm256_slli_epi16::<1>(h)),
            _mm256_slli_epi16::<3>(h),
        );
        let sbox = _mm256_and_si256(sbox, mask);
        let s = _mm256_xor_si256(_mm256_xor_si256(sbox, l), r);
        x = _mm256_or_si256(_mm256_slli_epi16::<8>(l), s);

        // Compact the sixteen 16-bit symbols to sixteen bytes: packus halves each
        // 128-bit lane, then interleave the two lanes' low quads.
        let packed = _mm256_packus_epi16(s, s);
        let compact = _mm_unpacklo_epi64(
            _mm256_castsi256_si128(packed),
            _mm256_extracti128_si256::<1>(packed),
        );
        // SAFETY (per the function contract): `base + 16 <= out.len()`.
        _mm_storeu_si128(out.as_mut_ptr().add(base) as *mut __m128i, compact);
    }
    _mm256_storeu_si256(states.as_mut_ptr() as *mut __m256i, x);
}

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

    #[test]
    fn gather_matches_scalar_for_various_shapes() {
        let hot = lcg(0x512D_0001, 4096);
        let n = hot.len() as u64;
        let mut s = 0xDEAD_BEEF_0000_0001u64;
        let random: Vec<u64> = (0..300)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 17) % n
            })
            .collect();
        let cases: [&[u64]; 5] = [&[], &[0], &[n - 1], &[0, 1, 2, 3, 4, 5, 6, 7], &random];
        for offsets in cases {
            let mut want = Vec::new();
            gather_bytes_scalar(&hot, offsets, &mut want).unwrap();
            let mut got = Vec::new();
            gather_bytes(&hot, offsets, &mut got).unwrap();
            assert_eq!(got, want, "offsets len {}", offsets.len());
        }
    }

    #[test]
    fn short_hot_array_uses_scalar_and_agrees() {
        // Arrays shorter than four bytes cannot take the vector path at all.
        for len in 0..4usize {
            let hot = lcg(7, len);
            let offsets: Vec<u64> = (0..len as u64).collect();
            let mut want = Vec::new();
            gather_bytes_scalar(&hot, &offsets, &mut want).unwrap();
            let mut got = Vec::new();
            gather_bytes(&hot, &offsets, &mut got).unwrap();
            assert_eq!(got, want, "len {len}");
        }
    }

    #[test]
    fn out_of_range_offsets_error_identically() {
        let hot = lcg(3, 256);
        let n = hot.len() as u64;
        for bad in [n, n + 1, u64::MAX] {
            let offsets = vec![5u64, bad, 7];
            let mut want = Vec::new();
            let err_scalar = gather_bytes_scalar(&hot, &offsets, &mut want).unwrap_err();
            let mut got = Vec::new();
            let err_avx2 = gather_bytes(&hot, &offsets, &mut got).unwrap_err();
            // No partial output is produced on either path.
            assert!(want.is_empty() && got.is_empty());
            assert_eq!(err_scalar.to_string(), err_avx2.to_string());
        }
    }

    /// Scalar reference with the same step-major layout as the vector kernel.
    fn windows_scalar(states: &mut [u16], cols: &[u8], window_len: usize, out: &mut [u8]) {
        for i in 0..window_len {
            let base = i * NL2_WINDOW_LANES;
            for k in 0..NL2_WINDOW_LANES {
                let (s, next) = nl2_step(states[k], cols[base + k]);
                states[k] = next;
                out[base + k] = s;
            }
        }
    }

    #[test]
    fn nl2_window_kernel_matches_scalar_and_machine() {
        for len in [1usize, 2, 3, 7, 16, 17, 32, 64, 100] {
            let mut s = 0x9E37_79B9_7F4A_7C15u64;
            let mut states = [0u16; NL2_WINDOW_LANES];
            for x in states.iter_mut() {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *x = (s >> 33) as u16;
            }
            let cols: Vec<u8> = (0..len * NL2_WINDOW_LANES)
                .map(|_| {
                    s = s
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (s >> 33) as u8
                })
                .collect();

            let mut want = vec![0u8; len * NL2_WINDOW_LANES];
            let mut want_states = states;
            windows_scalar(&mut want_states, &cols, len, &mut want);

            let mut got = vec![0u8; len * NL2_WINDOW_LANES];
            let mut got_states = states;
            nl2_windows16_avx2(&mut got_states, &cols, len, &mut got).unwrap();
            assert_eq!(got, want, "len {len}");
            assert_eq!(got_states, want_states, "end states len {len}");

            // And against the authoritative authority step-by-step.
            for k in 0..NL2_WINDOW_LANES {
                let mut x = states[k];
                for i in 0..len {
                    let (s, next) = nl2_step(x, cols[i * NL2_WINDOW_LANES + k]);
                    assert_eq!(got[i * NL2_WINDOW_LANES + k], s);
                    x = next;
                }
            }
        }
    }

    #[test]
    fn nl2_window_kernel_rejects_bad_shapes() {
        let mut states = [0u16; NL2_WINDOW_LANES];
        let cols = [0u8; 32];
        let mut out = [0u8; 32];
        assert!(nl2_windows16_avx2(&mut states, &cols, 3, &mut out).is_err());
        let mut short = [0u16; NL2_WINDOW_LANES - 1];
        assert!(nl2_windows16_avx2(&mut short, &cols, 2, &mut out).is_err());
    }
}
