//! Optimised read paths — the speed ceiling of the manifestation frontier.
//!
//! The governing objective is to **spend manifested space to maximise speed**.
//! When a body carries a *certified path bloom* `P_D[j]` (see
//! [`crate::bloom_path`]), the bloom's `hot` array already holds the
//! authoritative symbol at every logical position. That turns reads into pure
//! memory movement:
//!
//! - sequential read = one `extend_from_slice` of the `hot` array,
//! - single-byte random read = one indexed `hot[pos]` lookup,
//! - range read = one `extend_from_slice` per range.
//!
//! No machine stepping, no branchy per-symbol decode, and no bloom probing is
//! performed on this path. The successor-state `cold` array is never touched, so
//! the hot/cold split of the path bloom is preserved (hot bytes are contiguous
//! and cache-friendly; cold bytes stay out of the read footprint).
//!
//! When no certified path bloom is present the fast paths **delegate** to
//! [`crate::reader::read`], which selects the next-best correct path (checkpoint
//! restart, full history, or authoritative machine replay).
//!
//! # Correctness contract
//!
//! Every fast path returns bytes identical to [`crate::reader::read`] for the
//! same request. This is enforced mechanically: the shell-acceptance checks in
//! [`certified_path_hot`] mirror [`crate::bloom_path::PathBloom::from_shell`], so
//! the fast path selects exactly the same shells as `reader::read` and therefore
//! the same authoritative bytes. The module and integration tests assert
//! byte-for-byte agreement across every manifestation profile.
//!
//! The implementation is entirely safe Rust: only checked arithmetic, safe
//! slices and iterators are used.
//!
//! A runtime-detected AVX2 gather for the random single-byte path was implemented
//! and measured (see [`crate::simd`] and `docs/reviews/performance.md`); it was
//! **slower** than this scalar loop on the reference machine, so it is deliberately
//! *not* wired into the default path. [`crate::simd::gather_bytes`] remains
//! available for callers and benchmarking.

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification};
use crate::error::{EkbloomError, Result};
use crate::manifest::EkbloomBody;
use crate::reader;
use crate::ReadRequest;

/// Prefix of a path bloom payload: `depth u16` ++ `logical_len u64` ++
/// `nblocks u64` (see [`crate::bloom_path`]).
const PATH_PREFIX_LEN: usize = 2 + 8 + 8;

/// CPU features relevant to choosing a fast path.
///
/// Detection uses the safe `std::is_x86_feature_detected!` macro on x86/x86_64;
/// on other architectures every field is `false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuFeatures {
    /// Streaming SIMD Extensions 2.
    pub sse2: bool,
    /// Advanced Vector Extensions 2.
    pub avx2: bool,
    /// AVX-512 Foundation.
    pub avx512f: bool,
}

/// Detect the CPU features of the current process.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn cpu_features() -> CpuFeatures {
    CpuFeatures {
        sse2: std::is_x86_feature_detected!("sse2"),
        avx2: std::is_x86_feature_detected!("avx2"),
        avx512f: std::is_x86_feature_detected!("avx512f"),
    }
}

/// Detect the CPU features of the current process.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn cpu_features() -> CpuFeatures {
    CpuFeatures::default()
}

/// Construct an [`EkbloomError::OutOfBounds`] for a rejected fast-path request.
#[inline]
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

/// The authoritative `hot` symbol array of the first certified path bloom that
/// covers the whole object, or `None` when none is present.
///
/// Acceptance mirrors [`crate::bloom_path::PathBloom::from_shell`] exactly so the
/// fast path uses the same shells — and therefore the same bytes — as
/// `reader::read`. The returned slice borrows the shell payload directly, so no
/// per-read allocation or copy of the symbol array occurs.
fn certified_path_hot(body: &EkbloomBody) -> Option<&[u8]> {
    for shell in &body.blooms {
        if shell.kind != BloomKind::Path
            || shell.status != Certification::Certified
            || shell.logical_len != body.logical_len
        {
            continue;
        }
        if let Some(hot) = path_hot_of(shell, body.logical_len) {
            return Some(hot);
        }
    }
    None
}

