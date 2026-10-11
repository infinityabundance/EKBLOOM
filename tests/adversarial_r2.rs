//! Independent adversarial pass over the round-3 research modules: `prefix`,
//! `packed`, `tiles` (and its persistence), `handle`, `planner` and `court`.
//!
//! Every test here is an attempt to falsify a claimed invariant. A test that
//! passes means the claim *held*; the one test named
//! `tile_set_from_bytes_rejects_out_of_range_shift` (and its end-to-end sibling
//! `persisted_out_of_range_tile_shift_is_rejected_not_panicked`) was confirmed to
//! fail before the `src/tiles.rs` fix and passes after, so those two are the
//! executable evidence for the single defect found in this pass.
//!
//! Tooling only via `scripts/container-*.sh`.

use std::path::PathBuf;

use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::packed::{state_from_hot, PackedHistory, PackedRestarts};
use ekbloom::prefix::{
    block_summary, prefix_scan, reconstruct_blocks, reconstruct_scalar, state_at_via_prefix,
    Affine, LinMap,
};
use ekbloom::state::Snapshot;
use ekbloom::{
    append_generation, create, grow, manifest, parse_tile_set_prefix, read, reader, verify_deep,
    write_body, AccessPlan, AccessTrace, Candidate, DirectoryKind, EkbloomError,
    ManifestationBudget, ManifestationProfile, ObjectCache, Planner, ReadRequest, TileSet,
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
        "ekbloom-adv-r2-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn tiled_body(src: &[u8], hot: u64, shift: u8, kind: DirectoryKind) -> ekbloom::EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    let regions: Vec<(u64, u64)> = if hot == 0 {
        vec![(0, src.len().max(1) as u64)]
    } else {
        (0..hot.div_ceil(4096)).map(|k| (k * 4096, 4096)).collect()
    };
    let counts: Vec<u64> = (0..regions.len() as u64).map(|k| 10_000 - k).collect();
    TileSet::manifest_hot_tiles(
        &mut body,
        &AccessTrace { regions, counts },
        shift,
        kind,
        &generous(),
    )
    .unwrap();
    body
}

// ---------------------------------------------------------------------------
// Attack 1 — `prefix`: falsify the affine claim on adversarial blocks/maps.
// ---------------------------------------------------------------------------

fn random_map(seed: u64) -> LinMap {
    let mut cols = [0u16; 16];
    let mut s = seed;
    for c in cols.iter_mut() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *c = (s >> 33) as u16;
    }
    LinMap { cols }
}

/// Direct bit-matrix action: XOR the column of every set bit of `x`.
fn ref_apply(cols: &[u16; 16], x: u16) -> u16 {
    let mut y = 0u16;
    let mut k = 0u32;
    while k < 16 {
        if (x >> k) & 1 == 1 {
            y ^= cols[k as usize];
        }
        k += 1;
    }
    y
}

