//! End-to-end tests for manifestation, in-memory reads, and verification.
//!
//! These exercise the public API only: `create` / `grow` / `manifest`, `read`,
//! `verify` / `verify_deep`, and `inspect`, across every profile and a wide range
//! of inputs (empty, single byte, every byte value, repeating, alternating,
//! deterministic pseudorandom, and lengths crossing block/checkpoint boundaries).

use ekbloom::{
    create, grow, inspect, read, verify, verify_deep, EkbloomBody, ManifestationProfile,
    ReadRequest,
};

/// Deterministic pseudorandom bytes from a small LCG (no new dependencies).
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

/// Inputs spanning the documented categories and boundary lengths.
fn cases() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        vec![0x7A],
        (0u8..=255).collect(),
        vec![0x5Au8; 300],
        (0..301u32)
            .map(|i| if i.is_multiple_of(2) { 0x11 } else { 0xEE })
            .collect(),
        lcg(0x1234_5678_9ABC_DEF0, 777),
        lcg(0x0FED_CBA9_8765_4321, 1023),
        lcg(0x1111_2222_3333_4444, 1024),
        lcg(0x5555_6666_7777_8888, 1025),
        lcg(0x9999_AAAA_BBBB_CCCC, 63),
        lcg(0xDDDD_EEEE_FFFF_0000, 64),
        lcg(0x0123_4567_89AB_CDEF, 65),
        lcg(0xCAFE_BABE_DEAD_BEEF, 129),
    ]
}

/// Every profile required by the acceptance tests.
fn profiles() -> Vec<ManifestationProfile> {
    vec![
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Dense { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ]
}

/// A batch of valid `ReadRequest`s, including 0-length reads and the whole object.
fn requests(n: u64) -> Vec<ReadRequest> {
    let mut reqs = vec![
        ReadRequest {
            offset: 0,
            length: n,
        },
        ReadRequest {
            offset: 0,
            length: 0,
        },
        ReadRequest {
            offset: n,
            length: 0,
        },
        ReadRequest {
            offset: n.saturating_sub(1),
            length: if n == 0 { 0 } else { 1 },
        },
        ReadRequest {
            offset: n / 2,
            length: n - n / 2,
        },
    ];
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..64 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let offset = if n == 0 { 0 } else { s % n };
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let remaining = n - offset;
        let length = if remaining == 0 {
            0
        } else {
            s % (remaining + 1)
        };
        reqs.push(ReadRequest { offset, length });
    }
    reqs
}

#[test]
fn whole_object_roundtrip_for_every_profile() {
    for src in cases() {
        for profile in profiles() {
            let body = create(&src, profile).unwrap();
            assert_eq!(body.logical_len, src.len() as u64);
            assert_eq!(body.visible_bytes(), src.len() as u64);
            assert_eq!(body.machine_id, 1);
            assert!(!body.object_id.iter().all(|&b| b == 0));

            let got = read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64,
                },
            )
            .unwrap();
            assert_eq!(
                got,
                src,
                "roundtrip mismatch: len={} profile={:?}",
                src.len(),
                profile
            );
        }
    }
}

#[test]
fn random_ranges_match_the_source_slice() {
    for src in cases() {
        let n = src.len() as u64;
        // A representative mix of access paths: checkpoints, history, and path.
        for profile in [
            ManifestationProfile::Dense { interval: 64 },
            ManifestationProfile::Sparse { interval: 1024 },
            ManifestationProfile::Path { depth: 4 },
            ManifestationProfile::MaxSpeed,
        ] {
            let body = create(&src, profile).unwrap();
            for req in requests(n) {
                let got = read(&body, req).unwrap();
                let want = src[req.offset as usize..(req.offset + req.length) as usize].to_vec();
                assert_eq!(
                    got,
                    want,
                    "range ({},{}) mismatch: len={} profile={:?}",
                    req.offset,
                    req.length,
                    src.len(),
                    profile
                );
            }
        }
    }
}

