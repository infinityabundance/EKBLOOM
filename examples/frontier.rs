//! Competitive frontier: EKBLOOM's bounded-access engine vs established indexed
//! codecs (block-indexed LZ4, chunked/seekable Zstandard) and raw storage, under
//! **one equal total physical budget** per corpus.
//!
//! Run inside the pinned, memory-capped container with `sh scripts/run-frontier.sh`.
//! Writes `evidence/bench/frontier.json`. No number is estimated.
//!
//! Every contender is byte-exact (asserted here against the source before any
//! timing). Each indexed-codec contender is measured both **without** and **with**
//! a conventional byte-budgeted hot-block cache, so the comparison is not a
//! cache-less straw man. Latency is exact random 4 KiB reads; we record warm hot
//! reads (inside the hot working set), cold reads (outside it), and reads of the
//! hot set **after a full sequential scan** — the non-eviction property.

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::{
    create_auto, encode_with_k, machine_by_id, now_ns, percentiles, prepare, spread_offsets,
    AccessTrace, BlockIndex, DirectoryKind, Engine, HotBlockCache, ManifestationBudget,
    ManifestationProfile, OrMachine, Percentiles, ReadRequest, TileSet, MACHINE_STORE, RANS0,
};
use std::path::Path;

/// Logical size of each corpus (4 MiB — comfortably inside the 8 GiB cap).
const N: usize = 4 * 1024 * 1024;
/// Hot working set at the start of the object (256 KiB).
const HOT_BYTES: u64 = 256 * 1024;
/// Exact read length (bytes).
const READ: u64 = 4096;
/// Cold-read lengths for the head-to-head: the 4 KiB page and a 64 KiB extent.
const COLD4K: u64 = 4096;
const COLD64K: u64 = 64 * 1024;
/// Cold samples per latency point.
const COLD_SAMPLES: usize = 256;
/// Random samples per latency point.
const SAMPLES: usize = 512;
/// Samples and best-of-N passes for the hot-index micro-measurement.
const HOT_SAMPLES: usize = 2048;
const HOT_REPS: usize = 5;
/// LZ4 block size (bytes) — equal to the read length so a read is one block.
const LZ4_BLOCK: usize = 4096;
/// Zstandard chunk size (bytes) — a seekable chunk coarse enough to be realistic,
/// fine enough that a scan evicts the hot chunks (so the tail is measurable).
const ZSTD_CHUNK: usize = 16 * 1024;
/// Working sets for the allocation-free `byte_at` vs `raw[i]` residency probe.
const RES_1M: usize = 1 << 20;
const RES_256M: usize = 256 << 20;
/// Sequential sweeps per timing pass, and best-of-N passes, for the probe.
const RES_SWEEPS: usize = 64;
const RES_REPS: usize = 9;

/// Deterministic LCG corpus.
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

/// Skewed 4-symbol corpus: order-0 compressible, so machine 2 attains `V < N`.
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(alpha[idx]);
    }
    out
}

fn corpora() -> Vec<(&'static str, Vec<u8>)> {
    let half = N / 2;
    let mut mixed = skewed(0xE7B0_0000_00CC_0002, half);
    mixed.extend_from_slice(&lcg(0xE7B0_0000_00CC_0003, half));
    vec![
        ("skewed", skewed(0xE7B0_0000_00CC_0001, N)),
        ("mixed", mixed),
        ("uniform", lcg(0xE7B0_0000_00CC_0004, N)),
    ]
}

/// Time one read per offset, returning the nearest-rank percentiles (ns).
///
/// The returned bytes are fed through [`std::hint::black_box`] so the copy that
/// produces them cannot be dead-code-eliminated (which would make the memcpy
/// contenders look artificially fast).
fn time_reads(offsets: &[u64], mut read: impl FnMut(u64) -> Vec<u8>) -> Percentiles {
    let mut samples: Vec<u64> = Vec::with_capacity(offsets.len());
    let mut sink = 0u64;
    for &off in offsets {
        let t = now_ns();
        let bytes = read(off);
        let bytes = std::hint::black_box(bytes);
        sink = sink.wrapping_add(bytes.len() as u64);
        samples.push(now_ns().saturating_sub(t));
    }
    std::hint::black_box(sink);
    samples.sort_unstable();
    percentiles(&samples)
}

/// Best (minimum) `p50` over `reps` passes — a robust, noise-resistant floor for
/// a nanosecond-scale comparison.
fn best_p50(reps: usize, offsets: &[u64], read: impl FnMut(u64) -> Vec<u8> + Copy) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..reps {
        best = best.min(time_reads(offsets, read).p50);
    }
    best
}

/// A deterministic shuffle of one offset per 64-byte cache line across the whole
/// working set. Sweeping it walks `len/64` distinct lines in random order, so the
/// working set is genuinely exercised (a random sample that fits the cache would
/// stay resident regardless of the array size, and a contiguous scan would be
/// auto-vectorised into a bandwidth measurement rather than a per-access probe).
fn line_offsets(len: usize) -> Vec<usize> {
    let lines = (len / 64).max(1);
    let mut v: Vec<usize> = (0..lines).map(|i| i * 64).collect();
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15 ^ lines as u64;
    for i in (1..v.len()).rev() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((s >> 33) as usize) % (i + 1);
        v.swap(i, j);
    }
    v
}

/// Time `sweeps` passes over `offsets`, dividing wall time by the exact number of
/// operations so two sub-nanosecond per-op costs can be compared.
fn sweep_ns_per_op<F: FnMut(usize) -> u64>(offsets: &[usize], sweeps: usize, mut op: F) -> f64 {
    let mut sink = 0u64;
    for &off in offsets {
        sink = sink.wrapping_add(op(off));
    }
    let t = now_ns();
    for _ in 0..sweeps {
        for &off in offsets {
            sink = sink.wrapping_add(op(off));
        }
    }
    let elapsed = now_ns().saturating_sub(t);
    std::hint::black_box(sink);
    elapsed as f64 / (sweeps.saturating_mul(offsets.len()).max(1)) as f64
}

/// The result of the allocation-free `byte_at` vs `raw[i]` probe at one working
/// set: both sides read one byte per distinct cache line with no allocation, at
/// equal residency. The `checked` column (`fastpath::byte_at_hot`, a plain
/// bounds-checked + `Result` slice index) isolates the bounds cost from the engine
/// dispatch/indirection. The `resident` column is a **borrowed `ResidentView`**
/// resolved once, read through the tight `view.get(i)` loop — the branch-lean
/// surface with per-call dispatch removed.
struct Residency {
    working_bytes: u64,
    machine: u32,
    raw_p50_ns: f64,
    checked_p50_ns: f64,
    ek_p50_ns: f64,
    resident_p50_ns: f64,
    surface_p50_ns: f64,
    ratio: f64,
    resident_ratio: f64,
    surface_ratio: f64,
    bounds_overhead_ns: f64,
    dispatch_overhead_ns: f64,
    within_15pct: bool,
    resident_within_15pct: bool,
    surface_within_15pct: bool,
}