#[test]
fn prefix_claims_hold_on_adversarial_blocks_and_random_maps() {
    // (a) LinMap: compose/power match the direct action, associativity holds on
    // random triples, over the whole 16-bit domain.
    for seed in [1u64, 0xDEAD_BEEF, 0x0123_4567_89AB_CDEF] {
        let m = random_map(seed);
        for x in (0u32..=u16::MAX as u32).step_by(7) {
            assert_eq!(m.apply(x as u16), ref_apply(&m.cols, x as u16));
        }
        // (b o a)(x) == b(a(x)) for random maps.
        let a = random_map(seed ^ 0x1111);
        let b = random_map(seed ^ 0x2222);
        let ba = b.compose(&a);
        let mut x = 0u32;
        while x <= u16::MAX as u32 {
            assert_eq!(ba.apply(x as u16), b.apply(a.apply(x as u16)));
            x += 13;
        }
        // Associativity (a∘b)∘c == a∘(b∘c).
        let c = random_map(seed ^ 0x3333);
        assert_eq!(b.compose(&a).compose(&c), b.compose(&a.compose(&c)));
    }
    // power(n) == n-fold application for random maps / exponents.
    let m = random_map(0xC0FF_EE00_0000_0001);
    for n in [0u64, 1, 2, 3, 4, 7, 16, 31, 64, 255, 1000] {
        let p = m.power(n);
        for x in [0u16, 1, 2, 0x8000, 0x1234, 0xFFFF] {
            let mut y = x;
            for _ in 0..n {
                y = m.apply(y);
            }
            assert_eq!(p.apply(x), y, "power n={n} x={x}");
        }
        // power composes exactly: m^a ∘ m^b == m^(a+b).
        if n > 0 {
            assert_eq!(m.power(n - 1).compose(&m), m.power(n));
        }
    }

    // (c) block_summary/prefix_scan/reconstruct/state_at vs the authoritative
    // machine on degenerate corpora (empty, all-zero, all-0xFF, 1-byte, and
    // lengths not divisible by the block size).
    let corpora: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        vec![0x00; 64],
        vec![0xFF; 64],
        (0u8..=255).collect(),
        lcg(0x0BAD_F00D_CAFE_BEEF, 63),
        lcg(0x1234_5678_9ABC_DEF0, 65),
        lcg(0xAAAA_BBBB_CCCC_DDDD, 257),
    ];
    for core in &corpora {
        let n = core.len();
        let oracle = NL2.decode(core, n as u64).unwrap();
        assert_eq!(reconstruct_scalar(0, core), oracle, "scalar n={n}");
        let logs = state_log(core, n as u64).unwrap();

        for &bs in &[1usize, 2, 3, 5, 7, 64, 65, 256] {
            let summaries: Vec<Affine> = core.chunks(bs).map(block_summary).collect();
            let prefix = prefix_scan(&summaries);
            assert_eq!(prefix.len(), summaries.len() + 1);
            assert_eq!(prefix[0], Affine::identity());

            // Each summary equals `bs` repeated machine steps (A^bs, b).
            for (k, sum) in summaries.iter().enumerate() {
                let lo = k * bs;
                let hi = (lo + bs).min(n);
                let mut omega = Snapshot::new(0, 0);
                for _ in 0..(hi - lo) {
                    NL2.reveal(&core[lo..hi], &mut omega).unwrap();
                }
                assert_eq!(sum.apply(0), omega.state as u16, "summary k={k} bs={bs}");
            }

            // prefix[k] on genesis is the machine state at position min(k*bs, n).
            for (k, p) in prefix.iter().enumerate() {
                let pos = (k * bs).min(n);
                assert_eq!(u32::from(p.apply(0)), logs[pos], "prefix k={k} bs={bs}");
            }

            // state_at_via_prefix matches state_log at every boundary.
            let mut k = 0usize;
            while k * bs <= n {
                let pos = k * bs;
                assert_eq!(
                    u32::from(state_at_via_prefix(&prefix, 0, bs as u64, pos as u64)),
                    logs[pos],
                    "state_at bs={bs} pos={pos}"
                );
                k += 1;
            }

            // reconstruct_blocks at every thread count is byte-exact.
            let starts: Vec<u16> = prefix[..summaries.len()]
                .iter()
                .map(|a| a.apply(0))
                .collect();
            let blocks: Vec<&[u8]> = core.chunks(bs).collect();
            for &threads in &[0usize, 1, 2, 3, 8] {
                let out = reconstruct_blocks(&starts, &blocks, threads).unwrap();
                assert_eq!(
                    out.concat(),
                    oracle,
                    "reconstruct bs={bs} threads={threads}"
                );
            }
        }
    }

    // Out-of-range/empty scans must never panic.
    let _ = state_at_via_prefix(&[], 0x1234, 64, u64::MAX);
    let _ = state_at_via_prefix(&[], 0x1234, 0, 5);
    assert!(reconstruct_blocks(&[0], &[], 4).is_err());
}

// ---------------------------------------------------------------------------
// Attack 2 — `packed`: exactness, crafted bytes, u16 invariant.
// ---------------------------------------------------------------------------

