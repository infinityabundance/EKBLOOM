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
//! # Why this is the only `unsafe` in the crate
//!
//! x86 SIMD intrinsics (`_mm256_i32gather_epi32`, `_mm256_loadu_si256`, ...) are
//! `unsafe` by nature. The unsafety is confined to **one** function,
//! `gather_bytes_avx2`, guarded by `#[cfg(target_arch = "x86_64")]` and a
//! runtime `is_x86_feature_detected!("avx2")` check, with a single `// SAFETY:`
//! justification. No other module contains `unsafe`.

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
}
