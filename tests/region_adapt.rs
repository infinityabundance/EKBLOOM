//! Region-scoped, independently certified manifestation (`src/region_adapt.rs`)
//! and the byte-budgeted LRU cache baseline (`src/baseline.rs`).
//!
//! These tests pin the contract the reviewer asked for: region-scoped
//! manifestation is **additive** (earlier structures stay byte-identical and
//! still verify), the [`ManifestationBudget`] hard cap is respected, traced and
//! cold reads are **byte-exact** against the authoritative reader, the LRU
//! baseline returns byte-exact reads, and both are **deterministic** for a fixed
//! seed.

use std::path::{Path, PathBuf};

use ekbloom::machine::MACHINE_RANS0;
use ekbloom::manifest::create_machine;
use ekbloom::reader::open_bytes;
use ekbloom::{
    create, manifest_hot_regions, read, read_region_scoped, verify_deep, write_body, AccessTrace,
    Baseline, BaselineKind, Certification, EkbloomBody, EkbloomError, ManifestationBudget,
    ManifestationProfile, ReadRequest, SweepConfig,
};

/// Deterministic LCG corpus (identical generator to the other harnesses).
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

fn small_cfg() -> SweepConfig {
    SweepConfig {
        iterations: 4,
        seed: 0x0E61_0000_0000_0001,
    }
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = Path::new("target/region-adapt-test").join(name);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A trace of `hot_regions` small hot regions starting at 0, counts decreasing
/// with region index (so selection order is deterministic and non-uniform).
fn hot_trace(region_len: u64, hot_regions: u64) -> AccessTrace {
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    for k in 0..hot_regions {
        regions.push((k * region_len, region_len));
        counts.push(1000u64.saturating_sub(k));
    }
    AccessTrace { regions, counts }
}

fn generous_budget() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

#[test]
fn hot_region_reads_are_byte_exact_against_the_authoritative_reader() {
    let src = lcg(0x0E61_0000_0000_0010, 256 * 1024);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let trace = hot_trace(64, 64);
    let report = manifest_hot_regions(&mut body, &trace, &generous_budget(), &small_cfg()).unwrap();

    // The adaptation is additive and deep-verifiable.
    assert_eq!(report.new_generation, body.generation);
    assert!(report.added_physical_bytes > 0);
    assert_eq!(body.physical_bytes(), report.after_physical_bytes);
    assert!(
        verify_deep(&body).unwrap().ok,
        "region-adapted body must pass deep verification"
    );

    // Every traced region reads back byte-exactly, through the region-scoped path.
    for &(start, len) in &trace.regions {
        let got = read_region_scoped(&body, start, len).unwrap();
        assert_eq!(&got, &src[start as usize..(start + len) as usize]);
        // Single bytes at the region boundary and interior.
        for off in [start, start + 1, start + len - 1] {
            let got = read_region_scoped(&body, off, 1).unwrap();
            assert_eq!(got, vec![src[off as usize]]);
            assert_eq!(
                got,
                read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: 1
                    }
                )
                .unwrap()
            );
        }
    }

    // At least one region certified (small regions collide rarely on random data).
    assert!(
        report.certified_regions > 0,
        "no region certified: {report:?}"
    );
}

#[test]
fn additive_and_earlier_structures_remain_byte_identical() {
    let src = lcg(0x0E61_0000_0000_0020, 128 * 1024);
    let mut body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();

    let before = body.clone();
    let blooms_before = body.blooms.clone();
    let certs_before = body.certificates.clone();
    let cps_before = body.checkpoints.clone();
    let profiles_before = body.profiles.clone();

    let report = manifest_hot_regions(
        &mut body,
        &hot_trace(64, 32),
        &generous_budget(),
        &small_cfg(),
    )
    .unwrap();

    // Non-withdrawal: earlier blooms/certificates/checkpoints are a strict prefix.
    assert!(body.blooms.starts_with(&blooms_before));
    assert!(body.certificates.starts_with(&certs_before));
    assert_eq!(
        body.checkpoints, cps_before,
        "existing checkpoints must not change"
    );
    assert!(body.profiles.starts_with(&profiles_before));
    assert_eq!(body.generation, before.generation + 1);

    // Earlier structures still verify on their own, and the whole body verifies.
    assert!(verify_deep(&before).unwrap().ok);
    assert!(verify_deep(&body).unwrap().ok);

    // Region tables really were appended (some region certified).
    assert!(body.blooms.len() > blooms_before.len());
    assert!(report.selected_regions > 0);
}