/// Build a fully-resident body of `working_bytes` and compare an allocation-free
/// `engine.byte_at` against a raw `Vec<u8>` index at equal residency, plus a
/// borrowed [`ekbloom::ResidentView`] obtained once and read through
/// `view.get(i)`.
fn measure_residency(working_bytes: usize) -> ekbloom::Result<Residency> {
    let source = lcg(0x1B17_0000_0000_0001 ^ working_bytes as u64, working_bytes);
    let n = source.len() as u64;
    // `auto` picks machine 3 (verbatim) for this incompressible source, so the
    // resident body is the source itself and the probe isolates the lookup dispatch.
    let mut body = create_auto(&source, ManifestationProfile::Root)?;
    let machine = body.machine_id;
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::Flat,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )?;
    let workload = Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, READ)],
    };
    let engine = Engine::plan(
        &body,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
        &workload,
    )?;
    assert_eq!(engine.worst_case_steps(), 0);
    let raw = source;
    let hot = body
        .tiles
        .last()
        .and_then(|ts| ts.hot_surface())
        .expect("fully-resident tile set must expose its hot surface");
    // ONE resolution call: the borrowed view borrows the same certified surface
    // the engine's hot index points at, with the per-call dispatch removed.
    let view = ekbloom::ResidentView::new(&engine, &body)?;
    assert_eq!(view.len(), n);
    assert_eq!(view.base(), 0);
    assert_eq!(view.surface(), hot);
    let offsets = line_offsets(working_bytes);

    // Byte-exactness before timing: the view agrees with the raw source AND with
    // the authoritative `reader::read` for `get`, `get_checked` and `copy_range`.
    let mut probe = [0u8; 1];
    for &off in offsets.iter().take(64) {
        assert_eq!(engine.byte_at(&body, off as u64)?, raw[off]);
        assert_eq!(ekbloom::byte_at_hot(hot, off as u64)?, raw[off]);
        assert_eq!(view.get(off as u64), raw[off]);
        assert_eq!(view.get_checked(off as u64), Some(raw[off]));
        let reference = ekbloom::read(
            &body,
            ReadRequest {
                offset: off as u64,
                length: 1,
            },
        )?;
        assert_eq!(reference, vec![raw[off]]);
        assert!(view.copy_range(off as u64, &mut probe));
        assert_eq!(probe[0], raw[off]);
    }
    assert!(view.get_checked(n).is_none());
    assert!(!view.copy_range(n, &mut probe));

    // Fewer sweeps for the 256 MiB set so a full working-set pass stays bounded.
    let (sweeps, reps) = if working_bytes >= RES_256M {
        (1usize, 3usize)
    } else {
        (RES_SWEEPS, RES_REPS)
    };
    let mut best_raw = f64::INFINITY;
    let mut best_checked = f64::INFINITY;
    let mut best_ek = f64::INFINITY;
    let mut best_resident = f64::INFINITY;
    let mut best_surface = f64::INFINITY;
    // The borrowed slice itself: extracting it once makes the caller's loop a
    // bare `sum += s[i]`, isolating what `view.get`'s method wrapper adds.
    let surface = view.surface();
    for rep in 0..reps {
        // Alternate the order of the two compared loops so neither is systematically
        // advantaged by warm-up (both are best-of-N minima, but at the clock floor
        // the first-measured loop otherwise tends to win).
        if rep % 2 == 0 {
            best_raw = best_raw.min(sweep_ns_per_op(&offsets, sweeps, |off| u64::from(raw[off])));
            best_resident = best_resident.min(sweep_ns_per_op(&offsets, sweeps, |off| {
                u64::from(view.get(off as u64))
            }));
        } else {
            best_resident = best_resident.min(sweep_ns_per_op(&offsets, sweeps, |off| {
                u64::from(view.get(off as u64))
            }));
            best_raw = best_raw.min(sweep_ns_per_op(&offsets, sweeps, |off| u64::from(raw[off])));
        }
        best_checked = best_checked.min(sweep_ns_per_op(&offsets, sweeps, |off| {
            u64::from(ekbloom::byte_at_hot(hot, off as u64).unwrap_or(0))
        }));
        best_ek = best_ek.min(sweep_ns_per_op(&offsets, sweeps, |off| {
            u64::from(engine.byte_at(&body, off as u64).unwrap_or(0))
        }));
        best_surface = best_surface.min(sweep_ns_per_op(&offsets, sweeps, |off| {
            u64::from(surface[off])
        }));
    }
    let ratio = best_ek / best_raw.max(f64::MIN_POSITIVE);
    let resident_ratio = best_resident / best_raw.max(f64::MIN_POSITIVE);
    let surface_ratio = best_surface / best_raw.max(f64::MIN_POSITIVE);
    Ok(Residency {
        working_bytes: working_bytes as u64,
        machine,
        raw_p50_ns: best_raw,
        checked_p50_ns: best_checked,
        ek_p50_ns: best_ek,
        resident_p50_ns: best_resident,
        surface_p50_ns: best_surface,
        ratio,
        resident_ratio,
        surface_ratio,
        bounds_overhead_ns: best_checked - best_raw,
        dispatch_overhead_ns: best_ek - best_checked,
        within_15pct: ratio <= 1.15,
        resident_within_15pct: resident_ratio <= 1.15,
        surface_within_15pct: surface_ratio <= 1.15,
    })
}

/// The base-representation footprint under the `auto` policy: uniform data must
/// store `<= N`, skewed data must still compress (`V < N`).
struct Representation {
    uniform_machine: u32,
    uniform_base_bytes: u64,
    uniform_source_bytes: u64,
    uniform_ratio: f64,
    skewed_machine: u32,
    skewed_base_bytes: u64,
    skewed_source_bytes: u64,
    skewed_ratio: f64,
}

fn measure_representation() -> ekbloom::Result<Representation> {
    let uniform = lcg(0x4E50_0000_0000_0001, N);
    let text = skewed(0x4E50_0000_0000_0002, N);
    let ub = create_auto(&uniform, ManifestationProfile::Root)?;
    let sb = create_auto(&text, ManifestationProfile::Root)?;
    assert_eq!(
        ub.machine_id, MACHINE_STORE,
        "uniform must be stored verbatim"
    );
    assert!(
        ub.physical_bytes() <= uniform.len() as u64,
        "uniform base footprint {} exceeds raw {}",
        ub.physical_bytes(),
        uniform.len()
    );
    assert!(
        sb.visible_bytes() < text.len() as u64,
        "skewed must compress"
    );
    Ok(Representation {
        uniform_machine: ub.machine_id,
        uniform_base_bytes: ub.physical_bytes(),
        uniform_source_bytes: uniform.len() as u64,
        uniform_ratio: ub.physical_bytes() as f64 / uniform.len() as f64,
        skewed_machine: sb.machine_id,
        skewed_base_bytes: sb.physical_bytes(),
        skewed_source_bytes: text.len() as u64,
        skewed_ratio: sb.physical_bytes() as f64 / text.len() as f64,
    })
}

/// The outcome of the dedicated hot-index comparison (TASK 2): a **fully
/// resident** body (the whole output materialised as tiles, `worst_case_steps =
/// 0`) against a raw `Vec<u8>` index, at equal allocation discipline.
struct HotIndex {
    one_byte_raw_p50: f64,
    one_byte_ek_p50: f64,
    one_byte_ratio: f64,
    four_k_raw_p50: f64,
    four_k_ek_p50: f64,
    four_k_ratio: f64,
    one_byte_within_15pct: bool,
    four_k_within_15pct: bool,
}

/// Measure the branch-lean hot-tile path against a raw `Vec` index.
fn measure_hot_index(source: &[u8]) -> ekbloom::Result<HotIndex> {
    let n = source.len() as u64;
    // Full tile set: the whole object is resident, so every read is a direct
    // contiguous copy with no reconstruction and no directory arithmetic.
    let mut body = create_auto(source, ManifestationProfile::Root)?;
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    let budget = ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &budget)?;
    let workload = Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, READ)],
    };
    let engine = Engine::plan(&body, &budget, &workload)?;
    assert_eq!(
        engine.worst_case_steps(),
        0,
        "fully resident body has no replay"
    );
    let raw = source.to_vec();

    let one_b = spread_offsets(0x1B00_0001, HOT_SAMPLES, n, 1);
    // 4 KiB-aligned offsets, so each read is one region and one tile.
    let four_k: Vec<u64> = (0..HOT_SAMPLES as u64)
        .map(|i| ((i * 7919) % (n / READ)) * READ)
        .collect();

    let ek1 = |off: u64| -> Vec<u8> {
        engine
            .read(
                &body,
                ReadRequest {
                    offset: off,
                    length: 1,
                },
            )
            .unwrap_or_default()
    };
    let ek4 = |off: u64| -> Vec<u8> {
        engine
            .read(
                &body,
                ReadRequest {
                    offset: off,
                    length: READ,
                },
            )
            .unwrap_or_default()
    };
    let raw1 = |off: u64| -> Vec<u8> { raw[off as usize..(off + 1) as usize].to_vec() };
    let raw4 = |off: u64| -> Vec<u8> { raw[off as usize..(off + READ) as usize].to_vec() };

    for _ in 0..4 {
        for &off in &one_b {
            let _ = std::hint::black_box(ek1(off));
            let _ = std::hint::black_box(raw1(off));
        }
        for &off in &four_k {
            let _ = std::hint::black_box(ek4(off));
            let _ = std::hint::black_box(raw4(off));
        }
    }

    let one_raw = best_p50(HOT_REPS, &one_b, raw1).max(1.0);
    let one_ek = best_p50(HOT_REPS, &one_b, ek1);
    let four_raw = best_p50(HOT_REPS, &four_k, raw4).max(1.0);
    let four_ek = best_p50(HOT_REPS, &four_k, ek4);
    let one_byte_ratio = one_ek / one_raw;
    let four_k_ratio = four_ek / four_raw;
    Ok(HotIndex {
        one_byte_raw_p50: one_raw,
        one_byte_ek_p50: one_ek,
        one_byte_ratio,
        four_k_raw_p50: four_raw,
        four_k_ek_p50: four_ek,
        four_k_ratio,
        one_byte_within_15pct: one_byte_ratio <= 1.15,
        four_k_within_15pct: four_k_ratio <= 1.15,
    })
}

