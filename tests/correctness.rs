//! The correctness court (directive §21).
//!
//! Every category is exercised explicitly and every fast path is asserted equal
//! to the authoritative machine — bytes *and* continuation state. Where a read
//! path uses a manifested structure (path bloom hot array, checkpoint restart,
//! full history) the answer must be byte-identical to replaying `OrANS-NL2`
//! from `Omega_0`, both via `reader::read` and via the `fastpath` entry points.
//!
//! Categories covered:
//!   * every byte value, empty input, single symbol;
//!   * repeating and alternating patterns, deterministic LCG pseudorandom;
//!   * region boundaries (sizes across `region_bytes`, reads crossing them);
//!   * final partial bloom group (lengths not divisible by `D`);
//!   * a bounded multi-MiB input;
//!   * history persistence (write `Full`, reopen, verify, random lookups);
//!   * checkpoint restart at every supported interval after reopen;
//!   * full-state random lookup;
//!   * multi-step successor correctness (`bloom` successor == `state_log[i + D]`);
//!   * concurrent readers sharing one `Arc<EkbloomBody>`;
//!   * interrupted shell publication (torn file -> earlier commit readable);
//!   * corrupted file rejection.

use std::path::PathBuf;
use std::sync::Arc;

use ekbloom::bloom::SUPPORTED_DEPTHS;
use ekbloom::bloom_path::PathBloom;
use ekbloom::checkpoint::{CheckpointSet, SUPPORTED_INTERVALS};
use ekbloom::fastpath;
use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::state::Snapshot;
use ekbloom::{create, grow, read, reader, recovery, verify, writer};
use ekbloom::{EkbloomBody, ManifestationProfile, ReadRequest};

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

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-correctness-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A representative spread of executable profiles.
fn representative_profiles() -> Vec<ManifestationProfile> {
    vec![
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Dense { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 64,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Path { depth: 8 },
        ManifestationProfile::MaxSpeed,
    ]
}

/// Deterministic in-bounds `(offset, length)` pairs, including empty reads and
/// the whole object.
fn sample_ranges(n: u64) -> Vec<(u64, u64)> {
    let mut ranges = vec![(0u64, 0u64), (0, n), (n, 0)];
    if n == 0 {
        return ranges;
    }
    let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ n;
    for _ in 0..12 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let offset = s % n;
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let remaining = n - offset;
        let length = s % (remaining + 1);
        ranges.push((offset, length));
    }
    ranges.push((n - 1, 1));
    ranges.push((n / 2, n - n / 2));
    if n > 3 {
        ranges.push((1, n - 2));
        ranges.push((n - 3, 3));
    }
    ranges
}

/// Deterministic single-byte offsets (empty when `n == 0`).
fn sample_offsets(n: u64) -> Vec<u64> {
    if n == 0 {
        return Vec::new();
    }
    let mut v = vec![0u64, n - 1, n / 2];
    let mut s = 0xDEAD_BEEFu64 ^ n;
    for _ in 0..32 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push(s % n);
    }
    v
}

/// Assert that every advertised read path returns exactly `src`.
fn assert_paths_match_source(body: &EkbloomBody, src: &[u8], tag: &str) {
    let n = body.logical_len;
    assert_eq!(NL2.encode(src).unwrap(), body.core, "{tag}: core mismatch");
    let reference = NL2.decode(&body.core, n).unwrap();
    assert_eq!(reference, src, "{tag}: authoritative decode mismatch");

    // Whole object through the public reader.
    let whole = read(
        body,
        ReadRequest {
            offset: 0,
            length: n,
        },
    )
    .unwrap();
    assert_eq!(whole, src, "{tag}: reader whole-object");

    // Fast sequential path (uses the certified path-bloom hot array when present).
    let mut seq = Vec::new();
    fastpath::read_sequential(body, &mut seq).unwrap();
    assert_eq!(seq, src, "{tag}: fast sequential");

    // Fast range path, compared against the source and against the reader.
    let ranges = sample_ranges(n);
    let mut got = Vec::new();
    fastpath::read_ranges(body, &ranges, &mut got).unwrap();
    let mut want = Vec::new();
    for &(o, l) in &ranges {
        let (o, l) = (o as usize, l as usize);
        want.extend_from_slice(&src[o..o + l]);
    }
    assert_eq!(got, want, "{tag}: fast ranges");
    for &(o, l) in &ranges {
        let one = read(
            body,
            ReadRequest {
                offset: o,
                length: l,
            },
        )
        .unwrap();
        let slice = &src[o as usize..(o + l) as usize];
        assert_eq!(one, slice.to_vec(), "{tag}: reader range ({o},{l})");
    }

    // Fast single-byte random path.
    let offsets = sample_offsets(n);
    let mut rand = Vec::new();
    fastpath::read_random_1b(body, &offsets, &mut rand).unwrap();
    let expect: Vec<u8> = offsets.iter().map(|&o| src[o as usize]).collect();
    assert_eq!(rand, expect, "{tag}: fast random-1b");
    for &o in &offsets {
        let one = read(
            body,
            ReadRequest {
                offset: o,
                length: 1,
            },
        )
        .unwrap();
        assert_eq!(one, vec![src[o as usize]], "{tag}: reader-1b @ {o}");
    }
}

