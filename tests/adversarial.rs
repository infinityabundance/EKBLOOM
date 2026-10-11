//! Adversarial review harness.
//!
//! Independent, hostile tests that try to break correctness of the frozen
//! EKBLOOM contract. Every test here is executable and self-contained; the ones
//! that *document* a residual gap say so in their name and body.
//!
//! Coverage:
//!  1. fast-path integrity against adversarial corpora and boundary lengths,
//!  2. certificate soundness and status gating on the read path,
//!  3. verify/verify_deep teeth per structure type,
//!  4. boundary/overflow totality (no panic, no attacker-sized allocation),
//!  5. non-withdrawal across `grow` + crash fallback,
//!  6. MANIFEST/header mismatch rejection.

use std::path::PathBuf;

use ekbloom::bloom_context::build_context_shell;
use ekbloom::bloom_global::certify_global;
use ekbloom::bloom_region::RegionTable;
use ekbloom::machine::state_log;
use ekbloom::reader::open_bytes;
use ekbloom::{
    append_generation, create, grow, read, read_random_1b, read_ranges, read_sequential,
    scan_generations, verify, verify_deep, write_body, BloomKind, Certification, EkbloomBody,
    EkbloomError, GlobalBloom, ManifestationProfile, OrMachine, PathBloom, ReadRequest, Snapshot,
    NL2,
};

/// Construct a [`ReadRequest`] (keeps call sites short and fmt-stable).
fn req(offset: u64, length: u64) -> ReadRequest {
    ReadRequest { offset, length }
}

// ---------------------------------------------------------------------------
// Deterministic corpora
// ---------------------------------------------------------------------------

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