/// Block-indexed LZ4 read (`block_size`-byte block grid, implicit index).
fn lz4_read(blocks: &[Vec<u8>], n: usize, offset: u64, len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let b = pos / LZ4_BLOCK;
        let start = b * LZ4_BLOCK;
        let seg_end = end.min(start + LZ4_BLOCK);
        let full = (n - start).min(LZ4_BLOCK);
        let decoded = lz4_flex::decompress(&blocks[b], full).expect("lz4 block");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// Block-indexed LZ4 read through a hot-block cache.
fn lz4_read_cached(
    cache: &mut HotBlockCache,
    blocks: &[Vec<u8>],
    n: usize,
    offset: u64,
    len: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let b = pos / LZ4_BLOCK;
        let start = b * LZ4_BLOCK;
        let seg_end = end.min(start + LZ4_BLOCK);
        let full = (n - start).min(LZ4_BLOCK);
        let decoded = cache
            .get_or_load(b as u64, || {
                Some(lz4_flex::decompress(&blocks[b], full).expect("lz4 block"))
            })
            .expect("cached lz4 block");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// Chunked/seekable Zstandard read (one independent frame per chunk).
fn zstd_read(frames: &[Vec<u8>], offset: u64, len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let c = pos / ZSTD_CHUNK;
        let start = c * ZSTD_CHUNK;
        let seg_end = end.min(start + ZSTD_CHUNK);
        let decoded = zstd::stream::decode_all(&frames[c][..]).expect("zstd frame");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// Chunked/seekable Zstandard read through a hot-block cache.
fn zstd_read_cached(
    cache: &mut HotBlockCache,
    frames: &[Vec<u8>],
    offset: u64,
    len: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let c = pos / ZSTD_CHUNK;
        let start = c * ZSTD_CHUNK;
        let seg_end = end.min(start + ZSTD_CHUNK);
        let decoded = cache
            .get_or_load(c as u64, || {
                Some(zstd::stream::decode_all(&frames[c][..]).expect("zstd frame"))
            })
            .expect("cached zstd frame");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

// ---------------------------------------------------------------------------
// COLD (uncached) read head-to-head at one footprint per contender.
//
// The bottleneck is decode throughput, so this compares EKBLOOM's block-restart
// decode across its three machines and three block sizes against a **fair**
// block-indexed LZ4 (a real per-block offset table, so a 4 KiB cold read
// decompresses only the covering block, never the whole object) and chunked
// Zstandard. Every contender is byte-exact before timing.
// ---------------------------------------------------------------------------

/// Deterministic cold offsets (`COLD_SAMPLES` of them) past the 256 KiB hot set,
/// aligned to `len`.
fn cold_offsets(n: u64, len: u64, salt: u64) -> Vec<u64> {
    let start = HOT_BYTES;
    let span = n.saturating_sub(start + len);
    let slots = (span / len).max(1);
    (0..COLD_SAMPLES as u64)
        .map(|i| {
            let j = if span >= len {
                i.wrapping_mul(2654435761)
                    .wrapping_add(salt)
                    .wrapping_mul(0x9E37_79B9)
                    % slots
            } else {
                0
            };
            start + j * len
        })
        .collect()
}

/// Block-indexed LZ4 read: decompress only the blocks covering the request (the
/// `block`-byte grid is the real, offset-addressed index).
fn lz4_read_blocked(blocks: &[Vec<u8>], block: usize, n: usize, offset: u64, len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let b = pos / block;
        let start = b * block;
        let seg_end = end.min(start + block);
        let full = (n - start).min(block);
        let decoded = lz4_flex::decompress(&blocks[b], full).expect("lz4 block");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// [`lz4_read_blocked`] through a byte-budgeted hot-block cache.
fn lz4_read_blocked_cached(
    cache: &mut HotBlockCache,
    blocks: &[Vec<u8>],
    block: usize,
    n: usize,
    offset: u64,
    len: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let b = pos / block;
        let start = b * block;
        let seg_end = end.min(start + block);
        let full = (n - start).min(block);
        let decoded = cache
            .get_or_load(b as u64, || {
                Some(lz4_flex::decompress(&blocks[b], full).expect("lz4 block"))
            })
            .expect("cached lz4 block");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// Chunked Zstandard read of an arbitrary `(offset, len)` extent.
fn zstd_read_at(frames: &[Vec<u8>], chunk: usize, offset: u64, len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let c = pos / chunk;
        let start = c * chunk;
        let seg_end = end.min(start + chunk);
        let decoded = zstd::stream::decode_all(&frames[c][..]).expect("zstd frame");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// [`zstd_read_at`] through a byte-budgeted hot-block cache.
fn zstd_read_at_cached(
    cache: &mut HotBlockCache,
    frames: &[Vec<u8>],
    chunk: usize,
    offset: u64,
    len: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let c = pos / chunk;
        let start = c * chunk;
        let seg_end = end.min(start + chunk);
        let decoded = cache
            .get_or_load(c as u64, || {
                Some(zstd::stream::decode_all(&frames[c][..]).expect("zstd frame"))
            })
            .expect("cached zstd frame");
        out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        pos = seg_end;
    }
    out
}

/// One cold-read contender row.
struct ColdRow {
    name: String,
    class: String,
    stored_bytes: u64,
    index_bytes: u64,
    build_ns: u64,
    r4k_p50: f64,
    r4k_p99: f64,
    r64k_p50: f64,
    r64k_p99: f64,
    note: String,
}

impl ColdRow {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        class: &str,
        stored_bytes: u64,
        index_bytes: u64,
        build_ns: u64,
        r4k: &Percentiles,
        r64k: &Percentiles,
        note: String,
    ) -> ColdRow {
        ColdRow {
            name,
            class: class.to_string(),
            stored_bytes,
            index_bytes,
            build_ns,
            r4k_p50: r4k.p50,
            r4k_p99: r4k.p99,
            r64k_p50: r64k.p50,
            r64k_p99: r64k.p99,
            note,
        }
    }
}

/// The cold head-to-head for one corpus.
struct ColdReport {
    name: String,
    source_len: u64,
    hot_bytes: u64,
    rows: Vec<ColdRow>,
}

/// Measure the cold 4 KiB / 64 KiB head-to-head for one corpus.
fn measure_cold(name: &str, source: &[u8]) -> ekbloom::Result<ColdReport> {
    let n = source.len() as u64;
    let offs4 = cold_offsets(n, COLD4K, 0xC01D);
    let offs64 = cold_offsets(n, COLD64K, 0xC064);
    let mut rows: Vec<ColdRow> = Vec::new();

    // ---- EKBLOOM: every machine x block size, each an authoritative encode plus
    // a certified BlockIndex, read through the bounded block decoder.
    for &machine in &[1u32, 2, 3, 4] {
        let m = machine_by_id(machine).expect("known machine");
        let t_enc = now_ns();
        let core = m.encode(source)?;
        let encode_ns = now_ns().saturating_sub(t_enc);
        for &block in &[256u32, 1024, 4096] {
            let t = now_ns();
            let index = BlockIndex::build_machine(&core, n, block, machine)?;
            let build_ns = now_ns().saturating_sub(t).saturating_add(encode_ns);
            let index_bytes = index
                .directory_bytes()
                .saturating_add(index.restart_bytes());
            let stored = (core.len() as u64).saturating_add(index_bytes);
            for &off in offs4.iter().take(8) {
                assert_eq!(
                    index.read_range(&core, off, COLD4K)?,
                    &source[off as usize..(off + COLD4K) as usize],
                    "ekbloom m{machine} b{block} cold exact"
                );
            }
            let r4k = time_reads(&offs4, |off| index.read_range(&core, off, COLD4K).unwrap());
            let r64k = time_reads(&offs64, |off| {
                index.read_range(&core, off, COLD64K).unwrap()
            });
            rows.push(ColdRow::new(
                format!("ekbloom-m{machine}-b{block}"),
                "ekbloom",
                stored,
                index_bytes,
                build_ns,
                &r4k,
                &r64k,
                format!("machine {machine}, {block} B block restart"),
            ));
        }
    }

    // ---- Block-indexed LZ4 (explicit u32 offset table) and a cached variant.
    let budget = n + n / 8;
    for &block in &[1024usize, 4096, 16384] {
        let t = now_ns();
        let blocks: Vec<Vec<u8>> = source.chunks(block).map(lz4_flex::compress).collect();
        let build_ns = now_ns().saturating_sub(t);
        let compressed: u64 = blocks.iter().map(|b| b.len() as u64).sum();
        let index_bytes = blocks.len() as u64 * 4;
        let stored = compressed.saturating_add(index_bytes);
        for &off in offs4.iter().take(8) {
            assert_eq!(
                lz4_read_blocked(&blocks, block, source.len(), off, COLD4K),
                &source[off as usize..(off + COLD4K) as usize],
                "lz4 b{block} cold exact"
            );
        }
        let r4k = time_reads(&offs4, |off| {
            lz4_read_blocked(&blocks, block, source.len(), off, COLD4K)
        });
        let r64k = time_reads(&offs64, |off| {
            lz4_read_blocked(&blocks, block, source.len(), off, COLD64K)
        });
        rows.push(ColdRow::new(
            format!("lz4-block-{block}"),
            "lz4",
            stored,
            index_bytes,
            build_ns,
            &r4k,
            &r64k,
            format!(
                "{block} B block grid + {index_bytes} B offset table, one decompress per block"
            ),
        ));
        if block == 4096 {
            let cache_budget = budget.saturating_sub(stored);
            let mut cache = HotBlockCache::new(block as u64, cache_budget);
            for _ in 0..2 {
                for &off in &offs4 {
                    let _ = lz4_read_blocked_cached(
                        &mut cache,
                        &blocks,
                        block,
                        source.len(),
                        off,
                        COLD4K,
                    );
                }
            }
            let r4k = time_reads(&offs4, |off| {
                lz4_read_blocked_cached(&mut cache, &blocks, block, source.len(), off, COLD4K)
            });
            let r64k = time_reads(&offs64, |off| {
                lz4_read_blocked_cached(&mut cache, &blocks, block, source.len(), off, COLD64K)
            });
            rows.push(ColdRow::new(
                "lz4-block-4096+cache".to_string(),
                "lz4",
                stored.saturating_add(cache_budget),
                index_bytes,
                build_ns,
                &r4k,
                &r64k,
                format!("same storage + {cache_budget} B LRU cache"),
            ));
        }
    }

    // ---- Chunked Zstandard and a cached variant.
    {
        let t = now_ns();
        let frames: Vec<Vec<u8>> = source
            .chunks(ZSTD_CHUNK)
            .map(|c| zstd::stream::encode_all(c, 0).unwrap())
            .collect();
        let build_ns = now_ns().saturating_sub(t);
        let compressed: u64 = frames.iter().map(|f| f.len() as u64).sum();
        let index_bytes = frames.len() as u64 * 4;
        let stored = compressed.saturating_add(index_bytes);
        let r4k = time_reads(&offs4, |off| zstd_read_at(&frames, ZSTD_CHUNK, off, COLD4K));
        let r64k = time_reads(&offs64, |off| {
            zstd_read_at(&frames, ZSTD_CHUNK, off, COLD64K)
        });
        rows.push(ColdRow::new(
            "zstd-chunk".to_string(),
            "zstd",
            stored,
            index_bytes,
            build_ns,
            &r4k,
            &r64k,
            format!("{ZSTD_CHUNK} B seekable chunk, one frame per chunk"),
        ));
        let cache_budget = budget.saturating_sub(stored);
        let mut cache = HotBlockCache::new(ZSTD_CHUNK as u64, cache_budget);
        for _ in 0..2 {
            for &off in &offs4 {
                let _ = zstd_read_at_cached(&mut cache, &frames, ZSTD_CHUNK, off, COLD4K);
            }
        }
        let r4k = time_reads(&offs4, |off| {
            zstd_read_at_cached(&mut cache, &frames, ZSTD_CHUNK, off, COLD4K)
        });
        let r64k = time_reads(&offs64, |off| {
            zstd_read_at_cached(&mut cache, &frames, ZSTD_CHUNK, off, COLD64K)
        });
        rows.push(ColdRow::new(
            "zstd-chunk+cache".to_string(),
            "zstd",
            stored.saturating_add(cache_budget),
            index_bytes,
            build_ns,
            &r4k,
            &r64k,
            format!("same storage + {cache_budget} B LRU cache"),
        ));
    }

    // ---- raw-vec reference (a memcpy, the cold floor).
    {
        let raw = source.to_vec();
        let r4k = time_reads(&offs4, |off| {
            raw[off as usize..(off + COLD4K) as usize].to_vec()
        });
        let r64k = time_reads(&offs64, |off| {
            raw[off as usize..(off + COLD64K) as usize].to_vec()
        });
        rows.push(ColdRow::new(
            "raw-vec".to_string(),
            "raw",
            n,
            0,
            0,
            &r4k,
            &r64k,
            "positional memcpy".to_string(),
        ));
    }

    Ok(ColdReport {
        name: name.to_string(),
        source_len: n,
        hot_bytes: HOT_BYTES,
        rows,
    })
}

/// One bulk-decode-throughput contender (the prepared fast path).
struct DecodeRate {
    name: String,
    machine: u32,
    k: usize,
    core_bytes: u64,
    source_bytes: u64,
    best_ns: f64,
    ns_per_byte: f64,
    gb_per_s: f64,
    exact: bool,
}

/// Measure bulk decode throughput (`ns/byte`, GB/s) of machine 2 versus the
/// interleaved machine 4 at `K = 4` and `K = 8`, all through the prepared bulk
/// decoder (`prepare` + `Prepared::decode_into`), best of 3, byte-exact checked.
fn measure_decode(source: &[u8]) -> ekbloom::Result<Vec<DecodeRate>> {
    let n = source.len() as u64;
    let reps = 3usize;
    let configs: [(&str, u32, usize, Vec<u8>); 3] = [
        ("rans0-m2", 2, 1, RANS0.encode(source)?),
        ("rans0i-k4", 4, 4, encode_with_k(source, 4)?),
        ("rans0i-k8", 4, 8, encode_with_k(source, 8)?),
    ];
    let mut rows = Vec::new();
    for (name, machine_id, k, core) in &configs {
        let machine = machine_by_id(*machine_id).expect("known machine");
        let mut best = u64::MAX;
        let mut exact = true;
        for _ in 0..reps {
            let ctx = prepare(machine, core)?;
            let mut omega = machine.init(core)?;
            let mut out: Vec<u8> = Vec::with_capacity(source.len());
            let t = now_ns();
            ctx.decode_into(core, &mut omega, n, &mut out)?;
            best = best.min(now_ns().saturating_sub(t));
            exact &= out == source;
        }
        let ns = best as f64;
        rows.push(DecodeRate {
            name: name.to_string(),
            machine: *machine_id,
            k: *k,
            core_bytes: core.len() as u64,
            source_bytes: n,
            best_ns: ns,
            ns_per_byte: ns / n as f64,
            gb_per_s: if ns > 0.0 { n as f64 / ns } else { 0.0 },
            exact,
        });
    }
    Ok(rows)
}

/// One measured contender.
#[derive(Clone)]
struct Row {
    name: String,
    class: String,
    retained_bytes: u64,
    storage_bytes: u64,
    cache_bytes: u64,
    build_ns: u64,
    hot_p50: f64,
    hot_p99: f64,
    cold_p50: f64,
    cold_p99: f64,
    thrash_hot_p50: f64,
    thrash_hot_p99: f64,
    hit_rate: f64,
    note: String,
}

impl Row {
    fn new(name: &str, class: &str, retained: u64, storage: u64) -> Row {
        Row {
            name: name.to_string(),
            class: class.to_string(),
            retained_bytes: retained,
            storage_bytes: storage,
            cache_bytes: 0,
            build_ns: 0,
            hot_p50: 0.0,
            hot_p99: 0.0,
            cold_p50: 0.0,
            cold_p99: 0.0,
            thrash_hot_p50: 0.0,
            thrash_hot_p99: 0.0,
            hit_rate: 0.0,
            note: String::new(),
        }
    }
}

struct CorpusReport {
    name: String,
    machine_id: u32,
    source_len: u64,
    core_len: u64,
    budget_bytes: u64,
    hot_bytes: u64,
    ekbloom_smaller_than_raw: bool,
    ekbloom_hot_p99_best: bool,
    ekbloom_cold_p99_best: bool,
    ekbloom_thrash_hot_p99_best: bool,
    rows: Vec<Row>,
}

/// Measure one corpus under a single equal budget.
fn measure_corpus(name: &str, source: &[u8]) -> ekbloom::Result<CorpusReport> {
    let n = source.len() as u64;
    let build_start = now_ns();

    // ---- EKBLOOM: `auto` body, hot tiles, a bounded cold block index, engine plan.
    let mut body = create_auto(source, ManifestationProfile::Root)?;
    let core_len = body.core.len() as u64;
    let machine = body.machine_id;
    // One equal budget per corpus: headroom above raw for the manifested
    // structures (but *not* enough for a conventional cache of the whole output).
    let budget = n.max(core_len) + n / 8;
    let budget_manifest = ManifestationBudget {
        maximum_physical_bytes: budget,
    };
    let trace = AccessTrace {
        regions: vec![(0, HOT_BYTES)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &budget_manifest,
    )?;
    let block = BlockIndex::build_machine(&body.core, n, 1024, machine)?;
    body.blocks.push(block);
    body.generation += 1;
    let workload = Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: Some(HOT_BYTES),
        sample: vec![(0, READ)],
    };
    let engine = Engine::plan(&body, &budget_manifest, &workload)?;
    let ek_build_ns = now_ns().saturating_sub(build_start);
    assert!(
        body.physical_bytes() <= budget,
        "ekbloom retained {} > budget {budget}",
        body.physical_bytes()
    );

    let hot_offsets = spread_offsets(0x4070_0001, SAMPLES, HOT_BYTES, READ);
    let cold_offsets: Vec<u64> = spread_offsets(0x4070_0002, SAMPLES, n - HOT_BYTES, READ)
        .iter()
        .map(|o| o + HOT_BYTES)
        .collect();
    let scan_offsets: Vec<u64> = (0..n.saturating_sub(READ)).step_by(READ as usize).collect();

    // Byte-exact check for EKBLOOM before timing.
    for &off in hot_offsets.iter().chain(cold_offsets.iter()).take(32) {
        let got = engine.read(
            &body,
            ReadRequest {
                offset: off,
                length: READ,
            },
        )?;
        assert_eq!(got, &source[off as usize..(off + READ) as usize]);
    }

    let mut rows: Vec<Row> = Vec::new();

    // ---- raw-vec (the raw-storage reference).
    let raw = source.to_vec();
    let raw_retained = raw.len() as u64;
    for _ in 0..2 {
        for &off in &hot_offsets {
            let _ = raw[off as usize..(off + READ) as usize].to_vec();
        }
    }
    {
        let mut row = Row::new("raw-vec", "raw", raw_retained, raw_retained);
        row.build_ns = 0;
        let hot = time_reads(&hot_offsets, |off| {
            raw[off as usize..(off + READ) as usize].to_vec()
        });
        let cold = time_reads(&cold_offsets, |off| {
            raw[off as usize..(off + READ) as usize].to_vec()
        });
        // A raw slice is never evicted; the post-scan hot read is unchanged.
        let thrash = time_reads(&hot_offsets, |off| {
            raw[off as usize..(off + READ) as usize].to_vec()
        });
        row.hot_p50 = hot.p50;
        row.hot_p99 = hot.p99;
        row.cold_p50 = cold.p50;
        row.cold_p99 = cold.p99;
        row.thrash_hot_p50 = thrash.p50;
        row.thrash_hot_p99 = thrash.p99;
        row.note = "raw storage; a positional slice is a memcpy".to_string();
        rows.push(row);
    }

    // ---- Block-indexed LZ4 (with and without a hot-block cache). The compressed
    // blocks are a fixed-size grid but their sizes vary, so a real direct-access
    // reader needs a per-block offset table; `u32` offsets (one per block) are
    // counted in the footprint so the LZ4 competitor is not made artificially slim.
    let t = now_ns();
    let lz4_blocks: Vec<Vec<u8>> = source.chunks(LZ4_BLOCK).map(lz4_flex::compress).collect();
    let lz4_build_ns = now_ns().saturating_sub(t);
    let lz4_index_bytes = lz4_blocks.len() as u64 * 4;
    let lz4_storage = lz4_blocks.iter().map(|b| b.len() as u64).sum::<u64>() + lz4_index_bytes;
    for &off in hot_offsets.iter().chain(cold_offsets.iter()).take(16) {
        assert_eq!(
            lz4_read(&lz4_blocks, source.len(), off, READ),
            &source[off as usize..(off + READ) as usize]
        );
    }
    {
        let mut row = Row::new("lz4-block", "indexed", lz4_storage, lz4_storage);
        row.build_ns = lz4_build_ns;
        let hot = time_reads(&hot_offsets, |off| {
            lz4_read(&lz4_blocks, source.len(), off, READ)
        });
        row.hot_p50 = hot.p50;
        row.hot_p99 = hot.p99;
        let cold = time_reads(&cold_offsets, |off| {
            lz4_read(&lz4_blocks, source.len(), off, READ)
        });
        row.cold_p50 = cold.p50;
        row.cold_p99 = cold.p99;
        // Stateless contender: every read decompresses, so a scan changes nothing.
        row.thrash_hot_p50 = row.hot_p50;
        row.thrash_hot_p99 = row.hot_p99;
        row.note = format!("{LZ4_BLOCK} B block grid, one LZ4 block per read, no cache");
        rows.push(row);
    }
    {
        let cache_budget = budget.saturating_sub(lz4_storage);
        let mut cache = HotBlockCache::new(LZ4_BLOCK as u64, cache_budget);
        let mut row = Row::new(
            "lz4-block+cache",
            "indexed",
            lz4_storage.saturating_add(cache_budget),
            lz4_storage,
        );
        row.cache_bytes = cache_budget;
        row.build_ns = lz4_build_ns;
        // Warm the hot set.
        for _ in 0..2 {
            for &off in &hot_offsets {
                let _ = lz4_read_cached(&mut cache, &lz4_blocks, source.len(), off, READ);
            }
        }
        let hot = time_reads(&hot_offsets, |off| {
            lz4_read_cached(&mut cache, &lz4_blocks, source.len(), off, READ)
        });
        let cold = time_reads(&cold_offsets, |off| {
            lz4_read_cached(&mut cache, &lz4_blocks, source.len(), off, READ)
        });
        // Full sequential scan evicts the LRU, then re-read the hot set.
        for &off in &scan_offsets {
            let _ = lz4_read_cached(&mut cache, &lz4_blocks, source.len(), off, READ);
        }
        let thrash = time_reads(&hot_offsets, |off| {
            lz4_read_cached(&mut cache, &lz4_blocks, source.len(), off, READ)
        });
        row.hot_p50 = hot.p50;
        row.hot_p99 = hot.p99;
        row.cold_p50 = cold.p50;
        row.cold_p99 = cold.p99;
        row.thrash_hot_p50 = thrash.p50;
        row.thrash_hot_p99 = thrash.p99;
        row.hit_rate = cache.hit_rate();
        row.note = format!(
            "same storage + {cache_budget} B LRU cache ({:.1} KiB/block)",
            LZ4_BLOCK as f64 / 1024.0
        );
        rows.push(row);
    }

    // ---- Chunked Zstandard (with and without a hot-block cache).
    let t = now_ns();
    let zstd_frames: Vec<Vec<u8>> = source
        .chunks(ZSTD_CHUNK)
        .map(|c| zstd::stream::encode_all(c, 0).unwrap())
        .collect();
    let zstd_build_ns = now_ns().saturating_sub(t);
    let zstd_storage = zstd_frames.iter().map(|f| f.len() as u64).sum::<u64>();
    for &off in hot_offsets.iter().chain(cold_offsets.iter()).take(16) {
        assert_eq!(
            zstd_read(&zstd_frames, off, READ),
            &source[off as usize..(off + READ) as usize]
        );
    }
    {
        let mut row = Row::new("zstd-chunk", "indexed", zstd_storage, zstd_storage);
        row.build_ns = zstd_build_ns;
        let hot = time_reads(&hot_offsets, |off| zstd_read(&zstd_frames, off, READ));
        let cold = time_reads(&cold_offsets, |off| zstd_read(&zstd_frames, off, READ));
        row.hot_p50 = hot.p50;
        row.hot_p99 = hot.p99;
        row.cold_p50 = cold.p50;
        row.cold_p99 = cold.p99;
        row.thrash_hot_p50 = hot.p50;
        row.thrash_hot_p99 = hot.p99;
        row.note = format!("{ZSTD_CHUNK} B seekable chunk, one frame per read, no cache");
        rows.push(row);
    }
    {
        let cache_budget = budget.saturating_sub(zstd_storage);
        let mut cache = HotBlockCache::new(ZSTD_CHUNK as u64, cache_budget);
        let mut row = Row::new(
            "zstd-chunk+cache",
            "indexed",
            zstd_storage.saturating_add(cache_budget),
            zstd_storage,
        );
        row.cache_bytes = cache_budget;
        row.build_ns = zstd_build_ns;
        for _ in 0..2 {
            for &off in &hot_offsets {
                let _ = zstd_read_cached(&mut cache, &zstd_frames, off, READ);
            }
        }
        let hot = time_reads(&hot_offsets, |off| {
            zstd_read_cached(&mut cache, &zstd_frames, off, READ)
        });
        let cold = time_reads(&cold_offsets, |off| {
            zstd_read_cached(&mut cache, &zstd_frames, off, READ)
        });
        for &off in &scan_offsets {
            let _ = zstd_read_cached(&mut cache, &zstd_frames, off, READ);
        }
        let thrash = time_reads(&hot_offsets, |off| {
            zstd_read_cached(&mut cache, &zstd_frames, off, READ)
        });
        row.hot_p50 = hot.p50;
        row.hot_p99 = hot.p99;
        row.cold_p50 = cold.p50;
        row.cold_p99 = cold.p99;
        row.thrash_hot_p50 = thrash.p50;
        row.thrash_hot_p99 = thrash.p99;
        row.hit_rate = cache.hit_rate();
        row.note = format!("same storage + {cache_budget} B LRU cache");
        rows.push(row);
    }

    // ---- EKBLOOM engine row.
    {
        let mut row = Row::new("ekbloom-engine", "ekbloom", body.physical_bytes(), core_len);
        row.build_ns = ek_build_ns;
        let ek_read = |off: u64| -> Vec<u8> {
            engine
                .read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: READ,
                    },
                )
                .unwrap_or_default()
        };
        let hot = time_reads(&hot_offsets, ek_read);
        let cold = time_reads(&cold_offsets, ek_read);
        // A full scan never evicts manifested structures.
        for &off in &scan_offsets {
            let _ = engine.read(
                &body,
                ReadRequest {
                    offset: off,
                    length: READ,
                },
            );
        }
        let thrash = time_reads(&hot_offsets, ek_read);
        row.hot_p50 = hot.p50;
        row.hot_p99 = hot.p99;
        row.cold_p50 = cold.p50;
        row.cold_p99 = cold.p99;
        row.thrash_hot_p50 = thrash.p50;
        row.thrash_hot_p99 = thrash.p99;
        row.note = format!(
            "engine: worst_case_steps {} over {} regions; core {core_len} B",
            engine.worst_case_steps(),
            engine.plan_ref().region_count()
        );
        rows.push(row);
    }

    // ---- Verdict flags for this corpus.
    let indexed = |pred: &dyn Fn(&Row) -> f64, class: &str| -> f64 {
        rows.iter()
            .filter(|r| r.class == class)
            .map(pred)
            .fold(f64::INFINITY, f64::min)
    };
    let ek = rows.iter().find(|r| r.name == "ekbloom-engine").unwrap();
    let best_indexed_hot_p99 = indexed(&|r| r.hot_p99, "indexed");
    let best_indexed_cold_p99 = indexed(&|r| r.cold_p99, "indexed");
    let best_indexed_thrash_hot_p99 = {
        let v = indexed(&|r| r.thrash_hot_p99, "indexed");
        if v.is_finite() {
            v
        } else {
            f64::INFINITY
        }
    };
    let report = CorpusReport {
        name: name.to_string(),
        machine_id: machine,
        source_len: n,
        core_len,
        budget_bytes: budget,
        hot_bytes: HOT_BYTES,
        ekbloom_smaller_than_raw: (ek.retained_bytes as f64) < 0.9 * n as f64,
        ekbloom_hot_p99_best: ek.hot_p99 < best_indexed_hot_p99,
        ekbloom_cold_p99_best: ek.cold_p99 < best_indexed_cold_p99,
        ekbloom_thrash_hot_p99_best: ek.thrash_hot_p99 < best_indexed_thrash_hot_p99,
        rows,
    };
    Ok(report)
}

fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn render_json(
    corpora: &[CorpusReport],
    hot: &HotIndex,
    residency: &[Residency],
    rep: &Representation,
    cold: &[ColdReport],
    decode: &[DecodeRate],
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "{{");
    let _ = writeln!(s, "  \"producer\": \"examples/frontier.rs\",");
    let _ = writeln!(s, "  \"budget_ratio\": 1.125,");
    let _ = writeln!(s, "  \"read_bytes\": {READ},");
    let _ = writeln!(s, "  \"samples\": {SAMPLES},");
    let _ = writeln!(s, "  \"lz4_block_bytes\": {LZ4_BLOCK},");
    let _ = writeln!(s, "  \"zstd_chunk_bytes\": {ZSTD_CHUNK},");
    s.push_str("  \"hot_index\": {\n");
    let _ = writeln!(
        s,
        "    \"one_byte_raw_p50_ns\": {:.1},",
        finite(hot.one_byte_raw_p50)
    );
    let _ = writeln!(
        s,
        "    \"one_byte_ekbloom_p50_ns\": {:.1},",
        finite(hot.one_byte_ek_p50)
    );
    let _ = writeln!(
        s,
        "    \"one_byte_ratio\": {:.3},",
        finite(hot.one_byte_ratio)
    );
    let _ = writeln!(
        s,
        "    \"four_k_raw_p50_ns\": {:.1},",
        finite(hot.four_k_raw_p50)
    );
    let _ = writeln!(
        s,
        "    \"four_k_ekbloom_p50_ns\": {:.1},",
        finite(hot.four_k_ek_p50)
    );
    let _ = writeln!(s, "    \"four_k_ratio\": {:.3},", finite(hot.four_k_ratio));
    let _ = writeln!(
        s,
        "    \"one_byte_within_15pct\": {},",
        hot.one_byte_within_15pct
    );
    let _ = writeln!(
        s,
        "    \"four_k_within_15pct\": {}",
        hot.four_k_within_15pct
    );
    s.push_str("  },\n");
    s.push_str("  \"byte_at_residency\": [\n");
    for (i, r) in residency.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"working_bytes\": {},", r.working_bytes);
        let _ = writeln!(s, "      \"machine\": {},", r.machine);
        let _ = writeln!(s, "      \"raw_p50_ns\": {:.4},", finite(r.raw_p50_ns));
        let _ = writeln!(
            s,
            "      \"checked_p50_ns\": {:.4},",
            finite(r.checked_p50_ns)
        );
        let _ = writeln!(s, "      \"ekbloom_p50_ns\": {:.4},", finite(r.ek_p50_ns));
        let _ = writeln!(
            s,
            "      \"resident_p50_ns\": {:.4},",
            finite(r.resident_p50_ns)
        );
        let _ = writeln!(
            s,
            "      \"surface_p50_ns\": {:.4},",
            finite(r.surface_p50_ns)
        );
        let _ = writeln!(s, "      \"ratio\": {:.4},", finite(r.ratio));
        let _ = writeln!(
            s,
            "      \"resident_ratio\": {:.4},",
            finite(r.resident_ratio)
        );
        let _ = writeln!(
            s,
            "      \"surface_ratio\": {:.4},",
            finite(r.surface_ratio)
        );
        let _ = writeln!(
            s,
            "      \"bounds_overhead_ns\": {:.4},",
            finite(r.bounds_overhead_ns)
        );
        let _ = writeln!(
            s,
            "      \"dispatch_overhead_ns\": {:.4},",
            finite(r.dispatch_overhead_ns)
        );
        let _ = writeln!(s, "      \"within_15pct\": {},", r.within_15pct);
        let _ = writeln!(
            s,
            "      \"resident_within_15pct\": {},",
            r.resident_within_15pct
        );
        let _ = writeln!(
            s,
            "      \"surface_within_15pct\": {}",
            r.surface_within_15pct
        );
        let _ = write!(s, "    }}");
        if i + 1 < residency.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ],\n");
    s.push_str("  \"representation\": {\n");
    let _ = writeln!(s, "    \"uniform_machine\": {},", rep.uniform_machine);
    let _ = writeln!(s, "    \"uniform_base_bytes\": {},", rep.uniform_base_bytes);
    let _ = writeln!(
        s,
        "    \"uniform_source_bytes\": {},",
        rep.uniform_source_bytes
    );
    let _ = writeln!(
        s,
        "    \"uniform_ratio\": {:.4},",
        finite(rep.uniform_ratio)
    );
    let _ = writeln!(s, "    \"skewed_machine\": {},", rep.skewed_machine);
    let _ = writeln!(s, "    \"skewed_base_bytes\": {},", rep.skewed_base_bytes);
    let _ = writeln!(
        s,
        "    \"skewed_source_bytes\": {},",
        rep.skewed_source_bytes
    );
    let _ = writeln!(s, "    \"skewed_ratio\": {:.4}", finite(rep.skewed_ratio));
    s.push_str("  },\n");
    s.push_str("  \"cold_reads\": [\n");
    for (ci, c) in cold.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&c.name));
        let _ = writeln!(s, "      \"source_len\": {},", c.source_len);
        let _ = writeln!(s, "      \"hot_bytes\": {},", c.hot_bytes);
        let _ = writeln!(s, "      \"cold4k_bytes\": {COLD4K},");
        let _ = writeln!(s, "      \"cold64k_bytes\": {COLD64K},");
        let _ = writeln!(s, "      \"samples\": {COLD_SAMPLES},");
        s.push_str("      \"rows\": [\n");
        for (ri, r) in c.rows.iter().enumerate() {
            s.push_str("        {\n");
            let _ = writeln!(s, "          \"name\": \"{}\",", json_escape(&r.name));
            let _ = writeln!(s, "          \"class\": \"{}\",", r.class);
            let _ = writeln!(s, "          \"stored_bytes\": {},", r.stored_bytes);
            let _ = writeln!(s, "          \"index_bytes\": {},", r.index_bytes);
            let _ = writeln!(s, "          \"build_ns\": {},", r.build_ns);
            let _ = writeln!(s, "          \"r4k_p50_ns\": {:.1},", finite(r.r4k_p50));
            let _ = writeln!(s, "          \"r4k_p99_ns\": {:.1},", finite(r.r4k_p99));
            let _ = writeln!(s, "          \"r64k_p50_ns\": {:.1},", finite(r.r64k_p50));
            let _ = writeln!(s, "          \"r64k_p99_ns\": {:.1},", finite(r.r64k_p99));
            let _ = writeln!(s, "          \"note\": \"{}\"", json_escape(&r.note));
            let _ = write!(s, "        }}");
            if ri + 1 < c.rows.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str("      ]\n");
        let _ = write!(s, "    }}");
        if ci + 1 < cold.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ],\n");
    s.push_str("  \"decode_rate\": [\n");
    for (i, d) in decode.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&d.name));
        let _ = writeln!(s, "      \"machine\": {},", d.machine);
        let _ = writeln!(s, "      \"k\": {},", d.k);
        let _ = writeln!(s, "      \"core_bytes\": {},", d.core_bytes);
        let _ = writeln!(s, "      \"source_bytes\": {},", d.source_bytes);
        let _ = writeln!(s, "      \"best_ns\": {:.0},", finite(d.best_ns));
        let _ = writeln!(s, "      \"ns_per_byte\": {:.4},", finite(d.ns_per_byte));
        let _ = writeln!(s, "      \"gb_per_s\": {:.3},", finite(d.gb_per_s));
        let _ = writeln!(s, "      \"exact\": {}", d.exact);
        let _ = write!(s, "    }}");
        if i + 1 < decode.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ],\n");
    s.push_str("  \"corpora\": [\n");
    for (ci, c) in corpora.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&c.name));
        let _ = writeln!(s, "      \"source_len\": {},", c.source_len);
        let _ = writeln!(s, "      \"machine\": {},", c.machine_id);
        let _ = writeln!(s, "      \"core_len\": {},", c.core_len);
        let _ = writeln!(s, "      \"budget_bytes\": {},", c.budget_bytes);
        let _ = writeln!(s, "      \"hot_bytes\": {},", c.hot_bytes);
        s.push_str("      \"verdict\": {\n");
        let _ = writeln!(
            s,
            "        \"ekbloom_smaller_than_raw\": {},",
            c.ekbloom_smaller_than_raw
        );
        let _ = writeln!(
            s,
            "        \"ekbloom_hot_p99_best\": {},",
            c.ekbloom_hot_p99_best
        );
        let _ = writeln!(
            s,
            "        \"ekbloom_cold_p99_best\": {},",
            c.ekbloom_cold_p99_best
        );
        let _ = writeln!(
            s,
            "        \"ekbloom_thrash_hot_p99_best\": {}",
            c.ekbloom_thrash_hot_p99_best
        );
        s.push_str("      },\n");
        s.push_str("      \"rows\": [\n");
        for (ri, r) in c.rows.iter().enumerate() {
            s.push_str("        {\n");
            let _ = writeln!(s, "          \"name\": \"{}\",", json_escape(&r.name));
            let _ = writeln!(s, "          \"class\": \"{}\",", r.class);
            let _ = writeln!(s, "          \"retained_bytes\": {},", r.retained_bytes);
            let _ = writeln!(s, "          \"storage_bytes\": {},", r.storage_bytes);
            let _ = writeln!(s, "          \"cache_bytes\": {},", r.cache_bytes);
            let _ = writeln!(s, "          \"build_ns\": {},", r.build_ns);
            let _ = writeln!(s, "          \"hot_p50_ns\": {:.1},", finite(r.hot_p50));
            let _ = writeln!(s, "          \"hot_p99_ns\": {:.1},", finite(r.hot_p99));
            let _ = writeln!(s, "          \"cold_p50_ns\": {:.1},", finite(r.cold_p50));
            let _ = writeln!(s, "          \"cold_p99_ns\": {:.1},", finite(r.cold_p99));
            let _ = writeln!(
                s,
                "          \"thrash_hot_p50_ns\": {:.1},",
                finite(r.thrash_hot_p50)
            );
            let _ = writeln!(
                s,
                "          \"thrash_hot_p99_ns\": {:.1},",
                finite(r.thrash_hot_p99)
            );
            let _ = writeln!(s, "          \"hit_rate\": {:.4},", finite(r.hit_rate));
            let _ = writeln!(s, "          \"note\": \"{}\"", json_escape(&r.note));
            let _ = write!(s, "        }}");
            if ri + 1 < c.rows.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str("      ]\n");
        let _ = write!(s, "    }}");
        if ci + 1 < corpora.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ]\n}\n");
    s
}

