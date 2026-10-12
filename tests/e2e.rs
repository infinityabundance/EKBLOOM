//! End-to-end EKBLOOM/OrFS proof — integration tests.
//!
//! These run on the **real corpora** the container provides ([`common::load`]) and
//! pin the properties `examples/e2e.rs` depends on:
//!
//! * the corpora load and hash **deterministically**;
//! * every EKBLOOM read (direct `OrFs`, the persistent `OpenObject` handle, a
//!   `ReadyObject`, and the FUSE projection when a mount is available) is
//!   **byte-exact** against the source;
//! * EKBLOOM's on-disk bytes are **measured and > 0** (and consistent with the
//!   administrative `inspect_physical`);
//! * the conventional pinned-hot comparison stores are **byte-exact**.
//!
//! Corpora are capped at [`common::Caps::SMALL`] so the suite stays bounded; the
//! full-size run lives in `examples/e2e.rs`.

#[path = "common/e2e.rs"]
mod common;

use std::path::PathBuf;

use ekbloom::engine::ReadyObject;
use ekbloom::{ManifestationProfile, OpenObject, OrFs, ReadRequest};

/// A fresh scratch directory under the scratch root (`EKB_E2E_SCRATCH`, default
/// `target/`).
fn scratch(name: &str) -> PathBuf {
    let base = std::env::var("EKB_E2E_SCRATCH").unwrap_or_else(|_| "target".to_string());
    let dir = PathBuf::from(base).join(format!("e2e-test-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The deterministically-first real corpus under the small test caps.
fn test_corpus() -> common::Corpus {
    common::load(common::Caps::SMALL)
        .into_iter()
        .find(|c| c.name == "text")
        .expect("a text corpus")
}

/// A few in-bounds, deterministic read offsets.
fn probes(n: u64, len: u64) -> Vec<u64> {
    let mut offsets = vec![0, (n / 3) & !4095, (n / 2) & !4095];
    offsets.retain(|&o| o + len <= n);
    offsets.dedup();
    offsets
}

#[test]
fn corpora_load_and_hash_deterministically() {
    let first = common::load(common::Caps::SMALL);
    let second = common::load(common::Caps::SMALL);
    assert_eq!(first.len(), 3);
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.source, b.source, "{} corpus is not deterministic", a.name);
        assert_eq!(common::blake3_hex(&a.source), common::blake3_hex(&b.source));
        assert!(!a.source.is_empty(), "{} corpus is empty", a.name);
        assert!(!a.origin.is_empty(), "{} corpus has no provenance", a.name);
    }
}

#[test]
fn ekbloom_reads_and_on_disk_bytes_are_exact() {
    let corpus = test_corpus();
    let source = &corpus.source;
    let n = source.len() as u64;
    assert!(n > 4096);

    let root = scratch("ekbloom");
    let fs = OrFs::open(&root).unwrap();
    fs.create(
        "data/obj.bin",
        source,
        ManifestationProfile::Path { depth: 4 },
    )
    .unwrap();

    // Administrative on-disk accounting, asserted real and positive.
    let report = fs.inspect_physical().unwrap();
    assert!(report.total_bytes > 0, "on-disk bytes must be > 0");
    assert_eq!(report.logical_bytes, n);
    assert_eq!(report.objects.len(), 1);
    let ekb = common::concealed_ekb(&root);
    assert!(
        common::du_bytes(&ekb) > 0,
        "the authoritative .ekb is empty"
    );
    assert!(common::du_bytes(&ekb) <= report.total_bytes);

    // Direct `OrFs` reads are byte-exact.
    for off in probes(n, 4096) {
        let got = fs
            .read(
                "data/obj.bin",
                ReadRequest {
                    offset: off,
                    length: 4096,
                },
            )
            .unwrap();
        assert_eq!(got, &source[off as usize..off as usize + 4096]);
    }

    // A persistent `OpenObject` handle is byte-exact.
    let handle = OpenObject::open(&ekb).unwrap();
    for off in probes(n, 4096) {
        let got = handle
            .read(ReadRequest {
                offset: off,
                length: 4096,
            })
            .unwrap();
        assert_eq!(got, &source[off as usize..off as usize + 4096]);
    }

    // A `ReadyObject` over the reopened `.ekb` is byte-exact.
    let body = ekbloom::reader::open(&ekb).unwrap();
    let ready = ReadyObject::prepare(body).unwrap();
    for off in probes(n, 8192) {
        let got = ready
            .read(ReadRequest {
                offset: off,
                length: 8192,
            })
            .unwrap();
        assert_eq!(got, &source[off as usize..off as usize + 8192]);
    }
}

#[test]
fn comparison_stores_are_byte_exact() {
    let corpus = test_corpus();
    let source = &corpus.source;
    let n = source.len() as u64;
    let dir = scratch("stores");
    let mut stores = [
        ("raw", common::Store::raw(&dir.join("raw"), source).unwrap()),
        (
            "lz4",
            common::Store::lz4(&dir.join("lz4"), source, 64 * 1024).unwrap(),
        ),
        (
            "zstd",
            common::Store::zstd(&dir.join("zstd"), source, 64 * 1024).unwrap(),
        ),
    ];
    for (name, store) in stores.iter_mut() {
        assert!(
            store.on_disk_bytes() > 0,
            "{name} on-disk bytes must be > 0"
        );
        assert_eq!(store.logical_len(), n);
        for off in probes(n, 4096) {
            let got = store.read(off, 4096);
            assert_eq!(
                got,
                &source[off as usize..off as usize + 4096],
                "{name} read at {off} is not byte-exact"
            );
        }
    }
    // The pinned-hot stores really retain decoded blocks permanently.
    assert!(stores[1].1.pinned_blocks() > 0);
    assert!(stores[2].1.pinned_blocks() > 0);
}

/// The FUSE projection must serve byte-exact reads end-to-end. Skipped (with a
/// message) when `/dev/fuse` is unavailable or the mount cannot be created.
#[cfg(all(feature = "fuse", target_os = "linux"))]
#[test]
fn fuse_mount_reads_are_byte_exact() {
    if std::fs::metadata("/dev/fuse").is_err() {
        eprintln!("SKIP: /dev/fuse is not available");
        return;
    }
    let corpus = test_corpus();
    let source = &corpus.source;
    let root = scratch("fuse-root");
    {
        let fs = OrFs::open(&root).unwrap();
        fs.create(
            "data/obj.bin",
            source,
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();
    }
    let mnt = scratch("fuse-mnt");
    let fs = OrFs::open(&root).unwrap();
    let handle = match ekbloom::mount(fs, &mnt, false) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("SKIP: FUSE mount failed: {e}");
            return;
        }
    };
    let got = std::fs::read(mnt.join("data/obj.bin")).unwrap();
    assert_eq!(&got, source, "FUSE read is not byte-exact");
    drop(handle);
}
