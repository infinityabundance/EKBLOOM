//! Integration tests for background CPU construction (`worker`) and asynchronous
//! publication with synchronous fallback (`asyncpub`).
//!
//! The parallel construction paths must agree byte-for-byte with their
//! sequential counterparts, publication must round-trip through the reader, and
//! detection must never panic and must always leave a readable, verified file.

use std::path::{Path, PathBuf};

use ekbloom::asyncpub::{append, detect_backend, publish, Backend};
use ekbloom::bloom_region::build_regional;
use ekbloom::machine::{OrMachine, NL2};
use ekbloom::worker::{available_parallelism, manifest_background, regional_parallel};
use ekbloom::{create, grow, manifest, open, read_file, verify, ManifestationProfile, ReadRequest};

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
        "ekbloom-async-build-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_source_round_trip(path: &Path, src: &[u8]) {
    let opened = open(path).unwrap();
    assert!(
        verify(&opened).unwrap().ok,
        "verification failed for {path:?}"
    );
    assert_eq!(
        read_file(
            path,
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
fn regional_parallel_equals_sequential_byte_for_byte() {
    for (seed, len) in [(0xABCD_0001u64, 777usize), (0xABCD_0002, 4096)] {
        let src = lcg(seed, len);
        let core = NL2.encode(&src).unwrap();
        let logical_len = src.len() as u64;
        for depth in [1u16, 2, 4, 8] {
            for region_bytes in [16u32, 64, 512] {
                let seq = build_regional(&core, logical_len, depth, region_bytes, 3).unwrap();
                for workers in [1usize, 2, 4, 8] {
                    let par =
                        regional_parallel(&core, logical_len, depth, region_bytes, 3, workers)
                            .unwrap();
                    assert_eq!(
                        par, seq,
                        "seed={seed} len={len} D={depth} R={region_bytes} workers={workers}"
                    );
                }
            }
        }
    }
}

#[test]
fn regional_parallel_handles_empty_and_zero_workers() {
    let core = NL2.encode(b"").unwrap();
    assert!(regional_parallel(&core, 0, 4, 64, 1, 0).unwrap().is_empty());
    // `workers = 0` is clamped to 1, never a hang or panic.
    let src = lcg(0xABCD_0003, 256);
    let core = NL2.encode(&src).unwrap();
    let par = regional_parallel(&core, src.len() as u64, 4, 32, 1, 0).unwrap();
    let seq = build_regional(&core, src.len() as u64, 4, 32, 1).unwrap();
    assert_eq!(par, seq);
}

#[test]
fn manifest_background_equals_sync_manifest() {
    let src = lcg(0xC0DE_0001, 3000);
    let base = create(&src, ManifestationProfile::Root).unwrap();
    let profile = ManifestationProfile::Regional {
        depth: 4,
        region_bytes: 256,
    };

    let mut reference = base.clone();
    manifest(&mut reference, profile).unwrap();

    let got = manifest_background(base, profile, 4)
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(got.generation, reference.generation);
    assert_eq!(got.physical_bytes(), reference.physical_bytes());
    assert_eq!(got.blooms, reference.blooms);
    assert_eq!(got.certificates, reference.certificates);
    assert!(verify(&got).unwrap().ok);
}

#[test]
fn publish_sync_then_append_shows_new_generation() {
    let dir = temp_dir("sync-append");
    let path = dir.join("obj.ekb");
    let src = lcg(0x5050_0001, 1024);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();

    publish(&path, &body, Backend::Sync).unwrap();
    assert_source_round_trip(&path, &src);

    grow(&mut body, ManifestationProfile::Sparse { interval: 256 }).unwrap();
    let generation = append(&path, &body, Backend::Sync).unwrap();
    assert_eq!(generation, body.generation);

    let opened = open(&path).unwrap();
    assert_eq!(opened.generation, generation);
    assert_eq!(opened.checkpoints.len(), 1);
    // Earlier committed bytes are never rewritten.
    assert_source_round_trip(&path, &src);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn detection_never_panics_and_detected_path_round_trips() {
    let dir = temp_dir("detected");
    let src = lcg(0xDEAD_0001, 1536);
    let body = create(&src, ManifestationProfile::Root).unwrap();

    let detected = detect_backend();
    assert!(matches!(detected, Backend::Sync | Backend::IoUring));

    let via_detected = dir.join("detected.ekb");
    publish(&via_detected, &body, detected).unwrap();
    assert_source_round_trip(&via_detected, &src);

    // The detected backend must produce the same bytes as the sync path.
    let via_sync = dir.join("sync.ekb");
    publish(&via_sync, &body, Backend::Sync).unwrap();
    assert_eq!(
        std::fs::read(&via_detected).unwrap(),
        std::fs::read(&via_sync).unwrap()
    );

    match detected {
        Backend::IoUring => {
            // Exercise the io-uring path explicitly; it must match sync.
            let via_uring = dir.join("uring.ekb");
            publish(&via_uring, &body, Backend::IoUring).unwrap();
            assert_source_round_trip(&via_uring, &src);
            assert_eq!(
                std::fs::read(&via_uring).unwrap(),
                std::fs::read(&via_sync).unwrap()
            );
        }
        Backend::Sync => {
            // No io_uring here; the sync fallback must already have worked.
            assert_eq!(detected, Backend::Sync);
        }
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn concurrent_background_manifests_all_verify() {
    let profile = ManifestationProfile::Regional {
        depth: 4,
        region_bytes: 128,
    };
    let jobs: Vec<_> = (0..8u64)
        .map(|i| {
            let src = lcg(0xF00D_0000 + i, 700 + (i as usize) * 37);
            let body = create(&src, ManifestationProfile::Root).unwrap();
            (
                manifest_background(body, profile, available_parallelism()),
                src,
            )
        })
        .collect();

    for (handle, src) in jobs {
        let body = handle.join().unwrap().unwrap();
        assert_eq!(body.logical_len, src.len() as u64);
        assert!(
            verify(&body).unwrap().ok,
            "concurrent manifest failed verify"
        );
        assert_eq!(
            ekbloom::read(
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
}
