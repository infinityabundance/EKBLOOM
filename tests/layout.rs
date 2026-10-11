//! Layout and parallelism experiments (directive §9 entry variants and §23
//! performance engineering).
//!
//! Every experiment that `docs/LAYOUT.md` measures has a correctness test here
//! proving the new path is byte-for-byte equal to the authoritative/sequential
//! reference:
//!
//! * [`Bloom4Resume`] pack/unpack (16-byte entry variant),
//! * [`AlignedBuffer`] 64-byte alignment (`aligned == unaligned` content),
//! * `gather_bytes_prefetch` (`prefetch == none`),
//! * `gather_bytes_avx512` (`AVX-512 gather == scalar`, adversarial offsets),
//! * [`read_ranges_parallel`] (`parallel == sequential`, in order).

use ekbloom::simd::{
    gather_bytes, gather_bytes_avx512, gather_bytes_prefetch, gather_bytes_scalar,
};
use ekbloom::{
    copy_hot_sequential, create, gather_hot_1b, read_ranges, read_ranges_parallel, sweep_profiles,
    AlignedBuffer, Bloom4Resume, ManifestationProfile, ReadRequest, CACHE_LINE,
};

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

// --- 1. 16-byte entry variant (§9) ----------------------------------------------

#[test]
fn bloom4_resume_pack_unpack_roundtrips() {
    let cases = [
        Bloom4Resume::new([0, 0, 0, 0], 0, 0),
        Bloom4Resume::new([1, 2, 3, 4], 0x0102_0304, 0x0506_0708_090A_0B0C),
        Bloom4Resume::new([0xFF; 4], u32::MAX, u64::MAX),
    ];
    for entry in cases {
        let packed = entry.pack();
        assert_eq!(packed.len(), Bloom4Resume::PACKED_LEN);
        assert_eq!(Bloom4Resume::unpack(&packed).unwrap(), entry);
    }
}

#[test]
fn bloom4_resume_unpack_bounds_checks() {
    let full = Bloom4Resume::new([9, 8, 7, 6], 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0).pack();
    // Every short prefix is rejected, never read partially.
    for n in 0..Bloom4Resume::PACKED_LEN {
        assert!(Bloom4Resume::unpack(&full[..n]).is_err(), "len {n}");
    }
    // Trailing bytes are ignored (inference style of `unpack_block`).
    let mut long = full.to_vec();
    long.extend_from_slice(&[0xAA; 5]);
    assert_eq!(
        Bloom4Resume::unpack(&long).unwrap(),
        Bloom4Resume::new([9, 8, 7, 6], 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0)
    );
}

#[test]
fn entry_size_tradeoff_is_the_documented_one() {
    const ENTRIES: usize = 65536;
    let eight = ENTRIES * 8; // default D + 4 entry at D = 4
    let sixteen = ENTRIES * Bloom4Resume::PACKED_LEN;
    assert_eq!(eight, 512 * 1024, "8-byte entry must be 512 KiB");
    assert_eq!(sixteen, 1024 * 1024, "16-byte entry must be 1 MiB");
    assert_eq!(sixteen, eight * 2);
}

// --- 2. cache-line alignment (§23) ----------------------------------------------

#[test]
fn aligned_buffer_is_aligned_and_preserves_bytes() {
    let src = lcg(0xA11C_0001, 4096);
    let aligned = AlignedBuffer::from_slice(&src);
    assert!(aligned.is_aligned(), "payload must be 64-byte aligned");
    assert_eq!(aligned.as_slice(), &src[..], "content must be preserved");

    // Empty and single-byte edge cases.
    let empty = AlignedBuffer::from_slice(&[]);
    assert!(empty.as_slice().is_empty());
    let one = AlignedBuffer::from_slice(&[42]);
    assert!(one.is_aligned());
    assert_eq!(one.as_slice(), &[42]);
}