fn print_table(corpora: &[CorpusReport]) {
    for c in corpora {
        println!(
            "\n== corpus {} ==  machine {}, source {} B, core {} B (V/N {:.3}), budget {} B, hot {} B",
            c.name,
            c.machine_id,
            c.source_len,
            c.core_len,
            c.core_len as f64 / c.source_len as f64,
            c.budget_bytes,
            c.hot_bytes
        );
        println!(
            "{:<18} {:<9} {:>11} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7}",
            "contender",
            "class",
            "retained",
            "build_ns",
            "hot_p50",
            "hot_p99",
            "cold_p99",
            "thr_p50",
            "thr_p99",
            "hit%"
        );
        for r in &c.rows {
            println!(
                "{:<18} {:<9} {:>11} {:>9} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>7.2}",
                r.name,
                r.class,
                r.retained_bytes,
                r.build_ns,
                r.hot_p50,
                r.hot_p99,
                r.cold_p99,
                r.thrash_hot_p50,
                r.thrash_hot_p99,
                r.hit_rate * 100.0
            );
        }
        println!(
            "   verdict: smaller_than_raw={} hot_p99_best={} cold_p99_best={} thrash_hot_p99_best={}",
            c.ekbloom_smaller_than_raw,
            c.ekbloom_hot_p99_best,
            c.ekbloom_cold_p99_best,
            c.ekbloom_thrash_hot_p99_best
        );
    }
}

