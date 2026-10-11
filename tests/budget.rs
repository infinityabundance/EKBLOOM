//! Hard physical-byte budget enforcement (`ManifestationBudget`).
//!
//! These tests pin the four budget entry points to real, transactional hard
//! ceilings: rejection uses [`EkbloomError::AllocationTooLarge`], a rejected
//! `manifest_within_budget` leaves the body byte-for-byte and generation-for-
//! generation unchanged, `estimate_physical_bytes` never under-estimates the real
//! accounted `physical_bytes()`, and the CLI flag fails closed when too small.

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use ekbloom::{
    create, create_within_budget, estimate_physical_bytes, estimate_physical_bytes_for_core,
    manifest_within_budget, select_profile_within_budget, EkbloomError, ManifestationBudget,
    ManifestationProfile,
};

/// Deterministic pseudorandom source bytes.
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

fn budget(bytes: u64) -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: bytes,
    }
}

#[test]
fn budget_below_root_is_rejected_and_no_body_is_produced() {
    let src = lcg(1, 256);
    let result = create_within_budget(
        &src,
        ManifestationProfile::Root,
        &budget(src.len() as u64 - 1),
    );
    match result {
        Err(EkbloomError::AllocationTooLarge { requested, limit }) => {
            assert_eq!(limit, src.len() as u64 - 1);
            assert!(requested > limit, "requested {requested} <= limit {limit}");
        }
        other => panic!("expected AllocationTooLarge, got {other:?}"),
    }

    // A budget exactly at the root size is enough: the core alone is the body.
    let ok = create_within_budget(&src, ManifestationProfile::Root, &budget(src.len() as u64))
        .expect("root fits its own size");
    assert_eq!(ok.physical_bytes(), src.len() as u64);
}

#[test]
fn path4_fits_but_full_does_not() {
    let src = lcg(2, 1024);
    let path4 = estimate_physical_bytes(src.len() as u64, ManifestationProfile::Path { depth: 4 });
    let full = estimate_physical_bytes(src.len() as u64, ManifestationProfile::Full);
    assert!(
        path4 < full,
        "path(4) {path4} should be cheaper than full {full}"
    );

    let b = budget(path4);
    let body = create_within_budget(&src, ManifestationProfile::Path { depth: 4 }, &b)
        .expect("path(4) fits");
    assert_eq!(body.physical_bytes(), path4, "path(4) estimate is exact");
    assert!(body.physical_bytes() <= b.maximum_physical_bytes);

    assert!(matches!(
        create_within_budget(&src, ManifestationProfile::Full, &b),
        Err(EkbloomError::AllocationTooLarge { .. })
    ));
}

#[test]
fn manifest_within_budget_is_transactional() {
    let src = lcg(3, 512);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let before = body.clone();

    // A budget that cannot accommodate a full history (5N+4) must reject, and the
    // rejected call must not perturb the body at all.
    let too_small = budget(body.physical_bytes() + 16);
    let result = manifest_within_budget(&mut body, ManifestationProfile::Full, &too_small);
    assert!(matches!(
        result,
        Err(EkbloomError::AllocationTooLarge { .. })
    ));
    assert_eq!(body.physical_bytes(), before.physical_bytes());
    assert_eq!(body.generation, before.generation);
    assert_eq!(body, before);

    // A budget that fits the full history commits exactly one generation.
    let full = estimate_physical_bytes(src.len() as u64, ManifestationProfile::Full);
    manifest_within_budget(&mut body, ManifestationProfile::Full, &budget(full))
        .expect("full history fits its own estimate");
    assert!(body.physical_bytes() <= full);
    assert_eq!(body.generation, before.generation + 1);
}

#[test]
fn select_profile_returns_a_fitting_fastest_profile_or_errors() {
    let n = 2048u64;
    let path4 = estimate_physical_bytes(n, ManifestationProfile::Path { depth: 4 });
    let full = estimate_physical_bytes(n, ManifestationProfile::Full);
    assert!(path4 < full);

    let candidates = [
        ManifestationProfile::Root,
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Full,
    ];
    let (chosen, bytes) = select_profile_within_budget(n, &budget(path4), &candidates)
        .expect("path(4) fits the budget");
    // Faster than root, cheaper than full: the path bloom wins.
    assert_eq!(chosen, ManifestationProfile::Path { depth: 4 });
    assert!(bytes <= path4);

    // Nothing fits a budget smaller than even root.
    assert!(matches!(
        select_profile_within_budget(n, &budget(path4 - 1), &[ManifestationProfile::Full]),
        Err(EkbloomError::AllocationTooLarge { .. })
    ));
}