#[test]
fn aligned_and_misaligned_reads_are_identical() {
    let src = lcg(0xA11C_0002, 8192);
    let n = src.len() as u64;

    // A deliberately misaligned copy of the *same logical bytes*: the payload is
    // placed one byte after a 64-byte boundary, so it is provably not 64-aligned.
    let mut raw = vec![0u8; src.len() + CACHE_LINE + 1];
    let base = raw.as_ptr() as usize;
    let aligned_start = (CACHE_LINE - (base % CACHE_LINE)) % CACHE_LINE;
    let start = aligned_start + 1;
    raw[start..start + src.len()].copy_from_slice(&src);
    let misaligned = &raw[start..start + src.len()];
    assert_ne!(
        misaligned.as_ptr() as usize % CACHE_LINE,
        0,
        "control slice must be misaligned"
    );

    let aligned = AlignedBuffer::from_slice(&src);
    assert_eq!(aligned.as_slice(), misaligned);

    // Sequential copies agree byte-for-byte.
    let mut a = Vec::new();
    copy_hot_sequential(aligned.as_slice(), &mut a);
    let mut b = Vec::new();
    copy_hot_sequential(misaligned, &mut b);
    assert_eq!(a, b);
    assert_eq!(a, src);

    // Random single-byte reads agree byte-for-byte.
    let offs = offsets(0xA11C_0002, 5000, n);
    let mut ga = Vec::new();
    gather_hot_1b(aligned.as_slice(), &offs, &mut ga).unwrap();
    let mut gb = Vec::new();
    gather_hot_1b(misaligned, &offs, &mut gb).unwrap();
    assert_eq!(ga, gb);
}

#[test]
fn hot_primitives_match_authoritative_reader() {
    let src = lcg(0xA11C_0003, 5000);
    let n = src.len() as u64;
    let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();

    // `copy_hot_sequential` over the source equals the authoritative whole read.
    let mut seq = Vec::new();
    copy_hot_sequential(&src, &mut seq);
    assert_eq!(
        seq,
        ekbloom::read(
            &body,
            ReadRequest {
                offset: 0,
                length: n
            }
        )
        .unwrap()
    );

    // `gather_hot_1b` over the source equals the authoritative indexed reads.
    let offs = offsets(0xA11C_0003, 1000, n);
    let mut got = Vec::new();
    gather_hot_1b(&src, &offs, &mut got).unwrap();
    let want: Vec<u8> = offs
        .iter()
        .map(|&o| {
            ekbloom::read(
                &body,
                ReadRequest {
                    offset: o,
                    length: 1,
                },
            )
            .unwrap()[0]
        })
        .collect();
    assert_eq!(got, want);
}

// --- 3. software prefetch (§23) -------------------------------------------------

/// The prefetch path must equal the scalar reference for every shape, including
/// the adversarial ones the SIMD reviewers probe.
#[test]
fn prefetch_matches_scalar_adversarial() {
    let hot = lcg(0x9FE7_C401, 1 << 16);
    let n = hot.len() as u64;
    let random = offsets(0x9FE7_C401, 40_000, n);
    let cases: [&[u64]; 7] = [
        &[],
        &[0],
        &[n - 1],
        &[0, 1, 2, 3, 4, 5, 6, 7],
        &[n - 1; 8],
        &[n - 4, n - 3, n - 2, n - 1, n - 4],
        &random,
    ];
    for offs in cases {
        let mut want = Vec::new();
        gather_bytes_scalar(&hot, offs, &mut want).unwrap();
        let mut got = Vec::new();
        gather_bytes_prefetch(&hot, offs, &mut got).unwrap();
        assert_eq!(got, want, "prefetch differs for {} offsets", offs.len());
    }
}