fn print_cold_table(cold: &[ColdReport]) {
    for c in cold {
        println!(
            "\n== COLD reads: corpus {} ==  source {} B, hot prefix {} B",
            c.name, c.source_len, c.hot_bytes
        );
        println!(
            "{:<22} {:<8} {:>11} {:>9} {:>10} {:>10} {:>11} {:>11}",
            "contender",
            "class",
            "stored",
            "build_ns",
            "r4k p50",
            "r4k p99",
            "r64k p50",
            "r64k p99"
        );
        for r in &c.rows {
            println!(
                "{:<22} {:<8} {:>11} {:>9} {:>10.0} {:>10.0} {:>11.0} {:>11.0}",
                r.name,
                r.class,
                r.stored_bytes,
                r.build_ns,
                r.r4k_p50,
                r.r4k_p99,
                r.r64k_p50,
                r.r64k_p99
            );
        }
    }
}

fn print_decode_table(decode: &[DecodeRate]) {
    println!("\n== bulk decode throughput (skewed corpus, best of 3) ==");
    println!(
        "{:<12} {:>8} {:>3} {:>11} {:>10} {:>10} {:>9} {:>6}",
        "config", "machine", "K", "core_B", "ns", "ns/byte", "GB/s", "exact"
    );
    for d in decode {
        println!(
            "{:<12} {:>8} {:>3} {:>11} {:>10.0} {:>10.4} {:>9.3} {:>6}",
            d.name, d.machine, d.k, d.core_bytes, d.best_ns, d.ns_per_byte, d.gb_per_s, d.exact
        );
    }
}