#[test]
fn packed_claims_hold_for_all_positions_and_strides() {
    let sources: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        vec![7u8; 40],
        (0u8..=255).collect(),
        lcg(0x5150_5150_5150_5150, 130),
    ];
    for src in &sources {
        let core = NL2.encode(src).unwrap();
        let n = src.len() as u64;
        let log = state_log(&core, n).unwrap();
        let hot = NL2.decode(&core, n).unwrap();

        let ph = PackedHistory::build(&core, n).unwrap();
        assert_eq!(ph.states.len() as u64, n + 1);
        for (i, &x) in log.iter().enumerate() {
            assert_eq!(ph.state_at(i as u64), Some(x as u16), "history i={i}");
            // The u16 packing assumption is a type invariant, never violated.
            assert!(u32::from(ph.states[i]) < (1 << 16));
            assert_eq!(state_from_hot(&hot, i as u64), Some(x as u16), "hot i={i}");
        }
        assert_eq!(ph.state_at(n + 1), None);

        for &sh in &[0u8, 1, 2, 3, 5, 8, 16, 32, 63] {
            let pr = PackedRestarts::build(&core, n, sh).unwrap();
            for i in 0..=n {
                assert_eq!(
                    pr.snapshot_at(&core, i).unwrap(),
                    Snapshot::new(log[i as usize], i),
                    "restart sh={sh} i={i}"
                );
                let (ckpt, _idx) = pr.position_of(i).unwrap();
                assert_eq!(ckpt, (i >> sh) << sh);
            }
            assert!(pr.snapshot_at(&core, n + 1).is_err());
            assert_eq!(pr.position_of(n + 1), None);
        }

        // `state_from_hot` only succeeds when covered.
        assert_eq!(state_from_hot(&hot, n + 1), None);
    }
    assert_eq!(state_from_hot(&[], 0), Some(0));
    assert_eq!(state_from_hot(&[], 1), None);
}

