//! Integration tests for the hybrid batched `io_uring` read path
//! ([`ekbloom::async_io`], available under the `io-uring` feature).
//!
//! What is asserted, whether or not a ring can be created here:
//!
//! * a batch of cold reads returns byte-exact data mapped by request index, and a
//!   batch larger than the ring's submission capacity is chunked while preserving
//!   order;
//! * the transparent `pread` fallback returns exactly the same bytes as the ring
//!   path;
//! * a **single/hot** read ([`HybridReader::read_direct`]) never submits to the
//!   ring;
//! * batched reads interleaved with concurrent generation publication through the
//!   ring never observe a partial shell: every intermediate generation is complete
//!   and verifies, and the logical content is stable.

#![cfg(all(feature = "io-uring", target_os = "linux"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ekbloom::async_io::{BatchRead, HybridReader};
use ekbloom::{
    append, create, grow, open, publish, read_file, verify, Backend, ManifestationProfile,
    ReadRequest,
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
        "ekbloom-io-batch-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A shuffled, mixed-size batch must come back byte-exact and in request order.
#[test]
fn batched_reads_are_byte_exact_and_ordered() {
    let dir = temp_dir("ordered");
    let src = lcg(0x10BA_7C41, 256 * 1024);
    let path = dir.join("blob.bin");
    std::fs::write(&path, &src).unwrap();

    let mut reader = HybridReader::open(&path).unwrap();
    let reqs = vec![
        BatchRead::new(200_000, 4096),
        BatchRead::new(0, 1),
        BatchRead::new(131_072, 65_536),
        BatchRead::new(4096, 16),
        BatchRead::new(255_000, 24),
        BatchRead::new(100_000, 1024),
    ];
    let got = reader.read_batch(&reqs).unwrap();
    assert_eq!(got.len(), reqs.len());
    for (buf, req) in got.iter().zip(reqs.iter()) {
        let start = req.offset as usize;
        let end = start + req.length as usize;
        assert_eq!(
            buf,
            &src[start..end],
            "request {req:?} returned wrong bytes"
        );
    }
    if reader.ring_available() {
        assert!(
            reader.ring_submissions() >= reqs.len() as u64,
            "a ring-backed batch must submit its reads"
        );
    } else {
        assert_eq!(reader.fallback_batches(), 1);
    }

    // A batch larger than the ring's submission capacity is chunked; ordering and
    // byte-exactness must still hold.
    let big: Vec<BatchRead> = (0..300)
        .map(|i| BatchRead::new((i * 512) as u64, 512))
        .collect();
    let got = reader.read_batch(&big).unwrap();
    assert_eq!(got.len(), big.len());
    for (i, buf) in got.iter().enumerate() {
        let start = i * 512;
        assert_eq!(buf, &src[start..start + 512]);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// The `pread` fallback returns exactly the same bytes as the ring path.
#[test]
fn fallback_path_is_byte_exact_and_matches_the_ring() {
    let dir = temp_dir("fallback");
    let src = lcg(0xFA11_BACC, 64 * 1024);
    let path = dir.join("blob.bin");
    std::fs::write(&path, &src).unwrap();

    let reqs: Vec<BatchRead> = (0..16)
        .map(|i| BatchRead::new((i * 4000) as u64, 1024))
        .collect();

    let mut forced = HybridReader::open_pread_only(&path).unwrap();
    assert!(
        !forced.ring_available(),
        "pread-only reader must have no ring"
    );
    let fallback = forced.read_batch(&reqs).unwrap();
    assert_eq!(forced.fallback_batches(), 1);
    assert_eq!(forced.ring_submissions(), 0);

    let mut ring = HybridReader::open(&path).unwrap();
    let via_ring = ring.read_batch(&reqs).unwrap();
    assert_eq!(
        via_ring, fallback,
        "ring and fallback must agree byte-for-byte"
    );

    for (buf, req) in fallback.iter().zip(reqs.iter()) {
        let start = req.offset as usize;
        assert_eq!(buf, &src[start..start + req.length as usize]);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// A single/hot read must never submit a kernel operation to the ring.
#[test]
fn single_hot_reads_never_touch_the_ring() {
    let dir = temp_dir("hot");
    let src = lcg(0x40_70_70_07, 32 * 1024);
    let path = dir.join("blob.bin");
    std::fs::write(&path, &src).unwrap();

    let reader = HybridReader::open(&path).unwrap();
    let before = reader.ring_submissions();
    assert_eq!(before, 0);
    for i in 0..64u64 {
        let off = (i * 97) % (src.len() as u64 - 8);
        let got = reader.read_direct(off, 8).unwrap();
        assert_eq!(got, &src[off as usize..off as usize + 8]);
    }
    assert_eq!(
        reader.ring_submissions(),
        before,
        "single reads must not submit anything to the ring"
    );

    // The same holds for a reader that has no ring at all.
    let forced = HybridReader::open_pread_only(&path).unwrap();
    assert_eq!(forced.read_direct(0, 16).unwrap(), src[..16].to_vec());
    assert_eq!(forced.ring_submissions(), 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Batched reads interleaved with concurrent generation publication through the
/// ring: data+fsync ordering is preserved and no reader ever sees a partial shell.
#[test]
fn mixed_reads_and_concurrent_publication_through_the_ring() {
    let dir = temp_dir("mixed");
    let path = dir.join("obj.ekb");
    let src = lcg(0x1357_9BDF, 128 * 1024);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    publish(&path, &body, Backend::IoUring).unwrap();

    // A reader that batches reads of the file's **stable prefix** (the header and
    // core, which appends never rewrite) while new generations are appended through
    // the ring.
    let expected = std::fs::read(&path).unwrap()[..4096].to_vec();
    let stop = Arc::new(AtomicU64::new(0));
    let progress = Arc::new(AtomicU64::new(0));
    let reader_path = path.clone();
    let stop_reader = Arc::clone(&stop);
    let progress_reader = Arc::clone(&progress);
    let reader = std::thread::spawn(move || {
        let mut hr = HybridReader::open(&reader_path).unwrap();
        let mut batches = 0u64;
        while stop_reader.load(Ordering::Relaxed) == 0 {
            let reqs: Vec<BatchRead> = (0..8)
                .map(|i| BatchRead::new((i * 512) as u64, 512))
                .collect();
            let got = hr.read_batch(&reqs).unwrap();
            for (i, buf) in got.iter().enumerate() {
                let start = i * 512;
                assert_eq!(buf, &expected[start..start + 512]);
            }
            batches += 1;
            progress_reader.store(batches, Ordering::Relaxed);
        }
        batches
    });

    const GENERATIONS: u64 = 12;
    let mut grown = create(&src, ManifestationProfile::Root).unwrap();
    for g in 2..=GENERATIONS {
        grow(&mut grown, ManifestationProfile::Sparse { interval: 4096 }).unwrap();
        let committed = append(&path, &grown, Backend::IoUring).unwrap();
        assert_eq!(committed, g);
        // Each committed generation is complete: it verifies, i.e. no partial shell
        // was ever advertised.
        let opened = open(&path).unwrap();
        assert_eq!(opened.generation, g);
        assert!(verify(&opened).unwrap().ok, "generation {g} did not verify");
    }

    stop.store(1, Ordering::Relaxed);
    let batches = reader.join().unwrap();
    assert!(
        batches > 0,
        "the reader made no progress against publication"
    );
    assert!(progress.load(Ordering::Relaxed) > 0);

    let final_open = open(&path).unwrap();
    assert_eq!(final_open.generation, GENERATIONS);
    assert!(verify(&final_open).unwrap().ok);
    assert_eq!(
        read_file(
            &path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