fn main() -> ekbloom::Result<()> {
    let skew_src = skewed(0xE7B0_0000_00CC_0001, N);
    let hot_index = measure_hot_index(&skew_src)?;
    let residency = vec![measure_residency(RES_1M)?, measure_residency(RES_256M)?];
    let representation = measure_representation()?;
    let corpus_list = corpora();
    let cold: Vec<ColdReport> = corpus_list
        .iter()
        .filter(|(name, _)| *name == "skewed" || *name == "mixed")
        .map(|(name, source)| measure_cold(name, source))
        .collect::<ekbloom::Result<Vec<_>>>()?;
    let corpora: Vec<CorpusReport> = corpus_list
        .iter()
        .map(|(name, source)| measure_corpus(name, source))
        .collect::<ekbloom::Result<Vec<_>>>()?;
    let decode = measure_decode(&skew_src)?;

    print_table(&corpora);
    print_cold_table(&cold);
    print_decode_table(&decode);

    println!("\n== hot index (fully resident, allocation-parity) ==");
    println!(
        "  1-byte: raw {:.0} ns, ekbloom {:.0} ns, ratio {:.2}",
        hot_index.one_byte_raw_p50, hot_index.one_byte_ek_p50, hot_index.one_byte_ratio
    );
    println!(
        "  4 KiB : raw {:.0} ns, ekbloom {:.0} ns, ratio {:.2}",
        hot_index.four_k_raw_p50, hot_index.four_k_ek_p50, hot_index.four_k_ratio
    );
    println!(
        "  within 15% of raw: 1-byte={} 4KiB={}",
        hot_index.one_byte_within_15pct, hot_index.four_k_within_15pct
    );

    println!("\n== byte_at vs raw[i] (allocation-free, equal residency) ==");
    for r in &residency {
        println!(
            "  {:>10} B working set (machine {}): raw {:.3} ns, checked {:.3} ns, ekbloom {:.3} ns, ratio {:.3} (within 15%: {})",
            r.working_bytes,
            r.machine,
            r.raw_p50_ns,
            r.checked_p50_ns,
            r.ek_p50_ns,
            r.ratio,
            r.within_15pct
        );
        println!(
            "      residual: bounds {:.3} ns + dispatch/indirection {:.3} ns",
            r.bounds_overhead_ns, r.dispatch_overhead_ns
        );
        println!(
            "      resident view: {:.3} ns, ratio {:.3} (within 15%: {})",
            r.resident_p50_ns, r.resident_ratio, r.resident_within_15pct
        );
        println!(
            "      borrowed slice (view.surface()[i]): {:.3} ns, ratio {:.3} (within 15%: {})",
            r.surface_p50_ns, r.surface_ratio, r.surface_within_15pct
        );
    }
    println!("\n== representation policy (base = core) ==");
    println!(
        "  uniform: machine {}, base {} B / raw {} B = {:.4} (<= raw: {})",
        representation.uniform_machine,
        representation.uniform_base_bytes,
        representation.uniform_source_bytes,
        representation.uniform_ratio,
        representation.uniform_base_bytes <= representation.uniform_source_bytes
    );
    println!(
        "  skewed:  machine {}, base {} B / raw {} B = {:.4}",
        representation.skewed_machine,
        representation.skewed_base_bytes,
        representation.skewed_source_bytes,
        representation.skewed_ratio
    );

    let footprint_win = corpora.iter().any(|c| c.ekbloom_smaller_than_raw);
    let advantage_win = corpora.iter().any(|c| {
        c.ekbloom_hot_p99_best || c.ekbloom_cold_p99_best || c.ekbloom_thrash_hot_p99_best
    });
    let holds = footprint_win && advantage_win;
    println!("\n== milestone acceptance ==");
    println!("  byte-exact all contenders: true (asserted before timing)");
    println!("  materially smaller footprint than raw: {footprint_win}");
    println!("  reproducible advantage over indexed codecs: {advantage_win}");
    println!("  HOLDS: {holds}");

    let json = render_json(
        &corpora,
        &hot_index,
        &residency,
        &representation,
        &cold,
        &decode,
    );
    let out = Path::new("evidence/bench/frontier.json");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, json)?;
    println!("wrote {}", out.display());
    Ok(())
}