#[test]
fn budget_is_respected_and_an_oversize_change_is_rejected() {
    let src = lcg(0x0E61_0000_0000_0030, 128 * 1024);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();

    // A budget with room for the cold checkpoint set plus several tables: the
    // committed body must fit it, and at least one region must be materialised.
    let budget = ManifestationBudget {
        maximum_physical_bytes: body.physical_bytes() + 64 * 1024,
    };
    let report =
        manifest_hot_regions(&mut body, &hot_trace(64, 64), &budget, &small_cfg()).unwrap();
    assert!(report.after_physical_bytes <= budget.maximum_physical_bytes);
    assert_eq!(body.physical_bytes(), report.after_physical_bytes);
    assert!(report.selected_regions > 0);

    // A budget with no room even for the cold checkpoint set is rejected
    // transactionally: the body is unchanged.
    let mut tight = create(&src, ManifestationProfile::Root).unwrap();
    let snapshot = tight.clone();
    let too_tight = ManifestationBudget {
        maximum_physical_bytes: tight.physical_bytes() - 1,
    };
    assert!(matches!(
        manifest_hot_regions(&mut tight, &hot_trace(64, 8), &too_tight, &small_cfg()),
        Err(EkbloomError::AllocationTooLarge { .. })
    ));
    assert_eq!(tight, snapshot, "a rejected call must not touch the body");
}

