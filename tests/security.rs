//! Security regressions for untrusted `.ekb` and wire-form handling.
//!
//! Each test crafts a concrete input and asserts the parser/reader either
//! returns a clean `Err` or detects the forgery, never panicking and never
//! attempting an attacker-sized allocation. All temporary state lives under a
//! unique `std::env::temp_dir()` subdirectory that is removed afterwards.

use std::path::PathBuf;

use ekbloom::{
    create, read, reader, verify, write_body, BloomKind, CheckpointSet, EkbloomError, History,
    ManifestationProfile, ReadRequest, Snapshot,
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

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-security-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Finding A: a crafted file declares a huge `logical_len` over a tiny core and
/// carries a checksum-valid checkpoint. Before the fix, `open` accepted it and a
/// subsequent `read` sized a buffer from the declaration (`Vec::with_capacity`),
/// i.e. an untrusted-input allocation / OOM vector. It must now be rejected at
/// open, before any read path can run.
#[test]
fn huge_logical_len_over_small_core_is_rejected() {
    let dir = temp_dir("huge-len");
    let path = dir.join("huge.ekb");

    let mut body = create(b"abc", ManifestationProfile::Root).unwrap();
    // Self-consistent file produced by the project's own writer: the header and
    // manifest both claim `logical_len = u64::MAX` while the core stays 3 bytes.
    body.logical_len = u64::MAX;
    body.checkpoints.push(CheckpointSet {
        interval: 1,
        count: 1,
        resume: vec![Snapshot::new(0, 0)],
    });
    write_body(&path, &body).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let opened = reader::open_bytes(&bytes);
    assert!(
        matches!(opened, Err(EkbloomError::CorruptGeneration { .. })),
        "a logical_len larger than the core must be rejected at open"
    );
    // A legitimate body of the same shape still opens and reads (guard is exact).
    let ok = create(b"abc", ManifestationProfile::Root).unwrap();
    let ok_bytes = {
        let p = dir.join("ok.ekb");
        write_body(&p, &ok).unwrap();
        std::fs::read(&p).unwrap()
    };
    let ok_body = reader::open_bytes(&ok_bytes).unwrap();
    assert_eq!(
        read(
            &ok_body,
            ReadRequest {
                offset: 0,
                length: 3
            }
        )
        .unwrap(),
        b"abc".to_vec()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Finding B: the frozen contract defines `feature_flags` as always zero. A
/// nonzero value advertises semantics this build does not implement, so it must
/// be refused instead of silently ignored.
#[test]
fn nonzero_feature_flags_are_rejected() {
    let dir = temp_dir("flags");
    let path = dir.join("flags.ekb");
    let body = create(b"xyz", ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(reader::open_bytes(&bytes).is_ok());

    let mut tampered = bytes.clone();
    // feature_flags is the u64 after magic(8) + major(2) + minor(2) + len(4).
    tampered[16..24].copy_from_slice(&1u64.to_le_bytes());
    assert!(
        reader::open_bytes(&tampered).is_err(),
        "nonzero feature_flags must be rejected"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Finding C: checksums protect integrity-in-transit, not authenticity. A
/// forged-but-valid checkpoint state (`resume[0].state`, cursor still legal) is
/// accepted by `open`, but the documented integrity engine (`verify`) must
/// detect it, and `read` must never panic on it.
#[test]
fn forged_checkpoint_state_is_detected_by_verify() {
    let dir = temp_dir("forge-cp");
    let path = dir.join("forge.ekb");
    let src = lcg(0xC0FFEE, 64);

    let mut body = create(&src, ManifestationProfile::Sparse { interval: 1 }).unwrap();
    assert!(!body.checkpoints.is_empty());
    // Genesis state x_0 must be 0; forge it while keeping the cursor legal.
    body.checkpoints[0].resume[0].state ^= 0xFFFF;
    write_body(&path, &body).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    match reader::open_bytes(&bytes) {
        Ok(opened) => {
            let report = verify(&opened).unwrap();
            assert!(
                !report.ok,
                "verify must flag a forged checkpoint successor state"
            );
            // Read must be total (no panic) even though the structure lies.
            let _ = read(
                &opened,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64,
                },
            );
        }
        Err(_) => {
            // Strengthening `open` to reject this is also acceptable.
        }
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Defense-in-depth: a declared count must never drive an allocation larger than
/// the input could possibly justify; the checked-arithmetic parsers reject these.
#[test]
fn declared_counts_cannot_force_huge_allocations() {
    // Checkpoint set: interval 1, count u64::MAX, no resume bytes.
    let mut cp = Vec::new();
    cp.extend_from_slice(&1u32.to_le_bytes());
    cp.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(CheckpointSet::from_bytes(&cp).is_err());

    // History: header claims logical_len = count = u64::MAX.
    let mut h = vec![0u8; 8 + 8 + 8 + 32 + 8];
    h[8..16].copy_from_slice(&(u64::MAX - 1).to_le_bytes()); // logical_len
    h[56..64].copy_from_slice(&u64::MAX.to_le_bytes()); // count
    assert!(History::from_bytes(&h).is_err());

    // Bloom shell: payload_len = u64::MAX over a short buffer.
    let mut shell = Vec::new();
    shell.extend_from_slice(&BloomKind::Path.to_u32().to_le_bytes());
    shell.extend_from_slice(&4u16.to_le_bytes());
    shell.extend_from_slice(&0u64.to_le_bytes()); // generation
    shell.extend_from_slice(&0u64.to_le_bytes()); // logical_len
    shell.extend_from_slice(&0u64.to_le_bytes()); // entries
    shell.extend_from_slice(&0u32.to_le_bytes()); // status code
    shell.extend_from_slice(&[0u8; 32]); // core_hash
    shell.extend_from_slice(&u64::MAX.to_le_bytes()); // payload_len
    assert!(ekbloom::BloomShell::from_bytes(&shell).is_err());
}

/// Finding C mitigation: `open` recomputes `blake3(core)` and cross-checks it
/// against every structure carrying a `core_hash` (history, bloom shells). A
/// structure whose field no longer matches the core is rejected at open, before
/// any read can use it. The crc32 over the section stays valid because the
/// writer recomputes it, so this is a genuine authenticity-of-linkage check.
#[test]
fn forged_structure_core_hash_is_rejected_at_open() {
    let dir = temp_dir("forge-corehash");
    let path = dir.join("f.ekb");
    let src = lcg(0x1234, 128);

    // History carries a core_hash field; forge it while keeping crc32 valid.
    let mut with_history = create(&src, ManifestationProfile::Full).unwrap();
    with_history.history.as_mut().unwrap().core_hash[0] ^= 0xFF;
    write_body(&path, &with_history).unwrap();
    assert!(
        matches!(
            reader::open_bytes(&std::fs::read(&path).unwrap()),
            Err(EkbloomError::CorruptGeneration { .. })
        ),
        "forged history core_hash must be rejected at open"
    );

    // Bloom shells likewise carry a core_hash field.
    let mut with_bloom = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    with_bloom.blooms[0].core_hash[0] ^= 0xFF;
    write_body(&path, &with_bloom).unwrap();
    assert!(
        matches!(
            reader::open_bytes(&std::fs::read(&path).unwrap()),
            Err(EkbloomError::CorruptGeneration { .. })
        ),
        "forged bloom core_hash must be rejected at open"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Fuzz-style totality: byte flips and truncations of a valid image must never
/// panic, and any accepted body must stay within the input's size budget.
#[test]
fn mutated_and_truncated_images_never_panic() {
    let dir = temp_dir("mut");
    let path = dir.join("m.ekb");
    let body = create(&lcg(7, 512), ManifestationProfile::MaxSpeed).unwrap();
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    for cut in 0..bytes.len().min(600) {
        let _ = reader::open_bytes(&bytes[..cut]);
    }
    for i in 0..bytes.len().min(400) {
        let mut b = bytes.clone();
        b[i] ^= 0x80;
        if let Ok(body) = reader::open_bytes(&b) {
            // A mutated image that still parses must not exceed the input size.
            assert!(body.logical_len <= b.len() as u64);
            let _ = read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: body.logical_len.min(b.len() as u64),
                },
            );
        }
    }

    std::fs::remove_dir_all(&dir).unwrap();
}