/// Extract the `hot` slice from a `Path` shell payload, applying the same
/// strict structural checks as [`crate::bloom_path::PathBloom::from_shell`].
fn path_hot_of(shell: &BloomShell, logical_len: u64) -> Option<&[u8]> {
    if !supported_depth(shell.depth) {
        return None;
    }
    let p = &shell.payload;
    if p.len() < PATH_PREFIX_LEN {
        return None;
    }
    let depth = u16::from_le_bytes([p[0], p[1]]);
    if depth != shell.depth {
        return None;
    }
    let payload_logical = u64::from_le_bytes(p[2..10].try_into().ok()?);
    if payload_logical != shell.logical_len {
        return None;
    }
    let nblocks = u64::from_le_bytes(p[10..18].try_into().ok()?);
    if nblocks != logical_len.div_ceil(u64::from(depth)) {
        return None;
    }
    if shell.entries != nblocks {
        return None;
    }
    let hot_len = usize::try_from(logical_len).ok()?;
    let cold_len = usize::try_from(nblocks).ok()?;
    let need = PATH_PREFIX_LEN
        .checked_add(hot_len)?
        .checked_add(cold_len.checked_mul(4)?)?;
    if p.len() != need {
        return None;
    }
    Some(&p[PATH_PREFIX_LEN..PATH_PREFIX_LEN + hot_len])
}

/// Fastest available byte-exact sequential extraction of the whole object into
/// `out` (appends; `out` is not cleared).
///
/// With a certified path bloom the whole object is one `extend_from_slice` of the
/// hot symbol array. Otherwise this delegates to [`reader::read`] over
/// `0..logical_len`.
pub fn read_sequential(body: &EkbloomBody, out: &mut Vec<u8>) -> Result<()> {
    if body.logical_len == 0 {
        return Ok(());
    }
    if let Some(hot) = certified_path_hot(body) {
        out.extend_from_slice(hot);
        return Ok(());
    }
    let bytes = reader::read(
        body,
        ReadRequest {
            offset: 0,
            length: body.logical_len,
        },
    )?;
    out.extend_from_slice(&bytes);
    Ok(())
}

/// Fastest available byte-exact extraction of many single-byte random offsets
/// (one byte appended per offset, in order).
///
/// With a certified path bloom each read is a direct indexed `hot[pos]` lookup;
/// otherwise each offset is served by [`reader::read`]. Bounds and overflow
/// behaviour match `reader::read`.
pub fn read_random_1b(body: &EkbloomBody, offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    if let Some(hot) = certified_path_hot(body) {
        out.reserve(offsets.len());
        for &offset in offsets {
            // Mirror reader::read's bounds/overflow semantics for a length-1 read.
            let end = offset
                .checked_add(1)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            if end > body.logical_len {
                return Err(oob(offset, 1, body.logical_len));
            }
            let idx = usize::try_from(offset).map_err(|_| oob(offset, 1, body.logical_len))?;
            match hot.get(idx) {
                Some(&byte) => out.push(byte),
                None => return Err(oob(offset, 1, body.logical_len)),
            }
        }
        return Ok(());
    }
    for &offset in offsets {
        let bytes = reader::read(body, ReadRequest { offset, length: 1 })?;
        out.extend_from_slice(&bytes);
    }
    Ok(())
}