#[test]
fn estimate_never_underestimates_actual_physical_bytes() {
    let profiles = [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Dense { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 4096,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Path { depth: 8 },
        ManifestationProfile::MaxSpeed,
    ];
    for &len in &[0usize, 1, 7, 64, 513, 4096] {
        let src = lcg(0x00B0_D6E7 + len as u64, len);
        for &profile in &profiles {
            let body = match create(&src, profile) {
                Ok(body) => body,
                Err(_) => continue, // not buildable at this length
            };
            let estimate = estimate_physical_bytes(src.len() as u64, profile);
            assert!(
                estimate >= body.physical_bytes(),
                "estimate {estimate} < actual {} for {profile:?} at len {len}",
                body.physical_bytes()
            );
        }
    }
}

#[test]
fn cli_max_physical_bytes_flag_fails_closed_when_too_small() {
    let dir = std::env::temp_dir().join(format!(
        "ekbloom-budget-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();

    let src = lcg(9, 512);
    let input = dir.join("in.bin");
    fs::write(&input, &src).unwrap();
    let out = dir.join("o.ekb");
    let bin = env!("CARGO_BIN_EXE_ekbloom");

    let full = estimate_physical_bytes(src.len() as u64, ManifestationProfile::Full);
    let too_small = (full - 1).to_string();
    let failed = Command::new(bin)
        .args([
            "create",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--profile",
            "full",
            "--max-physical-bytes",
            too_small.as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        !failed.status.success(),
        "budget below full must exit nonzero"
    );
    assert!(!out.exists(), "no output file may be produced on rejection");

    let generous = full.to_string();
    let ok = Command::new(bin)
        .args([
            "create",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--profile",
            "full",
            "--max-physical-bytes",
            generous.as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "generous budget must succeed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(out.exists());

    let _ = fs::remove_dir_all(&dir);
}

/// A skewed (low-entropy) source that machine 2 compresses (`V < N`).
fn skewed(len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b" etaoinshrdlu,.ET\"'\n";
    const WEIGHTS: &[u32] = &[40, 25, 18, 15, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 3, 2, 2];
    let total: u32 = WEIGHTS.iter().sum();
    let mut s: u64 = 0x5EED_1234_ABCD_0001;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut pick = ((s >> 33) as u32) % total;
        for (i, &w) in WEIGHTS.iter().enumerate() {
            if pick < w {
                out.push(ALPHABET[i % ALPHABET.len()]);
                break;
            }
            pick -= w;
        }
    }
    out
}

/// FIX 2: a machine-2 budget must be judged against the **actual compressed
/// core**, not the source length. A config whose real core fits is accepted even
/// when the source length alone would over-reject; an over-budget one is rejected
/// transactionally. Machine 1 is unchanged.
#[test]
fn machine2_budget_uses_the_actual_core_length() {
    use ekbloom::machine::MACHINE_RANS0;
    use ekbloom::machine_rans::RANS0;
    use ekbloom::manifest::create_machine_within_budget;
    use ekbloom::{ManifestationProfile, OrMachine};

    let src = skewed(16 * 1024);
    let core_len = RANS0.encode(&src).unwrap().len() as u64;
    let source_len = src.len() as u64;
    assert!(core_len < source_len, "machine 2 must compress this source");

    // The source-length prefilter would have rejected this budget; the actual
    // compressed core fits it exactly.
    let naive = estimate_physical_bytes(source_len, ManifestationProfile::Root);
    assert!(naive > core_len, "source-length estimate must over-reject");

    let root_budget = budget(core_len);
    let body = create_machine_within_budget(
        &src,
        ManifestationProfile::Root,
        MACHINE_RANS0,
        &root_budget,
    )
    .expect("machine-2 root fits its real core");
    assert_eq!(body.machine_id, MACHINE_RANS0);
    assert_eq!(body.physical_bytes(), core_len);
    assert!(body.physical_bytes() <= root_budget.maximum_physical_bytes);
    assert_eq!(
        estimate_physical_bytes_for_core(core_len, source_len, ManifestationProfile::Root),
        core_len,
        "the core-aware estimate is exact for root"
    );

    // One byte under the real core is rejected, and no body is produced.
    let tight = budget(core_len - 1);
    assert!(matches!(
        create_machine_within_budget(&src, ManifestationProfile::Root, MACHINE_RANS0, &tight),
        Err(EkbloomError::AllocationTooLarge { requested, limit })
            if requested > limit && limit == core_len - 1
    ));

    // A machine-2 configuration with a manifested structure also accounts from
    // the actual core, and the machine-1 estimate is unchanged.
    let full_estimate =
        estimate_physical_bytes_for_core(core_len, source_len, ManifestationProfile::Full);
    assert!(full_estimate >= core_len);
    assert_eq!(
        estimate_physical_bytes(source_len, ManifestationProfile::Root),
        source_len,
        "machine 1 root estimate is its own size"
    );
}