#[test]
fn cold_region_reads_are_byte_exact_via_checkpoint_restart() {
    let src = lcg(0x0E61_0000_0000_0040, 256 * 1024);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    // Trace only the very front; the rest of the object is cold.
    let trace = AccessTrace {
        regions: vec![(0, 64), (64, 64)],
        counts: vec![1000, 900],
    };
    manifest_hot_regions(&mut body, &trace, &generous_budget(), &small_cfg()).unwrap();
    assert!(
        !body.checkpoints.is_empty(),
        "cold restart needs a checkpoint set"
    );

    // Cold reads far from the traced regions, single-byte and ranges.
    let n = src.len() as u64;
    for (off, len) in [(n / 2, 4096u64), (n - 128, 128), (n - 1, 1), (200_000, 77)] {
        let got = read_region_scoped(&body, off, len).unwrap();
        assert_eq!(
            &got,
            &src[off as usize..(off + len) as usize],
            "cold ({off},{len})"
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

#[test]
fn lru_cache_baseline_is_byte_exact_and_evicts_within_budget() {
    let src = lcg(0x0E61_0000_0000_0050, 256 * 1024);
    let dir = tmp_dir("lru");
    let capacity = 16 * 1024u64;
    let b = Baseline::build_lru(&src, capacity, &dir).unwrap();
    assert_eq!(b.kind(), BaselineKind::LruCache);
    assert_eq!(b.capacity_bytes(), capacity);

    // A full sequential pass must be byte-exact even though it thrashes the cache.
    assert_eq!(b.read_sequential().unwrap(), src);
    assert!(
        b.resident_bytes() <= capacity.max(4096),
        "resident {} > budget",
        b.resident_bytes()
    );
    assert!(b.misses() > 0, "a thrashing sequential pass must miss");

    // Random single-byte reads must be byte-exact.
    let offsets: Vec<u64> = (0..512).map(|i| (i * 977) % src.len() as u64).collect();
    let mut got = Vec::new();
    b.read_random_1b(&offsets, &mut got).unwrap();
    let want: Vec<u8> = offsets.iter().map(|&o| src[o as usize]).collect();
    assert_eq!(got, want);

    // A "reopen" clears residency but the reads stay byte-exact.
    b.clear_cache();
    assert_eq!(b.resident_bytes(), 0);
    assert_eq!(
        b.read(ReadRequest {
            offset: 0,
            length: 4096
        })
        .unwrap(),
        &src[..4096]
    );

    // Bounds semantics match the reader.
    assert!(matches!(
        b.read(ReadRequest {
            offset: src.len() as u64,
            length: 1
        }),
        Err(EkbloomError::OutOfBounds { .. })
    ));
}

#[test]
fn region_adapt_and_lru_are_deterministic_for_a_fixed_seed() {
    let src = lcg(0x0E61_0000_0000_0060, 128 * 1024);
    let trace = hot_trace(64, 48);

    let mut a = create(&src, ManifestationProfile::Root).unwrap();
    let mut b = create(&src, ManifestationProfile::Root).unwrap();
    let ra = manifest_hot_regions(&mut a, &trace, &generous_budget(), &small_cfg()).unwrap();
    let rb = manifest_hot_regions(&mut b, &trace, &generous_budget(), &small_cfg()).unwrap();

    assert_eq!(a, b, "identical inputs must yield identical bodies");
    assert_eq!(ra.new_generation, rb.new_generation);
    assert_eq!(ra.added_physical_bytes, rb.added_physical_bytes);
    assert_eq!(ra.selected_regions, rb.selected_regions);
    assert_eq!(ra.regions, rb.regions);

    // LRU determinism: same capacity + same access sequence => same residency.
    let dir = tmp_dir("lru-determinism");
    let l1 = Baseline::build_lru(&src, 32 * 1024, &dir).unwrap();
    let l2 = Baseline::build_lru(&src, 32 * 1024, &dir).unwrap();
    let seq: Vec<u64> = (0..2000).map(|i| (i * 4099) % src.len() as u64).collect();
    for l in [&l1, &l2] {
        let mut out = Vec::new();
        l.read_random_1b(&seq, &mut out).unwrap();
    }
    assert_eq!(l1.hits(), l2.hits());
    assert_eq!(l1.misses(), l2.misses());
    assert_eq!(l1.resident_bytes(), l2.resident_bytes());
}

#[test]
fn certified_verdicts_match_the_deep_verifier() {
    // A body built only of certified region tables must be accepted by the deep
    // verifier, which recomputes each region's certification independently.
    let src = lcg(0x0E61_0000_0000_0070, 64 * 1024);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let report = manifest_hot_regions(
        &mut body,
        &hot_trace(64, 32),
        &generous_budget(),
        &small_cfg(),
    )
    .unwrap();
    for entry in &report.regions {
        if entry.selected {
            assert_eq!(entry.status, Certification::Certified);
        }
    }
    assert!(verify_deep(&body).unwrap().ok);
}

/// A trace that tiles `[0, n)` into `region_len`-byte regions with decreasing
/// counts, so many contiguous certified tables exist and there are boundaries
/// between them.
fn tiled_trace(n: u64, region_len: u64) -> AccessTrace {
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    let mut start = 0u64;
    let mut count = 1000u64;
    while start < n {
        let len = region_len.min(n - start);
        regions.push((start, len));
        counts.push(count.max(1));
        count = count.saturating_sub(1);
        start += len;
    }
    AccessTrace { regions, counts }
}

/// Read every traced region whole, and read a span straddling each interior
/// boundary (so a leap inside one table is followed by a leap inside the next),
/// comparing **every** returned byte with the source and with the authoritative
/// reader.
fn check_region_reads(body: &EkbloomBody, src: &[u8], trace: &AccessTrace) {
    let n = src.len() as u64;
    for &(start, len) in &trace.regions {
        let whole = read_region_scoped(body, start, len).unwrap();
        assert_eq!(&whole, &src[start as usize..(start + len) as usize]);
        let end = start + len;
        // A span covering the last full window of this region and the first of
        // the next: it exercises a table leap, then a second leap after a boundary.
        if end + 4 <= n {
            let off = end - 4;
            let span = 8u64;
            let got = read_region_scoped(body, off, span).unwrap();
            assert_eq!(
                got,
                &src[off as usize..(off + span) as usize],
                "boundary read ({off}, {span})"
            );
            assert_eq!(
                got,
                read(
                    body,
                    ReadRequest {
                        offset: off,
                        length: span
                    }
                )
                .unwrap()
            );
        }
    }
}

/// Machine 2 (`OrANS-RANS0`) is cursor-divergent: its successor state alone does
/// not resume. Certified region tables therefore carry the complete successor
/// snapshot, and reads must stay byte-exact across table boundaries — including
/// after file serialization and reopening.
#[test]
fn machine_two_region_tables_are_exact_across_boundaries_and_after_reopen() {
    let src = lcg(0x0E61_0000_0000_00B2, 4096);
    let n = src.len() as u64;
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    assert_eq!(body.machine_id, MACHINE_RANS0);
    let trace = tiled_trace(n, 32);
    let report = manifest_hot_regions(&mut body, &trace, &generous_budget(), &small_cfg()).unwrap();
    assert!(
        report.selected_regions > 0,
        "no certified machine-2 region was materialised: {report:?}"
    );
    // The cursor-carrying tables must be accepted for a cursor-divergent body,
    // i.e. the leap path is actually exercised (not silently rejected).
    assert!(
        ekbloom::RegionReader::new(&body).has_regions(),
        "machine-2 region tables were rejected by the reader"
    );
    assert!(
        verify_deep(&body).unwrap().ok,
        "machine-2 body must deep-verify"
    );
    check_region_reads(&body, &src, &trace);

    // Serialize + reopen: region tables (with cursors) and the cold checkpoint set
    // round-trip, and reads stay byte-exact.
    let dir = tmp_dir("machine2-region");
    let path = dir.join("m2.ekb");
    write_body(&path, &body).unwrap();
    let reopened = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(reopened.machine_id, MACHINE_RANS0);
    assert!(
        verify_deep(&reopened).unwrap().ok,
        "reopened body must deep-verify"
    );
    check_region_reads(&reopened, &src, &trace);
}

/// The machine-1 regression: the same boundary/reopen checks must hold where the
/// cursor equals the position (no per-record cursor bytes are stored).
#[test]
fn machine_one_region_tables_are_exact_across_boundaries_and_after_reopen() {
    let src = lcg(0x0E61_0000_0000_00B1, 4096);
    let n = src.len() as u64;
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let trace = tiled_trace(n, 32);
    let report = manifest_hot_regions(&mut body, &trace, &generous_budget(), &small_cfg()).unwrap();
    assert!(
        report.selected_regions > 0,
        "no certified machine-1 region was materialised: {report:?}"
    );
    assert!(ekbloom::RegionReader::new(&body).has_regions());
    assert!(
        verify_deep(&body).unwrap().ok,
        "machine-1 body must deep-verify"
    );
    check_region_reads(&body, &src, &trace);

    let dir = tmp_dir("machine1-region");
    let path = dir.join("m1.ekb");
    write_body(&path, &body).unwrap();
    let reopened = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert!(
        verify_deep(&reopened).unwrap().ok,
        "reopened body must deep-verify"
    );
    check_region_reads(&reopened, &src, &trace);
}
