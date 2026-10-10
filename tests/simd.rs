//! Integration tests for the isolated SIMD gather experiment ([`ekbloom::simd`]).
//!
//! The contract under test is that [`ekbloom::gather_bytes`] (AVX2 when available,
//! scalar otherwise) is **byte-for-byte identical** to
//! [`ekbloom::gather_bytes_scalar`] for every input, and that out-of-range offsets
//! produce a clean error rather than a panic or an out-of-bounds read. The equality
//! holds on every machine; the AVX2-specific cases are exercised only when
//! [`ekbloom::avx2_available`] reports true and are skipped gracefully otherwise.

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

/// Deterministic in-range offsets in `[0, n)`.
fn offsets(seed: u64, count: usize, n: u64) -> Vec<u64> {
    let mut s = seed;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 17) % n
        })
        .collect()
}

/// Assert both paths agree byte-for-byte and match a straightforward reference.
fn assert_agree(hot: &[u8], offs: &[u64]) {
    let mut scalar = Vec::new();
    gather_bytes_scalar(hot, offs, &mut scalar).expect("scalar gather");
    let mut simd = Vec::new();
    gather_bytes(hot, offs, &mut simd).expect("simd gather");
    assert_eq!(simd, scalar, "simd differs from scalar");

    let reference: Vec<u8> = offs.iter().map(|&o| hot[o as usize]).collect();
    assert_eq!(scalar, reference, "scalar differs from reference");
}

#[test]
fn empty_offsets_agree() {
    let hot = lcg(1, 1024);
    assert_agree(&hot, &[]);
}

#[test]
fn single_offset_agrees() {
    let hot = lcg(2, 1024);
    assert_agree(&hot, &[0]);
    assert_agree(&hot, &[511]);
    assert_agree(&hot, &[1023]);
}

#[test]
fn many_offsets_agree() {
    let hot = lcg(3, 1 << 16);
    let offs = offsets(0xBEEF, 50_000, hot.len() as u64);
    assert_agree(&hot, &offs);
}

#[test]
fn offsets_touching_last_byte_agree() {
    // The final byte of `hot` is the allocation/page-boundary case: a 4-byte gather
    // must be suppressed there and served by the scalar loop instead.
    let hot = lcg(4, 4096);
    let n = hot.len() as u64;
    let offs = vec![n - 1, n - 1, n - 2, n - 1, 0, n - 1];
    assert_agree(&hot, &offs);

    // A run of eight offsets all at the boundary forces the scalar group branch.
    let boundary = vec![n - 1; 8];
    assert_agree(&hot, &boundary);

    // Mixed: seven boundary offsets plus a valid aligned group.
    let mut mixed = vec![n - 1, n - 2, n - 3, n - 1, n - 1, n - 1, n - 1, n - 1];
    mixed.extend([0, 1, 2, 3, 4, 5, 6, 7]);
    assert_agree(&hot, &mixed);
}

#[test]
fn out_of_range_offsets_return_clean_err() {
    let hot = lcg(5, 256);
    let n = hot.len() as u64;

    // Past the end: OutOfBounds, no output.
    for bad in [n, n + 1, n + 1000] {
        let offs = vec![5u64, bad, 7];
        let mut scalar = Vec::new();
        let e1 = gather_bytes_scalar(&hot, &offs, &mut scalar).unwrap_err();
        let mut simd = Vec::new();
        let e2 = gather_bytes(&hot, &offs, &mut simd).unwrap_err();
        assert!(matches!(e1, EkbloomError::OutOfBounds { offset, .. } if offset == bad));
        assert!(matches!(e2, EkbloomError::OutOfBounds { offset, .. } if offset == bad));
        assert!(scalar.is_empty() && simd.is_empty(), "no partial output");
    }

    // Overflowing offset: Overflow, no output.
    let offs = vec![u64::MAX];
    let mut simd = Vec::new();
    let e = gather_bytes(&hot, &offs, &mut simd).unwrap_err();
    assert!(matches!(e, EkbloomError::Overflow { .. }));
    assert!(simd.is_empty());
}

#[test]
fn avx2_equality_when_available() {
    if !avx2_available() {
        // No AVX2: `gather_bytes` is exactly the scalar reference, already covered.
        eprintln!("avx2 unavailable: skipping AVX2-specific assertion");
        return;
    }
    // Large hot array so the vector path is taken for most groups.
    let hot = lcg(6, 1 << 20);
    let offs = offsets(0x5EED, 100_000, hot.len() as u64);
    assert_agree(&hot, &offs);
}