/// Fastest available byte-exact extraction of a list of `(offset, length)`
/// ranges (each range appended in order).
///
/// With a certified path bloom each range is a single `extend_from_slice` of the
/// hot symbol array; otherwise each range is served by [`reader::read`]. Bounds
/// and overflow behaviour match `reader::read`.
pub fn read_ranges(body: &EkbloomBody, ranges: &[(u64, u64)], out: &mut Vec<u8>) -> Result<()> {
    if let Some(hot) = certified_path_hot(body) {
        // Validate every range *before* reserving anything, so an adversarial
        // `length` (e.g. `u64::MAX`) cannot drive `Vec::reserve` into a capacity
        // overflow / abort before it is rejected. Only the true output size of
        // the validated ranges is used as the allocation hint.
        let mut total: u64 = 0;
        for &(offset, length) in ranges {
            let end = offset
                .checked_add(length)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            if end > body.logical_len {
                return Err(oob(offset, length, body.logical_len));
            }
            total = total.saturating_add(length);
        }
        if let Ok(extra) = usize::try_from(total) {
            out.reserve(extra);
        }
        for &(offset, length) in ranges {
            let end = offset
                .checked_add(length)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            // end <= logical_len <= hot.len(), and hot.len() fits in usize, so the
            // conversions below cannot fail on any platform the slice fits on.
            let start =
                usize::try_from(offset).map_err(|_| oob(offset, length, body.logical_len))?;
            let stop = usize::try_from(end).map_err(|_| oob(offset, length, body.logical_len))?;
            if length == 1 {
                // Unrolled common case: a single byte is a direct index.
                match hot.get(start) {
                    Some(&byte) => out.push(byte),
                    None => return Err(oob(offset, length, body.logical_len)),
                }
            } else {
                out.extend_from_slice(&hot[start..stop]);
            }
        }
        return Ok(());
    }
    for &(offset, length) in ranges {
        let bytes = reader::read(body, ReadRequest { offset, length })?;
        out.extend_from_slice(&bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::create;
    use crate::sweep::sweep_profiles;
    use crate::ManifestationProfile;

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
    fn cpu_features_is_callable() {
        let f = cpu_features();
        // The values are machine-dependent; the call must simply be safe and total.
        let _ = (f.sse2, f.avx2, f.avx512f);
    }

    #[test]
    fn empty_object_reads_are_empty() {
        let body = create(&[], ManifestationProfile::Root).unwrap();
        let mut out = Vec::new();
        read_sequential(&body, &mut out).unwrap();
        assert!(out.is_empty());
        read_random_1b(&body, &[], &mut out).unwrap();
        read_ranges(&body, &[(0, 0)], &mut out).unwrap();
        assert!(out.is_empty());
        assert!(read_random_1b(&body, &[0], &mut out).is_err());
    }

    #[test]
    fn fast_paths_match_reader_for_every_profile() {
        let src = lcg(0xF00D_1234, 3000);
        let n = src.len() as u64;
        let ranges: Vec<(u64, u64)> = vec![
            (0, 0),
            (0, n),
            (1, 17),
            (500, 700),
            (n - 1, 1),
            (n, 0),
            (n - 64, 64),
        ];
        let offsets: Vec<u64> = (0..64u64).map(|i| (i * 47 + 11) % n).collect();

        for profile in sweep_profiles() {
            let body = create(&src, profile).unwrap();
            let label = profile.label();

            // Sequential agrees with the source and with reader::read.
            let mut seq = Vec::new();
            read_sequential(&body, &mut seq).unwrap();
            assert_eq!(seq, src, "sequential mismatch for {label}");
            let reference = reader::read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: n,
                },
            )
            .unwrap();
            assert_eq!(seq, reference, "sequential vs reader for {label}");

            // Random single bytes agree with the source and with reader::read.
            let mut rand = Vec::new();
            read_random_1b(&body, &offsets, &mut rand).unwrap();
            let expect: Vec<u8> = offsets.iter().map(|&o| src[o as usize]).collect();
            assert_eq!(rand, expect, "random-1b mismatch for {label}");
            for &o in &offsets {
                let direct = reader::read(
                    &body,
                    ReadRequest {
                        offset: o,
                        length: 1,
                    },
                )
                .unwrap();
                assert_eq!(direct, vec![src[o as usize]], "reader-1b for {label}@{o}");
            }

            // Ranges agree with reader::read concatenated per range.
            let mut got = Vec::new();
            read_ranges(&body, &ranges, &mut got).unwrap();
            let mut want = Vec::new();
            for &(o, l) in &ranges {
                want.extend_from_slice(
                    &reader::read(
                        &body,
                        ReadRequest {
                            offset: o,
                            length: l,
                        },
                    )
                    .unwrap(),
                );
            }
            assert_eq!(got, want, "ranges mismatch for {label}");
        }
    }

    #[test]
    fn bounds_errors_match_reader() {
        let src = lcg(3, 256);
        let n = src.len() as u64;
        for profile in sweep_profiles() {
            let body = create(&src, profile).unwrap();
            let label = profile.label();

            // Past-the-end single byte.
            let mut out = Vec::new();
            assert!(read_random_1b(&body, &[n], &mut out).is_err(), "{label}");
            // Offset overflow.
            assert!(
                read_random_1b(&body, &[u64::MAX], &mut out).is_err(),
                "{label}"
            );
            // Range past the end (even a zero-length one).
            assert!(
                read_ranges(&body, &[(n + 1, 0)], &mut out).is_err(),
                "{label}"
            );
            // reader::read rejects the same requests.
            assert!(reader::read(
                &body,
                ReadRequest {
                    offset: n,
                    length: 1
                }
            )
            .is_err());
            assert!(reader::read(
                &body,
                ReadRequest {
                    offset: u64::MAX,
                    length: 1
                }
            )
            .is_err());
            assert!(reader::read(
                &body,
                ReadRequest {
                    offset: n + 1,
                    length: 0
                }
            )
            .is_err());
        }
    }
}