#[test]
fn prefetch_and_scalar_error_identically() {
    let hot = lcg(0x9FE7_C402, 256);
    let n = hot.len() as u64;
    for bad in [n, n + 1, u64::MAX] {
        let offs = vec![5u64, bad, 7];
        let mut a = Vec::new();
        let e1 = gather_bytes_scalar(&hot, &offs, &mut a).unwrap_err();
        let mut b = Vec::new();
        let e2 = gather_bytes_prefetch(&hot, &offs, &mut b).unwrap_err();
        assert!(a.is_empty() && b.is_empty(), "no partial output");
        assert_eq!(e1.to_string(), e2.to_string());
    }
}

// --- 4. AVX-512 gather (§23) ----------------------------------------------------

#[test]
fn avx512_matches_scalar_adversarial() {
    // A large array so the 16-lane vector path is actually taken when AVX-512F is
    // present; on machines without it, `gather_bytes_avx512` falls back and the
    // equality still holds trivially.
    let hot = lcg(0xA512_0001, 1 << 20);
    let n = hot.len() as u64;
    let random = offsets(0xA512_0001, 60_000, n);
    let cases: [&[u64]; 8] = [
        &[],
        &[0],
        &[n - 1],
        &[0, 1, 2, 3, 4, 5, 6, 7],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        &[n - 4, n - 3, n - 2, n - 1, n - 4],
        &[n - 1; 16],
        &random,
    ];
    for offs in cases {
        let mut want = Vec::new();
        gather_bytes_scalar(&hot, offs, &mut want).unwrap();
        let mut got = Vec::new();
        gather_bytes_avx512(&hot, offs, &mut got).unwrap();
        assert_eq!(got, want, "avx512 differs for {} offsets", offs.len());
    }
}

#[test]
fn avx512_out_of_range_errors_identically() {
    let hot = lcg(0xA512_0002, 4096);
    let n = hot.len() as u64;
    for bad in [n, n + 1, n + 1000, u64::MAX] {
        let offs = vec![1u64, bad, 3];
        let mut a = Vec::new();
        let e1 = gather_bytes_scalar(&hot, &offs, &mut a).unwrap_err();
        let mut b = Vec::new();
        let e2 = gather_bytes_avx512(&hot, &offs, &mut b).unwrap_err();
        assert!(a.is_empty() && b.is_empty(), "no partial output");
        assert_eq!(e1.to_string(), e2.to_string(), "error mismatch at {bad}");
    }
    // The 32-bit truncation probe: (1<<32)+5 must not be read as index 5.
    let truncating = (1u64 << 32) + 5;
    let offs = vec![truncating; 16];
    let mut b = Vec::new();
    assert!(gather_bytes_avx512(&hot, &offs, &mut b).is_err());
    assert!(b.is_empty());

    // Empty `hot` is total.
    let empty: Vec<u8> = Vec::new();
    let mut b = Vec::new();
    gather_bytes_avx512(&empty, &[], &mut b).unwrap();
    assert!(b.is_empty());
}

#[test]
fn avx2_avx512_and_scalar_all_agree() {
    // A belt-and-braces three-way comparison over a short adversarial list.
    let hot = lcg(0xA512_0003, 2048);
    let n = hot.len() as u64;
    let offs: Vec<u64> = (0..64u64)
        .map(|k| {
            if k.is_multiple_of(9) {
                n - 1
            } else {
                (k * 53) % n
            }
        })
        .collect();
    let mut s = Vec::new();
    gather_bytes_scalar(&hot, &offs, &mut s).unwrap();
    let mut a2 = Vec::new();
    gather_bytes(&hot, &offs, &mut a2).unwrap();
    let mut a5 = Vec::new();
    gather_bytes_avx512(&hot, &offs, &mut a5).unwrap();
    assert_eq!(s, a2);
    assert_eq!(s, a5);
}

// --- 5. parallel regional reads (§23) -------------------------------------------

fn ranges_4k(seed: u64, count: usize, n: u64) -> Vec<(u64, u64)> {
    const WINDOW: u64 = 4096;
    let span = n - WINDOW;
    let mut s = seed;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 20) % (span + 1), WINDOW)
        })
        .collect()
}

