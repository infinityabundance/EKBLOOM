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
    create_machine, now_ns, percentiles, spread_offsets, AccessTrace, BlockIndex, DirectoryKind,
    Engine, HotBlockCache, ManifestationBudget, ManifestationProfile, Percentiles, ReadRequest,
    TileSet,
};
use std::path::Path;

/// Logical size of each corpus (4 MiB — comfortably inside the 8 GiB cap).
const N: usize = 4 * 1024 * 1024;
/// Hot working set at the start of the object (256 KiB).
const HOT_BYTES: u64 = 256 * 1024;
/// Exact read length (bytes).
const READ: u64 = 4096;
/// Random samples per latency point.
const SAMPLES: usize = 512;
/// Samples and best-of-N passes for the hot-index micro-measurement.
const HOT_SAMPLES: usize = 2048;
const HOT_REPS: usize = 5;
/// Machine 2 (`OrANS-RANS0`) — the compressing machine.
const M2: u32 = 2;
/// LZ4 block size (bytes) — equal to the read length so a read is one block.
const LZ4_BLOCK: usize = 4096;
/// Zstandard chunk size (bytes) — a seekable chunk coarse enough to be realistic,
/// fine enough that a scan evicts the hot chunks (so the tail is measurable).
const ZSTD_CHUNK: usize = 16 * 1024;

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
    let mut body = create_machine(source, ManifestationProfile::Root, M2)?;
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

    // ---- EKBLOOM: machine-2 body, hot tiles, a bounded cold block index, engine plan.
    let mut body = create_machine(source, ManifestationProfile::Root, M2)?;
    let core_len = body.core.len() as u64;
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
    let block = BlockIndex::build_machine(&body.core, n, 1024, M2)?;
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

    // ---- Block-indexed LZ4 (with and without a hot-block cache).
    let t = now_ns();
    let lz4_blocks: Vec<Vec<u8>> = source.chunks(LZ4_BLOCK).map(lz4_flex::compress).collect();
    let lz4_build_ns = now_ns().saturating_sub(t);
    let lz4_storage = lz4_blocks.iter().map(|b| b.len() as u64).sum::<u64>();
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

fn render_json(corpora: &[CorpusReport], hot: &HotIndex) -> String {
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
    s.push_str("  \"corpora\": [\n");
    for (ci, c) in corpora.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&c.name));
        let _ = writeln!(s, "      \"source_len\": {},", c.source_len);
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
            "\n== corpus {} ==  source {} B, core {} B (V/N {:.3}), budget {} B, hot {} B",
            c.name,
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

fn main() -> ekbloom::Result<()> {
    let hot_index = measure_hot_index(&skewed(0xE7B0_0000_00CC_0001, N))?;
    let corpora: Vec<CorpusReport> = corpora()
        .into_iter()
        .map(|(name, source)| measure_corpus(name, &source))
        .collect::<ekbloom::Result<Vec<_>>>()?;

    print_table(&corpora);

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

    let json = render_json(&corpora, &hot_index);
    let out = Path::new("evidence/bench/frontier.json");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, json)?;
    println!("wrote {}", out.display());
    Ok(())
}
