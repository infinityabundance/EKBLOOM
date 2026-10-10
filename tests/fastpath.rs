//! Integration tests: the optimised fast paths are byte-exact against the
//! authoritative reader for every profile on the manifestation frontier.
//!
//! These exercise the public API only: for each profile in
//! [`ekbloom::sweep_profiles`] we assert that
//! [`ekbloom::read_sequential`], [`ekbloom::read_random_1b`] and
//! [`ekbloom::read_ranges`] return exactly what [`ekbloom::read`] returns (and
//! what the original source contains).

use ekbloom::{
    create, read, read_random_1b, read_ranges, read_sequential, sweep_profiles, ReadRequest,
};

/// Deterministic corpus.
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
fn fast_paths_are_byte_exact_for_every_profile() {
    let src = lcg(0x5EED_0BAD_C0FF_EE42, 100_000);
    let n = src.len() as u64;

    let ranges: Vec<(u64, u64)> = vec![
        (0, 0),
        (0, n),
        (0, 1),
        (1, 4095),
        (1234, 65536),
        (n / 2, 777),
        (n - 4096, 4096),
        (n - 1, 1),
        (n, 0),
    ];
    let offsets: Vec<u64> = {
        let mut s = 0xDEAD_BEEF_1234_5678u64;
        (0..512)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s % n
            })
            .collect()
    };

    let profiles = sweep_profiles();
    assert!(
        profiles.len() >= 2,
        "frontier should span multiple profiles"
    );

    for profile in profiles {
        let body = create(&src, profile).expect("build body");
        let label = profile.label();

        // Sequential: equals the source and equals reader::read.
        let mut seq = Vec::new();
        read_sequential(&body, &mut seq).expect("sequential");
        assert_eq!(seq, src, "sequential source mismatch for {label}");
        assert_eq!(
            seq,
            read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: n
                }
            )
            .unwrap(),
            "sequential vs reader for {label}"
        );

        // Random single bytes: equals the source and equals reader::read.
        let mut got = Vec::new();
        read_random_1b(&body, &offsets, &mut got).expect("random-1b");
        let want: Vec<u8> = offsets.iter().map(|&o| src[o as usize]).collect();
        assert_eq!(got, want, "random-1b source mismatch for {label}");
        for &o in &offsets {
            assert_eq!(
                read(
                    &body,
                    ReadRequest {
                        offset: o,
                        length: 1
                    }
                )
                .unwrap(),
                vec![src[o as usize]],
                "reader-1b at {o} for {label}"
            );
        }

        // Ranges: equals the concatenation of reader::read per range.
        let mut got = Vec::new();
        read_ranges(&body, &ranges, &mut got).expect("ranges");
        let mut want = Vec::new();
        for &(o, l) in &ranges {
            want.extend_from_slice(
                &read(
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
fn fast_path_errors_match_reader() {
    let src = lcg(11, 4096);
    let n = src.len() as u64;
    for profile in sweep_profiles() {
        let body = create(&src, profile).unwrap();
        let label = profile.label();

        let mut out = Vec::new();
        assert!(read_random_1b(&body, &[n], &mut out).is_err(), "{label}");
        assert!(
            read_random_1b(&body, &[u64::MAX], &mut out).is_err(),
            "{label}"
        );
        assert!(
            read_ranges(&body, &[(n + 1, 0)], &mut out).is_err(),
            "{label}"
        );
        assert!(
            read_ranges(&body, &[(u64::MAX, 2)], &mut out).is_err(),
            "{label}"
        );

        // The authoritative reader rejects the same requests.
        assert!(read(
            &body,
            ReadRequest {
                offset: n,
                length: 1
            }
        )
        .is_err());
        assert!(read(
            &body,
            ReadRequest {
                offset: u64::MAX,
                length: 2
            }
        )
        .is_err());
    }
}

#[test]
fn sequential_read_appends_without_clearing() {
    // The documented contract is that fast paths append; verify it, so callers can
    // batch multiple objects into one buffer.
    let src = lcg(21, 2048);
    let body = create(&src, ekbloom::ManifestationProfile::Path { depth: 4 }).unwrap();
    let mut out = b"prefix:".to_vec();
    read_sequential(&body, &mut out).unwrap();
    assert!(out.starts_with(b"prefix:"));
    assert_eq!(&out[7..], &src[..]);
}