// ---------------------------------------------------------------------------
// 1. Every byte value.
// ---------------------------------------------------------------------------

#[test]
fn category_every_byte_value() {
    let src: Vec<u8> = (0u8..=255).collect();
    for profile in representative_profiles() {
        let body = create(&src, profile).unwrap();
        assert_paths_match_source(&body, &src, &format!("every-byte {}", profile.label()));
    }
}

// ---------------------------------------------------------------------------
// 2. Empty input.
// ---------------------------------------------------------------------------

#[test]
fn category_empty_input() {
    let src: Vec<u8> = Vec::new();
    for profile in representative_profiles() {
        let body = create(&src, profile).unwrap();
        let tag = profile.label();
        assert_paths_match_source(&body, &src, &format!("empty {tag}"));

        // Out-of-bounds reads are rejected, zero-length-at-0 succeeds.
        assert!(read(
            &body,
            ReadRequest {
                offset: 0,
                length: 1
            }
        )
        .is_err());
        assert!(read(
            &body,
            ReadRequest {
                offset: 1,
                length: 0
            }
        )
        .is_err());
        assert!(read(
            &body,
            ReadRequest {
                offset: 0,
                length: 0
            }
        )
        .unwrap()
        .is_empty());
        let mut out = Vec::new();
        assert!(fastpath::read_random_1b(&body, &[0], &mut out).is_err());
        assert!(fastpath::read_ranges(&body, &[(1, 0)], &mut out).is_err());
    }
}

// ---------------------------------------------------------------------------
// 3. Single symbol.
// ---------------------------------------------------------------------------