/// Adversarial byte patterns: all byte values, long runs, alternating,
/// pseudorandom, and periodic with period 1/2/3/4.
fn pattern(kind: usize, len: usize) -> Vec<u8> {
    let per = |i: usize| -> u8 {
        match kind {
            0 => (i % 256) as u8,
            1 => 0x00,
            2 => 0xFF,
            3 => {
                if i.is_multiple_of(2) {
                    0xAA
                } else {
                    0x55
                }
            }
            5 => 0x5A,
            6 => [0x10u8, 0x20][i % 2],
            7 => [0x10u8, 0x20, 0x30][i % 3],
            8 => [0x11u8, 0x22, 0x33, 0x44][i % 4],
            _ => 0,
        }
    };
    if kind == 4 || kind == 9 {
        lcg(0x1234_5678_9ABC_DEF0 ^ kind as u64, len)
    } else {
        (0..len).map(per).collect()
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-adversarial-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Attack 1: fast-path integrity vs the authoritative machine
// ---------------------------------------------------------------------------

/// Assert every path (`read`, `read_sequential`, `read_random_1b`,
/// `read_ranges`) equals the authoritative `NL2` decode, and that bloom/checkpoint
/// continuation data equals the fresh state log.
fn assert_all_paths_agree(src: &[u8], profile: ManifestationProfile, label: &str) {
    let body = create(src, profile).expect("create");
    let n = body.logical_len;
    assert_eq!(n, src.len() as u64, "logical_len for {label}");
    let dec = NL2.decode(&body.core, n).expect("decode");
    assert_eq!(&dec[..], src, "authoritative decode != source for {label}");

    let whole = read(&body, req(0, n)).unwrap();
    assert_eq!(whole, dec, "reader::read mismatch for {label}");

    let mut seq = Vec::new();
    read_sequential(&body, &mut seq).unwrap();
    assert_eq!(seq, dec, "read_sequential mismatch for {label}");

    let probes: Vec<u64> = if n == 0 {
        Vec::new()
    } else {
        vec![0, n / 2, n - 1]
    };
    let mut got = Vec::new();
    read_random_1b(&body, &probes, &mut got).unwrap();
    let want: Vec<u8> = probes.iter().map(|&o| dec[o as usize]).collect();
    assert_eq!(got, want, "read_random_1b mismatch for {label}");

    let ranges: Vec<(u64, u64)> = if n == 0 {
        vec![(0, 0)]
    } else {
        vec![(0, n), (0, 1), (n / 2, n - n / 2), (n - 1, 1), (n, 0)]
    };
    let mut ranges_out = Vec::new();
    read_ranges(&body, &ranges, &mut ranges_out).unwrap();
    let mut want_r = Vec::new();
    for &(o, l) in &ranges {
        want_r.extend_from_slice(&read(&body, req(o, l)).unwrap());
    }
    assert_eq!(ranges_out, want_r, "read_ranges mismatch for {label}");

    // Continuation state after a certified path bloom equals state_log[i + D].
    let logs = state_log(&body.core, n).unwrap();
    for shell in &body.blooms {
        if shell.kind != BloomKind::Path || shell.status != Certification::Certified {
            continue;
        }
        let pb = PathBloom::from_shell(shell).expect("parse path bloom");
        assert_eq!(pb.logical_len, n, "path coverage for {label}");
        for pos in 0..n {
            assert_eq!(
                pb.symbol_at(pos).unwrap(),
                dec[pos as usize],
                "path symbol {pos} for {label}"
            );
        }
        let d = u64::from(pb.depth);
        for (j, &cold) in pb.cold.iter().enumerate() {
            let end = d.saturating_mul(j as u64 + 1).min(n);
            assert_eq!(
                cold, logs[end as usize],
                "path continuation state after block {j} for {label}"
            );
        }
    }

    // Checkpoint restarts equal the fresh state log at interval boundaries.
    for cp in &body.checkpoints {
        let k = u64::from(cp.interval);
        let mut positions = vec![0u64, n];
        if k > 0 {
            for j in 0..3u64 {
                let p = j * k;
                if p <= n {
                    positions.push(p);
                }
                if p < n {
                    positions.push(p + 1);
                }
            }
        }
        for p in positions {
            let got = cp.restore(&body.core, p).unwrap();
            assert_eq!(
                got,
                Snapshot::new(logs[p as usize], p),
                "checkpoint restore at {p} for {label}"
            );
        }
    }
}

const LENGTHS: &[usize] = &[
    0, 1, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 255, 256, 257, 511, 1023,
    1024, 1025, 2047, 4096, 4097,
];

#[test]
fn fast_paths_equal_authoritative_machine_on_adversarial_corpus() {
    for &len in LENGTHS {
        for kind in 0..9usize {
            let src = pattern(kind, len);
            assert_all_paths_agree(
                &src,
                ManifestationProfile::MaxSpeed,
                &format!("k={kind} len={len}"),
            );
        }
    }

    // Lengths congruent to 0..D-1 mod D (bloom block boundaries).
    for &d in &[1u16, 2, 4, 8, 16, 32] {
        for r in 0..d as usize {
            let len = 3 * d as usize + r;
            assert_all_paths_agree(
                &pattern(4, len),
                ManifestationProfile::MaxSpeed,
                &format!("D={d} r={r}"),
            );
        }
    }

    // Lengths around every supported checkpoint interval.
    for &k in &[1024usize, 256, 64, 16, 4, 1] {
        for len in [k.saturating_sub(1), k, k + 1] {
            assert_all_paths_agree(
                &pattern(4, len),
                ManifestationProfile::Sparse { interval: k as u32 },
                &format!("K={k} len={len}"),
            );
        }
    }
    for len in [65535usize, 65536, 65537] {
        assert_all_paths_agree(
            &pattern(4, len),
            ManifestationProfile::MaxSpeed,
            &format!("big len={len}"),
        );
        assert_all_paths_agree(
            &pattern(4, len),
            ManifestationProfile::Sparse { interval: 65536 },
            &format!("big K=65536 len={len}"),
        );
    }

    // Non-bloom paths (Root) on a subset, and the exotic bloom profiles.
    for &len in &[0usize, 1, 2, 3, 4, 5, 63, 64, 65, 255, 256, 257, 1024, 1025] {
        for kind in [0usize, 4, 7] {
            assert_all_paths_agree(
                &pattern(kind, len),
                ManifestationProfile::Root,
                &format!("root k={kind} len={len}"),
            );
        }
    }
    for profile in [
        ManifestationProfile::Full,
        ManifestationProfile::Dense { interval: 256 },
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 4096,
        },
    ] {
        let label = profile.label();
        assert_all_paths_agree(&pattern(4, 700), profile, &label);
    }
}

// ---------------------------------------------------------------------------
// Attack 2: certificate soundness / status gating on the read path
// ---------------------------------------------------------------------------

#[test]
fn read_never_uses_uncertified_path_shell() {
    let src = pattern(4, 500);
    let dec = {
        let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
        NL2.decode(&body.core, body.logical_len).unwrap()
    };

    for demoted in [Certification::Unverified, Certification::Rejected] {
        let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
        assert!(body
            .blooms
            .iter()
            .any(|b| b.kind == BloomKind::Path && b.status == Certification::Certified));
        // Corrupt the hot symbols *and* demote: if `read` consulted this shell its
        // output would be wrong, so a correct result proves the shell was ignored.
        body.blooms[0].payload[18 + 9] ^= 0xFF;
        for b in body.blooms.iter_mut() {
            if b.kind == BloomKind::Path {
                b.status = demoted;
            }
        }
        let got = read(&body, req(0, body.logical_len)).unwrap();
        assert_eq!(
            got, dec,
            "an uncertified ({demoted:?}) shell influenced the read"
        );
    }
}

#[test]
fn forged_certified_path_bloom_is_used_by_read_but_caught_by_verify() {
    let dir = temp_dir("forge-path");
    let path = dir.join("p.ekb");
    let src = pattern(4, 300);

    let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    assert_eq!(body.blooms[0].kind, BloomKind::Path);
    // Path payload = 18-byte prefix ++ hot symbols. Flip one hot symbol while
    // leaving `status = Certified` and every structural field intact.
    const PATH_PREFIX_LEN: usize = 18;
    body.blooms[0].payload[PATH_PREFIX_LEN + 7] ^= 0xFF;
    write_body(&path, &body).unwrap();

    let opened = open_bytes(&std::fs::read(&path).unwrap()).expect("open accepts forgery");
    let got = read(&opened, req(7, 1)).unwrap();
    assert_eq!(
        got[0],
        src[7] ^ 0xFF,
        "read used the falsely-Certified path shell (trust boundary)"
    );
    assert!(
        !verify(&opened).unwrap().ok,
        "verify must catch the forged path bloom"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Attack 3: verify/verify_deep teeth
// ---------------------------------------------------------------------------

fn has_failed(report: &ekbloom::VerificationReport, name: &str) -> bool {
    report.checks.iter().any(|c| c.name == name && !c.passed)
}

/// Re-link every certificate to the (possibly mutated) bloom at the same index,
/// so certificate linkage cannot mask a *different* structure's corruption.
fn refresh_certificates(body: &mut EkbloomBody) {
    let hashes: Vec<[u8; 32]> = body.blooms.iter().map(|s| s.content_hash()).collect();
    let core_hash = body.core_hash;
    for (i, cert) in body.certificates.iter_mut().enumerate() {
        if let Some(h) = hashes.get(i) {
            cert.bloom_hash = *h;
        }
        cert.core_hash = core_hash;
        cert.receipt_hash = cert.compute_receipt_hash();
    }
}

#[test]
fn verify_detects_history_tamper() {
    let mut body = create(&pattern(4, 400), ManifestationProfile::Full).unwrap();
    assert!(verify(&body).unwrap().ok);
    body.history.as_mut().unwrap().states[5] ^= 0xFFFF;
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(has_failed(&r, "history"));
}

#[test]
fn verify_detects_checkpoint_tamper() {
    let src = pattern(4, 400);
    let mut body = create(&src, ManifestationProfile::Sparse { interval: 1 }).unwrap();
    assert!(verify(&body).unwrap().ok);
    // resume[0] is Omega_0 = (0, 0); forge the state, keep the cursor legal.
    body.checkpoints[0].resume[0].state ^= 0xFFFF;
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(r.checks.iter().any(|c| c.name.starts_with("checkpoints")));
}

#[test]
fn verify_detects_path_bloom_tamper() {
    let src = pattern(4, 400);
    let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    assert!(verify(&body).unwrap().ok);
    body.blooms[0].payload[18 + 3] ^= 0xFF;
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(has_failed(&r, "bloom[0]:path"));
}

#[test]
fn verify_detects_global_bloom_entry_tamper() {
    let src = pattern(4, 4096);
    let mut body = create(&src, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    assert!(verify(&body).unwrap().ok);

    let mut gb = GlobalBloom::from_shell(&body.blooms[0]).unwrap();
    let k = gb.present.iter().position(|&p| p).expect("a present key");
    gb.table[k].symbols[0] ^= 0xFF;
    let shell = gb.to_shell(
        body.blooms[0].generation,
        body.blooms[0].core_hash,
        body.blooms[0].status,
    );
    body.blooms[0] = shell;
    refresh_certificates(&mut body);

    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(r.checks.iter().any(|c| c.name.starts_with("bloom[0]")));
}

#[test]
fn verify_detects_regional_bloom_tamper() {
    let src = pattern(4, 4096);
    let profile = ManifestationProfile::Regional {
        depth: 4,
        region_bytes: 4096,
    };
    let mut body = create(&src, profile).unwrap();
    assert!(verify(&body).unwrap().ok);
    assert!(RegionTable::from_shell(&body.blooms[0]).is_ok());
    // Region payload: 46-byte prefix, then records `key u16 ++ D symbols ++ u32`;
    // corrupt the first symbol of the first entry.
    body.blooms[0].payload[46 + 2] ^= 0xFF;
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(r.checks.iter().any(|c| c.name.starts_with("bloom[0]")));
}

#[test]
fn verify_detects_contextual_bloom_tamper() {
    let src = pattern(4, 200);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let mut shell = build_context_shell(&body.core, body.logical_len, 4, 1).unwrap();
    // Contextual payload: 14-byte prefix, then `state u16 ++ ctx[D] ++ syms[D]
    // ++ next u32`. Corrupt the first outcome symbol.
    shell.payload[14 + 2 + 4] ^= 0xFF;
    body.blooms.push(shell);
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(has_failed(&r, "bloom[0]:contextual"));
}

#[test]
fn verify_detects_certificate_receipt_tamper() {
    let src = pattern(4, 200);
    let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    assert!(verify(&body).unwrap().ok);
    // Change a field covered by the receipt without recomputing it.
    body.certificates[0].method.push('!');
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(has_failed(&r, "certificates"));
}

#[test]
fn verify_detects_core_tamper_and_deep_is_sound() {
    let src = pattern(4, 300);
    let mut body = create(&src, ManifestationProfile::MaxSpeed).unwrap();
    assert!(verify(&body).unwrap().ok);
    assert!(verify_deep(&body).unwrap().ok);

    body.core[0] ^= 0xFF;
    let r = verify(&body).unwrap();
    assert!(!r.ok);
    assert!(has_failed(&r, "core_hash"));
}

/// Finding #2 regression: `verify` must RE-DERIVE a global bloom's certification
/// status from the object's own positions, never trusting the stored label. A
/// bloom whose true verdict is `Rejected` but which claims `Certified` must fail,
/// with the global-bloom check itself reporting the failure.
#[test]
fn verify_rederives_global_bloom_certification_status() {
    let src = pattern(4, 4096);
    let mut body = create(&src, ManifestationProfile::Bloom { depth: 4 }).unwrap();

    // Genuine verdict over this object's positions is Rejected (the 16-bit state
    // key is insufficient), so an honest shell can never be Certified here.
    let honest = certify_global(&body.core, body.logical_len, 4).unwrap();
    assert_eq!(honest.status, Certification::Rejected);

    // Baseline: the profile's actual `Unverified` shell verifies cleanly.
    assert!(verify(&body).unwrap().ok);

    // Relabel the shell and its payload copy as Certified, and refresh the linked
    // certificate so no other check can mask the global bloom.
    body.blooms[0].status = Certification::Certified;
    body.blooms[0].payload[18..22]
        .copy_from_slice(&Certification::Certified.to_u32().to_le_bytes());
    body.certificates[0].status = Certification::Certified;
    refresh_certificates(&mut body);

    let r = verify(&body).unwrap();
    assert!(!r.ok, "verify accepted a falsely-Certified global bloom");
    assert!(
        r.checks
            .iter()
            .any(|c| c.name.starts_with("bloom[0]") && !c.passed),
        "the failing check must be the global bloom: {:?}",
        r.checks
    );
}

/// The positive half of finding #2: when the re-derived verdict really is
/// `Certified`, a `Certified` claim passes.
#[test]
fn verify_accepts_genuinely_certified_global_bloom() {
    let src = pattern(1, 700); // constant fill: state key is sufficient
    let mut body = create(&src, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    let honest = certify_global(&body.core, body.logical_len, 4).unwrap();
    assert_eq!(honest.status, Certification::Certified);

    body.blooms[0].status = Certification::Certified;
    body.blooms[0].payload[18..22]
        .copy_from_slice(&Certification::Certified.to_u32().to_le_bytes());
    body.certificates[0].status = Certification::Certified;
    refresh_certificates(&mut body);

    let r = verify(&body).unwrap();
    assert!(
        r.ok,
        "genuinely certified global bloom must verify: {:?}",
        r.checks
    );
}

// ---------------------------------------------------------------------------
// Attack 4: boundary / overflow totality
// ---------------------------------------------------------------------------

#[test]
fn huge_lengths_are_clean_errors_not_aborts() {
    let src = pattern(4, 300);
    let n = src.len() as u64;
    let profiles = [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ];
    for profile in profiles {
        let label = profile.label();
        let body = create(&src, profile).unwrap();

        // Zero-length read at the very end is legal; one past the end is not.
        assert!(read(&body, req(n, 0)).is_ok(), "{label}");
        assert!(read(&body, req(n + 1, 0)).is_err(), "{label}");
        assert!(read(&body, req(n, 1)).is_err(), "{label}");
        // u64 offset+length overflow.
        assert!(
            matches!(
                read(&body, req(u64::MAX, 2)),
                Err(EkbloomError::Overflow { .. })
            ),
            "{label}"
        );

        // `read_ranges` with an adversarial per-range length must validate before
        // it reserves; a raw `u64::MAX` length previously aborted here.
        let mut out = Vec::new();
        assert!(
            read_ranges(&body, &[(0, u64::MAX)], &mut out).is_err(),
            "{label}"
        );
        assert!(
            read_ranges(&body, &[(n, u64::MAX)], &mut out).is_err(),
            "{label}"
        );
        assert!(
            read_ranges(&body, &[(u64::MAX, 2)], &mut out).is_err(),
            "{label}"
        );
        assert!(
            read_ranges(&body, &[(n + 1, 0)], &mut out).is_err(),
            "{label}"
        );
        assert!(
            read_random_1b(&body, &[u64::MAX], &mut out).is_err(),
            "{label}"
        );
        assert!(read_random_1b(&body, &[n], &mut out).is_err(), "{label}");
    }
}

// ---------------------------------------------------------------------------
// Attack 5: non-withdrawal across `grow`
// ---------------------------------------------------------------------------

#[test]
fn grow_is_non_withdrawing_and_older_generation_recovers() {
    let dir = temp_dir("grow");
    let path = dir.join("g.ekb");
    let src = pattern(4, 2000);

    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();
    let prefix = std::fs::read(&path).unwrap();
    let before = read(&body, req(0, body.logical_len)).unwrap();

    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    append_generation(&path, &body).unwrap();

    let after = std::fs::read(&path).unwrap();
    assert!(after.len() > prefix.len());
    assert_eq!(
        &after[..prefix.len()],
        &prefix[..],
        "grow rewrote already-committed bytes"
    );

    let opened = ekbloom::reader::open(&path).unwrap();
    assert_eq!(opened.generation, 2);
    assert_eq!(read(&opened, req(0, opened.logical_len)).unwrap(), before);

    // Simulate a torn newest commit: the older generation must remain valid.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(after.len() as u64 - 20)
        .unwrap();
    let report = scan_generations(&path).unwrap();
    assert_eq!(report.newest_valid, Some(1));
    assert!(report.torn_tail_bytes > 0);

    let fallback = ekbloom::reader::open(&path).unwrap();
    assert_eq!(fallback.generation, 1);
    assert_eq!(
        read(&fallback, req(0, fallback.logical_len)).unwrap(),
        before
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Attack 6: MANIFEST / header mismatch
// ---------------------------------------------------------------------------

#[test]
fn manifest_header_mismatch_is_rejected() {
    let dir = temp_dir("manifest");
    let path = dir.join("m.ekb");
    let src = pattern(4, 400);
    let body = create(&src, ManifestationProfile::MaxSpeed).unwrap();
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(open_bytes(&bytes).is_ok());
    let n = src.len() as u64;

    // Header layout: magic(8) major(2) minor(2) len(4) flags(8) object_id(16)
    // logical_symbols(8) logical_bytes(8) core_len(8) machine_id(4) reserved(12).
    // Make the header's declared logical length disagree with the MANIFEST's.
    let mut t = bytes.clone();
    t[40..48].copy_from_slice(&(n + 1).to_le_bytes());
    t[48..56].copy_from_slice(&(n + 1).to_le_bytes());
    assert!(
        matches!(open_bytes(&t), Err(EkbloomError::CorruptGeneration { .. })),
        "a MANIFEST/header logical-length mismatch must be rejected"
    );

    // A core_len beyond the file must be a clean error, not a panic.
    let mut t2 = bytes.clone();
    t2[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(open_bytes(&t2).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}
