//! Independent adversarial review of the single `unsafe` block in `src/simd.rs`
//! (`gather_bytes_avx2`, an AVX2 `_mm256_i32gather_epi32` path).
//!
//! Every test drives both `gather_bytes` (AVX2 when available, else scalar) and
//! the `gather_bytes_scalar` reference, and asserts byte-for-byte agreement plus
//! clean `Err` (never a panic, never an out-of-bounds read) for the cases a
//! reviewer must probe: offsets at `len-1`/`len`/`len+1`, empty `hot`, offsets
//! beyond `u32::MAX` (32-bit index truncation), and long lists with scalar tails.
//! The AVX2 path is only taken when `avx2_available()`; on other machines the
//! equality still holds trivially.

use ekbloom::simd::{avx2_available, gather_bytes, gather_bytes_scalar};
use ekbloom::EkbloomError;

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

/// Both paths must return the same bytes and match a straightforward reference.
fn assert_agree(hot: &[u8], offs: &[u64]) {
    let mut scalar = Vec::new();
    gather_bytes_scalar(hot, offs, &mut scalar).expect("scalar gather");
    let mut simd = Vec::new();
    gather_bytes(hot, offs, &mut simd).expect("simd gather");
    assert_eq!(simd, scalar, "simd != scalar for {} offsets", offs.len());
    let reference: Vec<u8> = offs.iter().map(|&o| hot[o as usize]).collect();
    assert_eq!(scalar, reference, "scalar != reference");
}

/// Both paths must fail identically, with no partial output.
fn assert_both_err(hot: &[u8], offs: &[u64]) {
    let mut scalar = Vec::new();
    let e1 = gather_bytes_scalar(hot, offs, &mut scalar).unwrap_err();
    let mut simd = Vec::new();
    let e2 = gather_bytes(hot, offs, &mut simd).unwrap_err();
    assert!(scalar.is_empty(), "scalar produced partial output");
    assert!(simd.is_empty(), "simd produced partial output");
    assert_eq!(
        std::mem::discriminant(&e1),
        std::mem::discriminant(&e2),
        "error kinds differ: {e1} vs {e2}"
    );
}

/// Offsets at `len-1` (valid), `len`, `len+1` (both out of bounds).
#[test]
fn boundary_offsets_len_minus_one_len_len_plus_one() {
    let hot = lcg(1, 4096);
    let n = hot.len() as u64;
    assert_agree(&hot, &[n - 1]);
    assert_agree(&hot, &[n - 1; 8]); // forces the boundary/scalar group branch
    assert_both_err(&hot, &[n]);
    assert_both_err(&hot, &[n + 1]);
    assert_both_err(&hot, &[0, n]);
    // The 4-byte gather guard is `index + 4 <= len`, so `len-4` is safe and
    // `len-3` must fall back to scalar; both must agree with the reference.
    assert_agree(&hot, &[n - 4, n - 3, n - 2, n - 1, n - 4]);
}

/// Empty `hot`: no offsets is fine; any offset is out of bounds; never panics.
#[test]
fn empty_hot_array() {
    let hot: Vec<u8> = Vec::new();
    assert_agree(&hot, &[]);
    assert_both_err(&hot, &[0]);
    assert_both_err(&hot, &[0; 8]);
}

/// 32-bit truncation probe: offsets `> u32::MAX` that would land *in bounds* if
/// silently truncated to `u32`/`i32`. A truncating implementation would return
/// `hot[5]` instead of an error; the contract forbids that.
#[test]
fn offsets_beyond_u32_max_do_not_truncate() {
    let hot = lcg(2, 1 << 20); // large enough that the AVX2 path is active
    let n = hot.len() as u64;

    // (1<<32) + 5 truncates to 5 (in bounds) under a u32 cast — must NOT be read.
    let truncating = (1u64 << 32) + 5;
    assert!(truncating % (1 << 32) == 5);
    assert_both_err(&hot, &[truncating]);
    assert_both_err(&hot, &[0, truncating, 7]);
    assert_both_err(&hot, &[truncating; 8]); // a full vector group

    // The u32::MAX neighborhood, and the u64 overflow path.
    assert_both_err(&hot, &[u32::MAX as u64]);
    assert_both_err(&hot, &[u64::from(u32::MAX) + 1]);
    assert_both_err(&hot, &[u64::MAX]); // Overflow, not OutOfBounds

    // Sanity: the same indices *modulo* 2^32 are genuinely in bounds, so the
    // error above is real rejection, not an artefact of a tiny `hot`.
    let in_bounds = 5u64;
    assert!(in_bounds < n);
    assert_agree(&hot, &[in_bounds]);
}

/// Scalar-tail handling: list lengths that are not multiples of eight, including
/// boundary indices, must agree with the reference.
#[test]
fn scalar_tail_lengths_agree() {
    let hot = lcg(3, 4096);
    let n = hot.len() as u64;
    for extra in 1..=7usize {
        let mut offs: Vec<u64> = (0..8usize).map(|k| (k as u64 * 37) % n).collect();
        for k in 0..extra {
            offs.push(if k % 2 == 0 {
                n - 1
            } else {
                (k as u64 * 91) % n
            });
        }
        assert_agree(&hot, &offs);
    }
}

/// Mixed adversarial groups: exactly one boundary index inside otherwise
/// vectorisable groups forces the scalar branch; results must still match.
#[test]
fn mixed_boundary_and_vector_groups_agree() {
    let hot = lcg(4, 8192);
    let n = hot.len() as u64;
    let mut offs = Vec::new();
    for g in 0..2000u64 {
        for k in 0..8u64 {
            if k == (g % 8) {
                offs.push(n - 1); // boundary element
            } else {
                offs.push((g * 8 + k) % n);
            }
        }
    }
    assert_agree(&hot, &offs);
    assert!(offs.len() % 8 == 0);
}

/// A very long list (with a non-multiple-of-eight tail) still agrees, exercising
/// the vector loop, the scalar-group branch, and the scalar tail together.
#[test]
fn long_list_with_tail_agrees() {
    let hot = lcg(5, 1 << 16);
    let n = hot.len() as u64;
    let mut s = 0x0123_4567_89AB_CDEFu64;
    let mut offs: Vec<u64> = (0..100_003usize)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 19) % n
        })
        .collect();
    // Force some boundary indices into the middle and the tail.
    offs[8] = n - 1;
    offs[12345] = n - 2;
    offs[100_002] = n - 1;
    assert_agree(&hot, &offs);
}

/// Gating sanity: whatever `avx2_available()` reports, `gather_bytes` is total
/// and equal to the scalar reference (documented rather than assumed).
#[test]
fn gating_is_reported_and_total() {
    let hot = lcg(6, 1024);
    let offs: Vec<u64> = (0..64u64).collect();
    assert_agree(&hot, &offs);
    // The claim "AVX2 path is only taken when available" is enforced in src;
    // here we merely record availability so a failing run is diagnosable.
    eprintln!("avx2_available() = {}", avx2_available());
}

/// Error-kind fidelity for the overflow case.
#[test]
fn overflow_offset_is_overflow_not_outofbounds() {
    let hot = lcg(7, 256);
    let mut simd = Vec::new();
    let e = gather_bytes(&hot, &[u64::MAX], &mut simd).unwrap_err();
    assert!(matches!(e, EkbloomError::Overflow { .. }), "got {e}");
}