#[test]
fn category_single_symbol() {
    for src in [vec![0x00u8], vec![0x01], vec![0x42], vec![0xFF]] {
        for profile in representative_profiles() {
            let body = create(&src, profile).unwrap();
            assert_paths_match_source(&body, &src, &format!("single {}", profile.label()));
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Repeating and alternating patterns.
// ---------------------------------------------------------------------------

#[test]
fn category_repeating_and_alternating() {
    let repeating: Vec<Vec<u8>> = vec![vec![0xA5u8; 300], vec![0x00u8; 255], vec![0xFFu8; 257]];
    let alternating: Vec<Vec<u8>> = vec![
        (0..301u32)
            .map(|i| if i % 2 == 0 { 0x11 } else { 0xEE })
            .collect(),
        (0..200u32)
            .map(|i| if i % 3 == 0 { 0x01 } else { 0xFE })
            .collect(),
    ];
    for (tag, src) in repeating
        .iter()
        .map(|s| ("repeating", s))
        .chain(alternating.iter().map(|s| ("alternating", s)))
    {
        for profile in representative_profiles() {
            let body = create(src, profile).unwrap();
            assert_paths_match_source(&body, src, &format!("{tag} {}", profile.label()));
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Deterministic LCG pseudorandom.
// ---------------------------------------------------------------------------

#[test]
fn category_lcg_pseudorandom() {
    for seed in [0x1234_5678u64, 0x0FED_CBA9_8765_4321, 0xDEAD_BEEF_CAFE_BABE] {
        for &len in &[17usize, 129, 1024, 4096] {
            let src = lcg(seed, len);
            for profile in representative_profiles() {
                let body = create(&src, profile).unwrap();
                assert_paths_match_source(
                    &body,
                    &src,
                    &format!("lcg({seed:#x},{len}) {}", profile.label()),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Region boundaries.
// ---------------------------------------------------------------------------

#[test]
fn category_region_boundaries() {
    for &rb in &[16u32, 64, 256] {
        let rb = rb as usize;
        for &n in &[
            rb - 1,
            rb,
            rb + 1,
            2 * rb - 1,
            2 * rb,
            2 * rb + 1,
            3 * rb + 5,
        ] {
            let src = lcg(0x0E61_0111 ^ n as u64, n);
            let body = create(
                &src,
                ManifestationProfile::Regional {
                    depth: 4,
                    region_bytes: rb as u32,
                },
            )
            .unwrap();
            let tag = format!("regional R={rb} n={n}");
            assert_paths_match_source(&body, &src, &tag);

            // Explicit reads crossing each region boundary.
            let nregions = n.div_ceil(rb);
            for k in 1..nregions {
                let b = k * rb;
                // Straddling read: one byte before through one byte after b.
                if b + 2 <= n {
                    let got = read(
                        &body,
                        ReadRequest {
                            offset: (b - 1) as u64,
                            length: 3,
                        },
                    )
                    .unwrap();
                    assert_eq!(got, src[b - 1..b + 2].to_vec(), "{tag}: crossing @ {b}");
                }
                // Read starting exactly on the boundary.
                if b < n {
                    let got = read(
                        &body,
                        ReadRequest {
                            offset: b as u64,
                            length: 1,
                        },
                    )
                    .unwrap();
                    assert_eq!(got, vec![src[b]], "{tag}: boundary byte @ {b}");
                }
                // Read ending exactly on the boundary.
                let got = read(
                    &body,
                    ReadRequest {
                        offset: (b - 1) as u64,
                        length: 1,
                    },
                )
                .unwrap();
                assert_eq!(got, vec![src[b - 1]], "{tag}: last byte of region @ {b}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Final partial bloom group.
// ---------------------------------------------------------------------------

#[test]
fn category_final_partial_bloom_group() {
    for &d in SUPPORTED_DEPTHS.iter() {
        for &n in &[1usize, 2, 3, 5, 7, 8, 9, 15, 17, 31, 33] {
            let src = lcg(0x9A17 ^ n as u64, n);
            let core = NL2.encode(&src).unwrap();
            let n64 = n as u64;
            let pb = PathBloom::from_shell(&PathBloom::build(&core, n64, d, 0).unwrap()).unwrap();
            let logs = state_log(&core, n64).unwrap();
            let decoded = NL2.decode(&core, n64).unwrap();
            let dd = u64::from(d);
            let nblocks = n64.div_ceil(dd);
            assert_eq!(pb.cold.len() as u64, nblocks, "D={d} n={n}");

            for pos in 0..n64 {
                assert_eq!(
                    pb.symbol_at(pos).unwrap(),
                    decoded[pos as usize],
                    "D={d} n={n} pos={pos}"
                );
            }

            let last = nblocks - 1;
            let start = last * dd;
            let end = ((last + 1) * dd).min(n64);
            let (syms, succ) = pb.block_at(last).unwrap();
            assert_eq!(syms.len() as u64, end - start, "D={d} n={n} last block len");
            assert_eq!(syms, decoded[start as usize..end as usize].to_vec());
            assert_eq!(succ, logs[end as usize], "D={d} n={n} last successor");
            if n64 % dd != 0 {
                assert!(
                    (syms.len() as u64) < dd,
                    "final block must be partial: D={d} n={n}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Multi-step successor correctness.
// ---------------------------------------------------------------------------

#[test]
fn category_multi_step_successor_matches_state_log() {
    let src = lcg(0x5EED_F00D, 200);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    let logs = state_log(&core, n).unwrap();
    for &d in SUPPORTED_DEPTHS.iter() {
        let pb = PathBloom::from_shell(&PathBloom::build(&core, n, d, 0).unwrap()).unwrap();
        let dd = u64::from(d);
        for j in 0..pb.cold.len() as u64 {
            let (_syms, succ) = pb.block_at(j).unwrap();
            let end = (dd * (j + 1)).min(n);
            assert_eq!(succ, logs[end as usize], "D={d} block={j}");
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Bounded large input.
// ---------------------------------------------------------------------------

#[test]
fn category_large_input_bounded() {
    let n = 2 * 1024 * 1024;
    let src = lcg(0x1A2B_3C4D, n);
    for profile in [
        ManifestationProfile::Root,
        ManifestationProfile::Path { depth: 4 },
    ] {
        let body = create(&src, profile).unwrap();
        assert_eq!(body.logical_len, n as u64);
        let reference = NL2.decode(&body.core, n as u64).unwrap();
        assert_eq!(reference, src, "large decode mismatch");

        for &o in &[0u64, 1, (n / 2) as u64, (n - 1) as u64] {
            let got = read(
                &body,
                ReadRequest {
                    offset: o,
                    length: 1,
                },
            )
            .unwrap();
            assert_eq!(got, vec![src[o as usize]], "large spot read @ {o}");
        }
        let mut seq = Vec::new();
        fastpath::read_sequential(&body, &mut seq).unwrap();
        assert_eq!(seq, src, "large fast sequential");
    }
}

// ---------------------------------------------------------------------------
// 10. History persistence.
// ---------------------------------------------------------------------------

#[test]
fn category_history_persistence_reopen() {
    let dir = temp_dir("history");
    let src = lcg(0x1F15_7047, 777);
    let n = src.len() as u64;
    let path = dir.join("full.ekb");

    let body = create(&src, ManifestationProfile::Full).unwrap();
    writer::write_body(&path, &body).unwrap();

    let opened = reader::open(&path).unwrap();
    assert!(verify(&opened).unwrap().ok);
    assert!(opened.history.is_some(), "history must persist");
    assert_eq!(opened.logical_len, n);

    for &o in &sample_offsets(n) {
        let got = read(
            &opened,
            ReadRequest {
                offset: o,
                length: 1,
            },
        )
        .unwrap();
        assert_eq!(got, vec![src[o as usize]], "reopened random lookup @ {o}");
    }
    assert_eq!(
        read(
            &opened,
            ReadRequest {
                offset: 0,
                length: n
            }
        )
        .unwrap(),
        src
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 11. Checkpoint restart at every supported interval, after reopen.
// ---------------------------------------------------------------------------

#[test]
fn category_checkpoint_restart_all_intervals_after_reopen() {
    let dir = temp_dir("checkpoint");
    let src = lcg(0x0CBE_C4EC, 300);
    let n = src.len() as u64;
    let core = NL2.encode(&src).unwrap();
    let logs = state_log(&core, n).unwrap();

    for &k in SUPPORTED_INTERVALS.iter() {
        let path = dir.join(format!("sparse-{k}.ekb"));
        let body = create(&src, ManifestationProfile::Sparse { interval: k }).unwrap();
        writer::write_body(&path, &body).unwrap();

        let opened = reader::open(&path).unwrap();
        assert!(verify(&opened).unwrap().ok);
        assert_eq!(opened.checkpoints.len(), 1);
        let cp = &opened.checkpoints[0];
        assert_eq!(cp.interval, k);

        // In-memory set restarts match the state log exactly.
        let in_memory = CheckpointSet::build(&opened.core, n, k).unwrap();
        for i in 0..=n {
            let want = Snapshot::new(logs[i as usize], i);
            assert_eq!(cp.restore(&opened.core, i).unwrap(), want, "k={k} i={i}");
            assert_eq!(
                in_memory.restore(&opened.core, i).unwrap(),
                want,
                "k={k} i={i}"
            );
        }

        for &o in &sample_offsets(n) {
            let got = read(
                &opened,
                ReadRequest {
                    offset: o,
                    length: 1,
                },
            )
            .unwrap();
            assert_eq!(got, vec![src[o as usize]], "k={k} random lookup @ {o}");
        }
        assert_eq!(
            read(
                &opened,
                ReadRequest {
                    offset: 0,
                    length: n
                }
            )
            .unwrap(),
            src
        );
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 12. Full-state random lookup.
// ---------------------------------------------------------------------------

#[test]
fn category_full_state_random_lookup() {
    let src = lcg(0xF011_5EED, 257);
    let n = src.len() as u64;
    let body = create(&src, ManifestationProfile::Full).unwrap();
    let logs = state_log(&body.core, n).unwrap();

    let history = body.history.as_ref().unwrap();
    for i in 0..=n {
        let want = Snapshot::new(logs[i as usize], i);
        assert_eq!(body.snapshot_at(i).unwrap(), want, "body snapshot @ {i}");
        assert_eq!(history.snapshot_at(i), Some(want), "history snapshot @ {i}");
    }
    for &o in &sample_offsets(n) {
        assert_eq!(body.symbol_at(o).unwrap(), src[o as usize], "symbol @ {o}");
    }
    assert!(body.snapshot_at(n + 1).is_err());
    assert!(body.symbol_at(n).is_err());
}

// ---------------------------------------------------------------------------
// 13. Concurrent readers over one shared body.
// ---------------------------------------------------------------------------

#[test]
fn category_concurrent_readers() {
    let n = 4096usize;
    let src = lcg(0xC0C0_1234, n);
    let body = Arc::new(create(&src, ManifestationProfile::MaxSpeed).unwrap());
    let threads = 8usize;
    let chunk = n / threads;
    let src_ref: &[u8] = &src;

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for t in 0..threads {
            let body = Arc::clone(&body);
            handles.push(scope.spawn(move || {
                let start = t * chunk;
                let end = if t + 1 == threads { n } else { start + chunk };

                // Disjoint range owned by this thread.
                let got = read(
                    &body,
                    ReadRequest {
                        offset: start as u64,
                        length: (end - start) as u64,
                    },
                )
                .unwrap();
                assert_eq!(got, src_ref[start..end].to_vec(), "thread {t} disjoint");

                // Overlapping whole-object read shared by every thread.
                let all = read(
                    &body,
                    ReadRequest {
                        offset: 0,
                        length: n as u64,
                    },
                )
                .unwrap();
                assert_eq!(all, src_ref.to_vec(), "thread {t} whole");

                // Interleaved single-byte reads.
                for i in (start..end).step_by(7) {
                    let one = read(
                        &body,
                        ReadRequest {
                            offset: i as u64,
                            length: 1,
                        },
                    )
                    .unwrap();
                    assert_eq!(one, vec![src_ref[i]], "thread {t} byte @ {i}");
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });
}

// ---------------------------------------------------------------------------
// 14. Interrupted shell publication (torn file -> earlier commit readable).
// ---------------------------------------------------------------------------

#[test]
fn category_interrupted_publication_torn_file() {
    let dir = temp_dir("torn");
    let src = lcg(0x7074_0000, 400);
    let n = src.len() as u64;
    let path = dir.join("x.ekb");

    // Generation 1: root. Generation 2: full history.
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body(&path, &body).unwrap();
    grow(&mut body, ManifestationProfile::Full).unwrap();
    writer::append_generation(&path, &body).unwrap();

    // Tear the final commit footer.
    let len = std::fs::metadata(&path).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(len - 20).unwrap();
    drop(file);

    let report = recovery::recover(&path).unwrap();
    assert_eq!(report.newest_valid, Some(1), "torn tail must fall back");
    assert!(report.torn_tail_bytes > 0);

    // The earlier committed generation is still fully readable and verifiable.
    let opened = reader::open(&path).unwrap();
    assert_eq!(opened.generation, 1);
    assert!(verify(&opened).unwrap().ok);
    assert_eq!(
        read(
            &opened,
            ReadRequest {
                offset: 0,
                length: n
            }
        )
        .unwrap(),
        src
    );

    // Repair removes only the torn tail, preserving the earlier commit.
    let repaired = recovery::truncate_to_valid(&path).unwrap();
    assert!(repaired.repaired);
    assert_eq!(repaired.newest_valid, Some(1));
    assert_eq!(
        recovery::scan_generations(&path).unwrap().torn_tail_bytes,
        0
    );
    let reopened = reader::open(&path).unwrap();
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: n
            }
        )
        .unwrap(),
        src
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 15. Corrupted file rejection.
// ---------------------------------------------------------------------------

#[test]
fn category_corrupted_file_rejected() {
    let dir = temp_dir("corrupt");
    let src = lcg(0xC0FF_EE00, 200);
    let path = dir.join("c.ekb");

    let body = create(&src, ManifestationProfile::Full).unwrap();
    writer::write_body(&path, &body).unwrap();
    let good = std::fs::read(&path).unwrap();

    // 1. A flipped core byte fails the core-hash linkage.
    let mut flipped = good.clone();
    flipped[90] ^= 0xFF;
    std::fs::write(&path, &flipped).unwrap();
    assert!(reader::open(&path).is_err(), "flipped core byte accepted");

    // 2. Foreign bytes are rejected outright.
    std::fs::write(&path, b"not an ekb file at all").unwrap();
    assert!(reader::open(&path).is_err());

    // 3. A truncated header is rejected.
    std::fs::write(&path, &good[..40]).unwrap();
    assert!(reader::open(&path).is_err());

    // The pristine image still opens.
    std::fs::write(&path, &good).unwrap();
    assert!(reader::open(&path).is_ok());

    std::fs::remove_dir_all(&dir).unwrap();
}