#[test]
fn parallel_ranges_match_sequential_in_order() {
    let src = lcg(0x9A11_0001, 200_000);
    let n = src.len() as u64;
    let ranges = ranges_4k(0x9A11_0001, 64, n);

    for profile in sweep_profiles() {
        let body = create(&src, profile).unwrap();
        let label = profile.label();

        let mut seq = Vec::new();
        read_ranges(&body, &ranges, &mut seq).unwrap();

        for threads in [1usize, 2, 3, 4, 8, 16, 64, 1000] {
            let par = read_ranges_parallel(&body, &ranges, threads).unwrap();
            assert_eq!(
                par,
                seq,
                "parallel({threads}) != sequential for {label}, {} ranges",
                ranges.len()
            );
        }
    }
}

#[test]
fn parallel_ranges_handle_empty_and_degenerate_inputs() {
    let src = lcg(0x9A11_0002, 8192);
    let n = src.len() as u64;
    let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();

    // No ranges: empty output on every thread count.
    for threads in [0usize, 1, 4, 1000] {
        assert!(read_ranges_parallel(&body, &[], threads)
            .unwrap()
            .is_empty());
    }

    // Zero-length ranges and a full-object range agree with the sequential path.
    let ranges: Vec<(u64, u64)> = vec![(0, 0), (0, n), (n, 0), (n - 1, 1), (5, 0)];
    let mut seq = Vec::new();
    read_ranges(&body, &ranges, &mut seq).unwrap();
    for threads in [1usize, 2, 4, 8] {
        assert_eq!(
            read_ranges_parallel(&body, &ranges, threads).unwrap(),
            seq,
            "threads {threads}"
        );
    }
}

#[test]
fn parallel_ranges_errors_match_sequential() {
    let src = lcg(0x9A11_0003, 4096);
    let n = src.len() as u64;
    for profile in sweep_profiles() {
        let body = create(&src, profile).unwrap();
        let label = profile.label();

        let bad: Vec<(u64, u64)> = vec![(10, 8), (n + 1, 0)];
        let mut seq = Vec::new();
        assert!(read_ranges(&body, &bad, &mut seq).is_err(), "{label}");
        for threads in [1usize, 2, 4, 8] {
            assert!(
                read_ranges_parallel(&body, &bad, threads).is_err(),
                "parallel({threads}) accepted a bad range for {label}"
            );
        }

        // An overflowing length on a later range must still be rejected.
        let bad2: Vec<(u64, u64)> = vec![(0, 4), (u64::MAX, 2)];
        assert!(read_ranges_parallel(&body, &bad2, 4).is_err(), "{label}");
    }
}

#[test]
fn parallel_ranges_single_thread_is_identity() {
    // `read_ranges_parallel(.., 1)` must equal the sequential `read_ranges`.
    let src = lcg(0x9A11_0004, 50_000);
    let n = src.len() as u64;
    let body = create(&src, ManifestationProfile::Path { depth: 8 }).unwrap();
    let ranges = ranges_4k(0x9A11_0004, 32, n);
    let mut seq = Vec::new();
    read_ranges(&body, &ranges, &mut seq).unwrap();
    assert_eq!(read_ranges_parallel(&body, &ranges, 1).unwrap(), seq);
    assert_eq!(read_ranges_parallel(&body, &ranges, 0).unwrap(), seq);
}

#[test]
fn parallel_hot_reads_equal_sequential_reads() {
    // For a certified path bloom the parallel read is one copy per range, so it
    // must equal a direct concatenation of the source slices.
    let src = lcg(0x9A11_0005, 100_000);
    let n = src.len() as u64;
    let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    let ranges = ranges_4k(0x9A11_0005, 48, n);
    let par = read_ranges_parallel(&body, &ranges, 4).unwrap();

    let mut want = Vec::new();
    for &(o, l) in &ranges {
        want.extend_from_slice(&src[o as usize..(o + l) as usize]);
    }
    assert_eq!(par, want);
}