#[test]
fn invalid_ranges_are_rejected() {
    let body = create(b"abcdef", ManifestationProfile::Root).unwrap();
    assert!(read(
        &body,
        ReadRequest {
            offset: 0,
            length: 7
        }
    )
    .is_err());
    assert!(read(
        &body,
        ReadRequest {
            offset: 7,
            length: 0
        }
    )
    .is_err());
    assert!(read(
        &body,
        ReadRequest {
            offset: u64::MAX,
            length: 1
        }
    )
    .is_err());
    // A zero-length read at the end is valid.
    assert_eq!(
        read(
            &body,
            ReadRequest {
                offset: 6,
                length: 0
            }
        )
        .unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn valid_bodies_verify() {
    for src in [lcg(0x2222_3333, 200), lcg(0x4444_5555, 1025)] {
        for profile in profiles() {
            let body = create(&src, profile).unwrap();
            let report = verify(&body).unwrap();
            assert!(report.ok, "verify failed for {profile:?}: {report:?}");
            assert_eq!(report.logical_len, src.len() as u64);
            let deep = verify_deep(&body).unwrap();
            assert!(deep.ok, "deep verify failed for {profile:?}: {deep:?}");
        }
    }
}

#[test]
fn additive_growth_remains_verifiable() {
    let src = lcg(0x6666_7777, 500);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    grow(&mut body, ManifestationProfile::Full).unwrap();
    grow(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();
    grow(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 256,
        },
    )
    .unwrap();
    assert_eq!(body.generation, 5);
    assert_eq!(body.profiles.len(), 5);
    assert!(verify_deep(&body).unwrap().ok);
    assert_eq!(
        read(
            &body,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );
}

#[test]
fn tampered_history_is_detected() {
    let src = lcg(0x8888_9999, 300);
    let mut body = create(&src, ManifestationProfile::Dense { interval: 64 }).unwrap();
    body.history.as_mut().unwrap().states[3] ^= 0xFF;
    let report = verify(&body).unwrap();
    assert!(!report.ok);
    let failing: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect();
    assert!(failing.contains(&"history"), "failing checks: {failing:?}");
}

#[test]
fn tampered_bloom_payload_is_detected() {
    let src = lcg(0xAAAA_BBBB, 300);
    let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    let last = body.blooms[0].payload.len() - 1;
    body.blooms[0].payload[last] ^= 0xFF;
    let report = verify(&body).unwrap();
    assert!(!report.ok);
    assert!(report
        .checks
        .iter()
        .any(|c| !c.passed && c.name.starts_with("bloom[0]:path")));
}

#[test]
fn path_physical_accounting_dominant_terms() {
    let src = lcg(0xCCCC_DDDD, 1000);
    let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    let n = src.len() as u64;
    let core = body.core.len() as u64;

    // Dominant terms: core + N (path hot) + 4 * ceil(N / 4) (block successors).
    let dominant = core + n + 4 * n.div_ceil(4);
    let cert_bytes: u64 = body
        .certificates
        .iter()
        .map(|c| c.to_bytes().len() as u64)
        .sum();
    assert!(body.physical_bytes() >= dominant);
    assert!(
        body.physical_bytes() <= dominant + cert_bytes + 64,
        "physical {} exceeds dominant {} + certs {} + headers",
        body.physical_bytes(),
        dominant,
        cert_bytes
    );

    assert_eq!(body.visible_bytes(), n);
    assert_eq!(
        body.concealed_bytes(),
        body.physical_bytes() - body.visible_bytes()
    );
    assert!(body.expansion_ratio().is_finite());
    assert!(body.expansion_ratio() > 1.0);

    let report = inspect(&body);
    let sum: u64 = report.components.iter().map(|c| c.bytes).sum();
    assert_eq!(sum, report.physical_bytes);

    // `inspect` names the core, the bloom and its certificate.
    assert!(report.components.iter().any(|c| c.kind == "core"));
    assert!(report.components.iter().any(|c| c.kind == "bloom"));
    assert!(report.components.iter().any(|c| c.kind == "certificate"));
}

#[test]
fn manifests_match_across_bodies() {
    // Independent bodies from identical sources share identity and core bytes.
    let src = lcg(0xEEEE_FFFF, 128);
    let a: EkbloomBody = create(&src, ManifestationProfile::Full).unwrap();
    let b: EkbloomBody = create(&src, ManifestationProfile::Path { depth: 8 }).unwrap();
    assert_eq!(a.object_id, b.object_id);
    assert_eq!(a.core, b.core);
    assert_eq!(a.core_hash, b.core_hash);
}