#[test]
fn packed_parsers_reject_crafted_bytes_cleanly() {
    let core = NL2.encode(&lcg(0x2222_3333_4444_5555, 64)).unwrap();
    let n = 64u64;

    let ph = PackedHistory::build(&core, n).unwrap().to_bytes();
    for cut in 0..ph.len() {
        assert!(
            PackedHistory::from_bytes(&ph[..cut]).is_err(),
            "history truncation accepted at {cut}"
        );
    }
    let mut trailing = ph.clone();
    trailing.push(0);
    assert!(PackedHistory::from_bytes(&trailing).is_err());

    let pr = PackedRestarts::build(&core, n, 3).unwrap().to_bytes();
    for cut in 0..pr.len() {
        assert!(
            PackedRestarts::from_bytes(&pr[..cut]).is_err(),
            "restart truncation accepted at {cut}"
        );
    }
    let mut bad_shift = pr.clone();
    bad_shift[0] = 64;
    assert!(PackedRestarts::from_bytes(&bad_shift).is_err());
    let mut bad_reserved = pr.clone();
    bad_reserved[4] = 1;
    assert!(PackedRestarts::from_bytes(&bad_reserved).is_err());

    // A huge declared length must be a clean error, never an allocation.
    let mut huge = vec![0u8; 16];
    huge[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(PackedHistory::from_bytes(&huge).is_err());
    let mut huge_pr = vec![0u8; 16];
    huge_pr[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(PackedRestarts::from_bytes(&huge_pr).is_err());

    // Arbitrary bytes must never panic, and any accepted parse is self-consistent.
    let rnd = lcg(0xDEAD_BEEF_0BAD_F00D, 400);
    for len in 0..=rnd.len() {
        if let Ok(v) = PackedHistory::from_bytes(&rnd[..len]) {
            let _ = v.state_at(0);
            let _ = v.state_at(u64::MAX);
            assert_eq!(PackedHistory::from_bytes(&v.to_bytes()).unwrap(), v);
        }
        if let Ok(v) = PackedRestarts::from_bytes(&rnd[..len]) {
            let _ = v.position_of(0);
            let _ = v.position_of(u64::MAX);
            let _ = v.snapshot_at(&[], 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 3 — `tiles` + persistence: tamper detection, boundaries, budget.
// ---------------------------------------------------------------------------

/// Regression for the one defect found: `TileSet::from_bytes` did not validate
/// the untrusted `tile_shift`, so `1u64 << tile_shift` panicked (debug) at >= 64
/// and shifts 0 / 31..=63 were silently accepted contrary to the wire contract.
/// This test failed (panic / `Ok`) before the fix and passes after.
#[test]
fn tile_set_from_bytes_rejects_out_of_range_shift() {
    let src = lcg(0x7111_0000_0000_00A1, 64 * 1024);
    let body = tiled_body(&src, 8 * 1024, 12, DirectoryKind::Flat);
    let ts = body.tiles.last().unwrap();

    let pristine = ts.to_bytes();
    assert!(TileSet::from_bytes(&pristine).is_ok());

    // Byte 9 of the prefix is `tile_shift`.
    for bad in [0u8, 31, 32, 63, 64, 200, 255] {
        let mut bytes = pristine.clone();
        bytes[9] = bad;
        assert!(
            matches!(
                TileSet::from_bytes(&bytes),
                Err(EkbloomError::BadSection { .. })
            ),
            "tile_shift {bad} must be rejected"
        );
        assert!(parse_tile_set_prefix(&bytes).is_err());
    }
}

/// End-to-end: a committed `.ekb` whose `TILES` section carries an out-of-range
/// `tile_shift` must be a clean `Err` at open, never a panic. The file is
/// produced by the real writer (so the section CRC-32 and footer checksum are
/// valid); only the body's `tile_shift` is corrupted before serialisation.
#[test]
fn persisted_out_of_range_tile_shift_is_rejected_not_panicked() {
    let dir = temp_dir("shift");
    let src = lcg(0x7111_0000_0000_00A2, 32 * 1024);
    let mut body = tiled_body(&src, 8 * 1024, 12, DirectoryKind::Flat);
    body.tiles[0].tile_shift = 64;
    let path = dir.join("bad-shift.ekb");
    write_body(&path, &body).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(reader::open_bytes(&bytes).is_err());
    assert!(reader::open(&path).is_err());
    assert!(ekbloom::FileBody::open(&path).is_err());
    assert!(ekbloom::OpenObject::open(&path).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Inline IEEE CRC-32 (zlib polynomial), so the test can recompute a section's
/// CRC after tampering without depending on a private crate dependency.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[test]
fn tampered_tile_payload_with_recomputed_crc_fails_open_and_verify() {
    let dir = temp_dir("tamper");
    let src = lcg(0x7111_0000_0000_00A3, 64 * 1024);
    let body = tiled_body(&src, 8 * 1024, 12, DirectoryKind::TwoLevelRadix);
    let path = dir.join("t.ekb");
    write_body(&path, &body).unwrap();

    // Pristine opens and deep-verifies.
    let opened = reader::open(&path).unwrap();
    assert!(verify_deep(&opened).unwrap().ok);

    // Tamper the *tile blob* (last byte of the section payload), then recompute
    // both the section CRC-32 and the commit-footer checksum so the file is
    // structurally valid and the deeper content digest is what must catch it.
    let mut bytes = std::fs::read(&path).unwrap();
    let scan = ekbloom::format::scan(&bytes).unwrap();
    let section = scan
        .commits
        .iter()
        .flat_map(|c| c.sections.iter())
        .find(|s| s.kind == ekbloom::format::SectionKind::Tiles)
        .expect("a TILES section");
    let payload =
        section.payload_offset as usize..(section.payload_offset + section.payload_len) as usize;
    let victim = payload.end - 1;
    bytes[victim] ^= 0xFF;
    let new_crc = crc32(&bytes[payload.clone()]);
    let hdr = section.header_offset as usize;
    bytes[hdr + 36..hdr + 40].copy_from_slice(&new_crc.to_le_bytes());

    let commit = scan.commits.last().unwrap();
    let footer = commit.end as usize - ekbloom::format::FOOTER_LEN;
    let mut footer_buf = bytes[footer..footer + ekbloom::format::FOOTER_LEN].to_vec();
    let mut covered = footer_buf[..56].to_vec();
    covered.extend_from_slice(&footer_buf[60..]);
    let fsum = crc32(&covered);
    footer_buf[56..60].copy_from_slice(&fsum.to_le_bytes());
    bytes[footer..footer + ekbloom::format::FOOTER_LEN].copy_from_slice(&footer_buf);

    // The structure still scans, but the content digest rejects it.
    assert!(ekbloom::format::scan(&bytes).is_ok());
    assert!(reader::open_bytes(&bytes).is_err());

    let bad = dir.join("bad.ekb");
    std::fs::write(&bad, &bytes).unwrap();
    assert!(reader::open(&bad).is_err());
    assert!(ekbloom::FileBody::open(&bad).is_err());
    assert!(ekbloom::OpenObject::open(&bad).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tile_reads_are_exact_across_boundaries_and_degenerate_sizes() {
    // Degenerate: an empty tile set and a zero-length/out-of-bounds read.
    let empty = TileSet::default();
    assert_eq!(empty.read(&[], 0, 0).unwrap(), Vec::<u8>::new());
    assert!(empty.read(&[], 0, 1).is_err());
    assert_eq!(empty.tile_bytes_at(0), None);

    let src = lcg(0x7111_0000_0000_00A4, 3000);
    let n = src.len() as u64;
    for &(shift, kind) in &[
        (1u8, DirectoryKind::Flat),
        (2, DirectoryKind::Flat),
        (3, DirectoryKind::TwoLevelRadix),
        (5, DirectoryKind::TwoLevelRadix),
        (8, DirectoryKind::Flat),
        (12, DirectoryKind::TwoLevelRadix),
        (14, DirectoryKind::Flat),
    ] {
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        // Materialise every tile that the object has.
        let regions: Vec<(u64, u64)> = (0..n.div_ceil(1 << shift))
            .map(|t| (t << shift, (n - (t << shift)).min(1 << shift)))
            .collect();
        let counts: Vec<u64> = vec![1u64; regions.len()];
        TileSet::manifest_hot_tiles(
            &mut body,
            &AccessTrace { regions, counts },
            shift,
            kind,
            &generous(),
        )
        .unwrap();
        let ts = body.tiles.last().unwrap();
        assert!(ts.verify_against(&body.core).unwrap());

        let size = 1u64 << shift;
        // Every tile boundary, plus straddling and degenerate reads.
        let mut offs: Vec<u64> = vec![0, 1, n - 1, n];
        let mut b = 0u64;
        while b <= n {
            offs.push(b);
            if b > 0 {
                offs.push(b - 1);
            }
            if b < n {
                offs.push(b + 1);
            }
            b += size;
        }
        for off in offs {
            for len in [0u64, 1, 2, 3, size.saturating_sub(1), size, size + 1] {
                if off + len > n {
                    continue;
                }
                let got = ts.read(&body.core, off, len).unwrap();
                assert_eq!(
                    got,
                    &src[off as usize..(off + len) as usize],
                    "({off},{len})"
                );
                assert_eq!(
                    got,
                    read(
                        &body,
                        ReadRequest {
                            offset: off,
                            length: len
                        }
                    )
                    .unwrap()
                );
            }
        }
    }
}

#[test]
fn tile_budget_cap_is_never_exceeded_and_no_fit_is_inert() {
    let src = lcg(0x7111_0000_0000_00A5, 256 * 1024);
    let trace = AccessTrace {
        regions: (0..64).map(|k| (k * 4096, 4096)).collect(),
        counts: (0..64u64).map(|k| 1000 - k).collect(),
    };
    for &kind in &[DirectoryKind::Flat, DirectoryKind::TwoLevelRadix] {
        for &delta in &[0u64, 64, 4096, 8192, 40_000, u64::MAX] {
            let mut body = create(&src, ManifestationProfile::Root).unwrap();
            let base = body.physical_bytes();
            let snapshot = body.clone();
            let budget = ManifestationBudget {
                maximum_physical_bytes: base.saturating_add(delta),
            };
            let report = TileSet::manifest_hot_tiles(&mut body, &trace, 12, kind, &budget).unwrap();

            assert!(report.after_physical_bytes <= budget.maximum_physical_bytes);
            assert_eq!(body.physical_bytes(), report.after_physical_bytes);
            assert_eq!(
                report.after_physical_bytes,
                base + report.added_physical_bytes
            );
            assert_eq!(report.certified, report.tiles_selected > 0);
            if report.tiles_selected == 0 {
                assert_eq!(report.added_physical_bytes, 0);
                assert_eq!(report.new_generation, report.before_generation);
                assert_eq!(body, snapshot, "a no-fit call must be inert");
            } else {
                assert!(verify_deep(&body).unwrap().ok);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 4 — `handle`: generation invalidation, no torn publication.
// ---------------------------------------------------------------------------

#[test]
fn object_cache_invalidates_on_append_and_reads_stay_untorn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = temp_dir("cache");
    let src = lcg(0x9006, 64 * 1024);
    let n = src.len() as u64;
    let body = tiled_body(&src, 8 * 1024, 12, DirectoryKind::Flat);
    let path = dir.join("p.ekb");
    write_body(&path, &body).unwrap();

    let cache = Arc::new(ObjectCache::new());
    let v1 = cache.open(&path).unwrap();
    assert_eq!(v1.generation(), body.generation);
    assert!(Arc::ptr_eq(&v1, &cache.open(&path).unwrap()));

    // Publish the next committed generation by appending; the cache must not
    // serve the stale one. (`tiled_body` already committed generation 2, so the
    // grew body must land at generation 3.)
    let mut grown = body.clone();
    grow(&mut grown, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    assert_eq!(grown.generation, 3);
    append_generation(&path, &grown).unwrap();

    let v2 = cache.open(&path).unwrap();
    assert_eq!(
        v2.generation(),
        3,
        "appended generation must be re-validated"
    );
    assert!(!Arc::ptr_eq(&v1, &v2));
    // Both snapshots read their own immutable bytes (RCU, never torn).
    for (off, len) in [(0u64, 8u64), (4095, 2), (n - 7, 7)] {
        let want = &src[off as usize..(off + len) as usize];
        assert_eq!(
            v1.read(ReadRequest {
                offset: off,
                length: len
            })
            .unwrap(),
            want
        );
        assert_eq!(
            v2.read(ReadRequest {
                offset: off,
                length: len
            })
            .unwrap(),
            want
        );
    }

    // Concurrent appends while a held snapshot is read repeatedly: never torn.
    let stop = Arc::new(AtomicBool::new(false));
    let wpath = path.clone();
    let wstop = Arc::clone(&stop);
    let wsrc = src.clone();
    let writer = std::thread::spawn(move || {
        let mut bg = tiled_body(&wsrc, 8 * 1024, 12, DirectoryKind::Flat);
        let core = bg.core.clone();
        let mut g = 3u64;
        for i in 0..8u64 {
            if wstop.load(Ordering::Relaxed) {
                break;
            }
            g += 1;
            bg.generation = g;
            bg.checkpoints.push(
                ekbloom::CheckpointSet::build(&core, n, [4096u32, 1024, 256][i as usize % 3])
                    .unwrap(),
            );
            append_generation(&wpath, &bg).unwrap();
        }
    });
    let held = Arc::clone(&v1);
    std::thread::scope(|scope| {
        for t in 0..4u64 {
            let snap = Arc::clone(&held);
            let src = src.as_slice();
            scope.spawn(move || {
                for k in 0..128u64 {
                    let off = (t * 4096 + k * 89) % (n - 8);
                    let len = 1 + (k % 4);
                    let got = snap
                        .read(ReadRequest {
                            offset: off,
                            length: len,
                        })
                        .unwrap();
                    assert_eq!(got, &src[off as usize..(off + len) as usize]);
                }
            });
        }
    });
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Attack 5 — `planner`: byte-exact dispatch, certified-only, no-search.
// ---------------------------------------------------------------------------

fn hotcold_body(source: &[u8]) -> ekbloom::EkbloomBody {
    let n = source.len() as u64;
    let mut body = create(source, ManifestationProfile::Root).unwrap();
    let hot_len = (n / 4).max(4096).min(n);
    TileSet::manifest_hot_tiles(
        &mut body,
        &AccessTrace {
            regions: vec![(0, hot_len)],
            counts: vec![1000],
        },
        12,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )
    .unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    body
}

#[test]
fn planner_read_is_byte_exact_for_adversarial_requests() {
    let source = lcg(0x0C0D_0000_0000_0002, 200 * 1024);
    let n = source.len() as u64;
    let cfg = ekbloom::CourtConfig {
        reps: 1,
        interleave: false,
        seed: 0xAA,
    };
    let bodies = vec![
        create(&source, ManifestationProfile::Root).unwrap(),
        create(&source, ManifestationProfile::Sparse { interval: 64 }).unwrap(),
        create(&source, ManifestationProfile::Full).unwrap(),
        create(&source, ManifestationProfile::Path { depth: 4 }).unwrap(),
        hotcold_body(&source),
    ];

    for body in &bodies {
        let plan = AccessPlan::plan(body, &cfg).unwrap();
        assert!(plan.all_certified(), "planner chose an uncertified path");
        let planner = Planner::new(plan);

        // Structural: `path_for` is exactly the flat index offset >> region_shift.
        let shift = planner.plan().region_bytes().trailing_zeros();
        let regions = planner.plan().regions();
        let mut probe = 0u64;
        while probe < n {
            let idx = (probe >> shift) as usize;
            assert_eq!(planner.plan().path_for(probe), regions[idx].path);
            probe += 7919;
        }

        // Every structure boundary: tile (4096), planner region (65536) and the
        // checkpoint interval (64), plus 0-length, whole-object and huge spans.
        let mut offsets: Vec<u64> = vec![0, 1, 2, n - 2, n - 1, n];
        for b in [64u64, 4096, 65536, 131072] {
            for k in 0..=3u64 {
                let x = k * b;
                if x > n {
                    continue;
                }
                for d in [-1i64, 0, 1] {
                    let p = x as i64 + d;
                    if p >= 0 && p as u64 <= n {
                        offsets.push(p as u64);
                    }
                }
            }
        }
        let lengths = [
            0u64, 1, 2, 3, 7, 63, 64, 65, 4095, 4096, 4097, 65535, 65536, 65537, n,
        ];
        for &off in &offsets {
            for &len in &lengths {
                if !matches!(off.checked_add(len), Some(e) if e <= n) {
                    continue;
                }
                let want = read(
                    body,
                    ReadRequest {
                        offset: off,
                        length: len,
                    },
                )
                .unwrap();
                let got = planner
                    .read(
                        body,
                        ReadRequest {
                            offset: off,
                            length: len,
                        },
                    )
                    .unwrap();
                assert_eq!(
                    got, want,
                    "planner != reader ({off},{len}) {:?}",
                    body.profiles
                );
            }
        }
        // Out-of-bounds parity.
        assert!(planner
            .read(
                body,
                ReadRequest {
                    offset: n,
                    length: 0
                }
            )
            .unwrap()
            .is_empty());
        assert!(planner
            .read(
                body,
                ReadRequest {
                    offset: n,
                    length: 1
                }
            )
            .is_err());
        assert!(planner
            .read(
                body,
                ReadRequest {
                    offset: u64::MAX,
                    length: u64::MAX
                }
            )
            .is_err());
        assert_eq!(
            planner.plan().path_for(n.saturating_sub(1)).as_str(),
            regions.last().unwrap().path.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// Attack 6 — `court`: degenerate configurations must not panic.
// ---------------------------------------------------------------------------

#[test]
fn court_degenerate_configs_do_not_panic() {
    use ekbloom::court::{collect_environment, write_court_json, Workload};

    let source = lcg(0x0C0F_0000_0000_0003, 8192);
    let raw = Candidate::raw_vec("raw", source.clone());
    let tiny = Candidate::raw_vec("tiny", vec![0u8; 4]);
    let body = create(&source, ManifestationProfile::Path { depth: 4 }).unwrap();
    let cand = Candidate::body("body", body);

    // Zero reps, empty candidate lists, and a working set smaller than the
    // request length all return without panicking.
    let mut court = ekbloom::Court::new(ekbloom::CourtConfig {
        reps: 0,
        interleave: true,
        seed: 7,
    });
    let none: [&Candidate; 0] = [];
    court.measure(0, Workload::Random1b, &none);
    court.measure(0, Workload::Sequential, &none);
    court.measure(64 * 1024, Workload::Random1b, &none);
    court.measure(1, Workload::Random4k, &[&tiny]);
    court.measure(4, Workload::Random64k, &[&tiny]);
    court.measure(8192, Workload::Sequential, &[&raw, &cand]);
    court.measure_concurrent(&[], &[&raw]);
    court.measure_concurrent(&source, &none);

    let report = court.report(collect_environment());
    // An empty report still serialises to valid JSON.
    let dir = temp_dir("court");
    let out = dir.join("conservative").join("court.json");
    write_court_json(&out, &report).unwrap();
    assert!(std::fs::read_to_string(&out).unwrap().starts_with('{'));
    std::fs::remove_dir_all(&dir).unwrap();

    // n = 1: the bootstrap CI collapses to the single sample.
    let d = ekbloom::Dist::from_samples(&[42], 1);
    assert_eq!(
        (d.p50, d.ci95_lo, d.ci95_hi, d.min, d.max),
        (42.0, 42.0, 42.0, 42.0, 42.0)
    );
    let e = ekbloom::Dist::from_samples(&[], 1);
    assert_eq!(e.p50, 0.0);
}
