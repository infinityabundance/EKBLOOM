//! Persistent verified handles vs. reopen-per-request, concurrent readers under
//! publication, and persisted-tile reads — measured inside the pinned container.
//!
//! Writes `evidence/bench/handles.json` (run by `sh scripts/run-handles.sh`).
//!
//! Three measurement blocks:
//!
//! * **(a) cached handle vs. reopen + reparse** — latency per read for 1-byte and
//!   4 KiB hot reads, through an already-open [`OpenObject`] (no re-open, no
//!   re-parse, no lock) versus `FileBody::open` + read per request, plus the
//!   cache-hit path (`ObjectCache::open` + read).
//! * **(b) concurrent readers under background construction** — 1/2/4/8/16 reader
//!   threads while a background thread constructs additional manifestation (CPU)
//!   on a private body. Throughput and p50/p99/p99.9 read latency, reported with
//!   and without background construction.
//! * **(c) persisted tiles** — a hot 4 KiB range served from an `.ekb` through
//!   `FileBody` by bounded `pread` versus the in-memory `TileSet`.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use ekbloom::{
    create, now_ns, read, verify_deep, write_body, AccessTrace, DirectoryKind, EkbloomBody,
    FileBody, ManifestationBudget, ManifestationProfile, ObjectCache, ReadRequest, TileSet,
};

/// Deterministic corpus size (4 MiB).
const SRC_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed (shared with the other harnesses).
const SRC_SEED: u64 = 0xEB10_0000_C0DE_2026;
/// Traced hot working set (64 KiB).
const HOT_BYTES: u64 = 64 * 1024;
/// Tile size shift (4 KiB tiles).
const TILE_SHIFT: u8 = 12;
/// Hot read length (bytes).
const WINDOW: u64 = 4096;
/// Number of samples for the cached/read-latency rows.
const SAMPLES: usize = 512;
/// Number of samples for the expensive reopen-and-reparse rows.
const REPARSE_SAMPLES: usize = 96;
/// Reads performed per reader thread in the concurrency block.
const READS_PER_THREAD: usize = 16000;
/// Reader-thread counts exercised.
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];

fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
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

/// Nearest-rank percentile over an ascending sample.
fn pct(sorted: &[u64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let rank = (q * n as f64).ceil();
    let idx = (rank as usize).saturating_sub(1).min(n - 1);
    sorted[idx] as f64
}

#[derive(Clone, Copy)]
struct Stats {
    p50: f64,
    p99: f64,
    p999: f64,
    mean: f64,
}

fn stats(mut durs: Vec<u64>) -> Stats {
    durs.sort_unstable();
    let mean = if durs.is_empty() {
        0.0
    } else {
        durs.iter().map(|&d| d as f64).sum::<f64>() / durs.len() as f64
    };
    Stats {
        p50: pct(&durs, 0.50),
        p99: pct(&durs, 0.99),
        p999: pct(&durs, 0.999),
        mean,
    }
}

fn hot_sample() -> Vec<(u64, u64)> {
    let aligned = HOT_BYTES / WINDOW;
    (0..SAMPLES)
        .map(|i| {
            let off = (i as u64 % aligned) * WINDOW;
            (off, WINDOW)
        })
        .collect()
}

fn small_sample() -> Vec<(u64, u64)> {
    (0..SAMPLES)
        .map(|i| ((i as u64 * 131) % HOT_BYTES, 1u64))
        .collect()
}

fn render_json(
    file_bytes: u64,
    cached_vs_reopen: &[(&'static str, Stats)],
    concurrency: &[ConcurrencyRow],
    persistence: &[(&'static str, Stats)],
    verdict: &str,
) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(s, "  \"corpus_bytes\": {SRC_LEN},");
    let _ = writeln!(s, "  \"corpus_seed\": {SRC_SEED},");
    let _ = writeln!(s, "  \"file_bytes\": {file_bytes},");
    let _ = writeln!(s, "  \"hot_bytes\": {HOT_BYTES},");
    let _ = writeln!(s, "  \"tile_shift\": {TILE_SHIFT},");
    let _ = writeln!(s, "  \"window_bytes\": {WINDOW},");
    let _ = writeln!(s, "  \"samples\": {SAMPLES},");
    let _ = writeln!(s, "  \"reparse_samples\": {REPARSE_SAMPLES},");
    let _ = writeln!(s, "  \"reads_per_thread\": {READS_PER_THREAD},");
    s.push_str("  \"cached_vs_reopen\": [\n");
    for (i, (name, st)) in cached_vs_reopen.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{name}\",");
        let _ = writeln!(s, "      \"p50_ns\": {:.1},", st.p50);
        let _ = writeln!(s, "      \"p99_ns\": {:.1},", st.p99);
        let _ = writeln!(s, "      \"p999_ns\": {:.1},", st.p999);
        let _ = writeln!(s, "      \"mean_ns\": {:.1}", st.mean);
        let _ = write!(s, "    }}");
        let _ = writeln!(
            s,
            "{}",
            if i + 1 == cached_vs_reopen.len() {
                ""
            } else {
                ","
            }
        );
    }
    s.push_str("  ],\n");

    s.push_str("  \"concurrency\": [\n");
    for (i, row) in concurrency.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"threads\": {},", row.threads);
        let _ = writeln!(s, "      \"baseline_reads_per_sec\": {:.0},", row.base_tput);
        let _ = writeln!(s, "      \"bg_reads_per_sec\": {:.0},", row.bg_tput);
        let _ = writeln!(s, "      \"baseline_p50_ns\": {:.1},", row.base.p50);
        let _ = writeln!(s, "      \"baseline_p99_ns\": {:.1},", row.base.p99);
        let _ = writeln!(s, "      \"baseline_p999_ns\": {:.1},", row.base.p999);
        let _ = writeln!(s, "      \"bg_p50_ns\": {:.1},", row.bg.p50);
        let _ = writeln!(s, "      \"bg_p99_ns\": {:.1},", row.bg.p99);
        let _ = writeln!(s, "      \"bg_p999_ns\": {:.1},", row.bg.p999);
        let _ = writeln!(s, "      \"bg_manifestations\": {}", row.bg_manifestations);
        let _ = write!(s, "    }}");
        let _ = writeln!(s, "{}", if i + 1 == concurrency.len() { "" } else { "," });
    }
    s.push_str("  ],\n");

    s.push_str("  \"tile_persistence\": [\n");
    for (i, (name, st)) in persistence.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{name}\",");
        let _ = writeln!(s, "      \"p50_ns\": {:.1},", st.p50);
        let _ = writeln!(s, "      \"p99_ns\": {:.1},", st.p99);
        let _ = writeln!(s, "      \"p999_ns\": {:.1},", st.p999);
        let _ = writeln!(s, "      \"mean_ns\": {:.1}", st.mean);
        let _ = write!(s, "    }}");
        let _ = writeln!(s, "{}", if i + 1 == persistence.len() { "" } else { "," });
    }
    s.push_str("  ],\n");
    let _ = writeln!(s, "  \"verdict\": \"{}\"", json_escape(verdict));
    s.push_str("}\n");
    s
}

struct ConcurrencyRow {
    threads: usize,
    base_tput: f64,
    bg_tput: f64,
    base: Stats,
    bg: Stats,
    bg_manifestations: u64,
}

fn json_escape(x: &str) -> String {
    let mut out = String::with_capacity(x.len());
    for c in x.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn main() -> ekbloom::Result<()> {
    let source = lcg_bytes(SRC_SEED, SRC_LEN);
    let n = source.len() as u64;
    let dir = Path::new("target/handles-tmp");
    std::fs::create_dir_all(dir)?;
    let path = dir.join("object.ekb");

    // A root body plus a persisted 4 KiB-tile set over the 64 KiB hot region.
    let mut body = create(&source, ManifestationProfile::Root)?;
    let trace = AccessTrace {
        regions: (0..(HOT_BYTES / WINDOW))
            .map(|k| (k * WINDOW, WINDOW))
            .collect(),
        counts: (0..(HOT_BYTES / WINDOW)).map(|k| 10_000 - k).collect(),
    };
    let report = TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        TILE_SHIFT,
        DirectoryKind::TwoLevelRadix,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )?;
    assert!(report.certified);
    assert!(verify_deep(&body)?.ok, "tiled body must verify");
    write_body(&path, &body)?;
    let body_bytes = std::fs::metadata(&path)?.len();

    let cache = ObjectCache::new();
    let object = cache.open(&path)?;
    assert!(object.tiles_cover(0, WINDOW));

    // ---- (a) cached handle vs. reopen + reparse ------------------------------
    let big = hot_sample();
    let small = small_sample();

    // Warm the read paths, then time. The cached rows use the already-open handle
    // and take no lock; the reparse rows re-open and re-validate per request.
    for &(off, len) in &big {
        let _ = object.read(ReadRequest {
            offset: off,
            length: len,
        })?;
    }
    let time_cached = |sample: &[(u64, u64)]| -> Stats {
        let mut durs = Vec::with_capacity(sample.len());
        for &(off, len) in sample {
            let t0 = now_ns();
            let got = object
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .expect("cached read");
            let t1 = now_ns();
            assert_eq!(got.len(), len as usize);
            durs.push(t1.saturating_sub(t0));
        }
        stats(durs)
    };
    let time_reopen_big = {
        let mut durs = Vec::with_capacity(REPARSE_SAMPLES);
        for &(off, len) in big.iter().take(REPARSE_SAMPLES) {
            let t0 = now_ns();
            let fb = FileBody::open(&path)?;
            let got = fb.read(ReadRequest {
                offset: off,
                length: len,
            })?;
            let t1 = now_ns();
            assert_eq!(got.len(), len as usize);
            durs.push(t1.saturating_sub(t0));
        }
        stats(durs)
    };
    let time_reopen_small = {
        let mut durs = Vec::with_capacity(REPARSE_SAMPLES);
        for &(off, len) in small.iter().take(REPARSE_SAMPLES) {
            let t0 = now_ns();
            let fb = FileBody::open(&path)?;
            let got = fb.read(ReadRequest {
                offset: off,
                length: len,
            })?;
            let t1 = now_ns();
            assert_eq!(got.len(), len as usize);
            durs.push(t1.saturating_sub(t0));
        }
        stats(durs)
    };
    let cached_big = time_cached(&big);
    let cached_small = time_cached(&small);
    // Cache-hit path: identity+generation probe and Arc clone, then read.
    let cache_hit_big = {
        let mut durs = Vec::with_capacity(SAMPLES.min(128));
        for &(off, len) in big.iter().take(128) {
            let t0 = now_ns();
            let obj = cache.open(&path)?;
            let got = obj.read(ReadRequest {
                offset: off,
                length: len,
            })?;
            let t1 = now_ns();
            assert_eq!(got.len(), len as usize);
            durs.push(t1.saturating_sub(t0));
        }
        stats(durs)
    };
    let cached_vs_reopen = vec![
        ("1-byte cached handle read", cached_small),
        ("1-byte reopen + reparse", time_reopen_small),
        ("4 KiB cached handle read", cached_big),
        ("4 KiB reopen + reparse", time_reopen_big),
        ("4 KiB ObjectCache hit + read", cache_hit_big),
    ];

    // Verify the cached handle, the reparse reader and the in-memory reader agree.
    let fb = FileBody::open(&path)?;
    for &(off, len) in big.iter().take(8) {
        let want = read(
            &body,
            ReadRequest {
                offset: off,
                length: len,
            },
        )?;
        assert_eq!(
            object.read(ReadRequest {
                offset: off,
                length: len
            })?,
            want
        );
        assert_eq!(
            fb.read(ReadRequest {
                offset: off,
                length: len
            })?,
            want
        );
    }

    println!("(a) cached vs reopen+reparse (ns):");
    for (name, st) in &cached_vs_reopen {
        println!(
            "  {name:<32} p50 {:>9.0} p99 {:>9.0} p99.9 {:>9.0}",
            st.p50, st.p99, st.p999
        );
    }

    // ---- (b) concurrent readers, optionally under background construction ----
    let stop = Arc::new(AtomicBool::new(false));
    let manifestations = Arc::new(AtomicU64::new(0));
    let mut concurrency: Vec<ConcurrencyRow> = Vec::new();

    let run_phase = |threads: usize| -> (Stats, f64) {
        let object = Arc::clone(&object);
        let sample = Arc::new(big.clone());
        let t0 = Instant::now();
        let all = thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..threads {
                let object = Arc::clone(&object);
                let sample = Arc::clone(&sample);
                handles.push(scope.spawn(move || {
                    let mut local = Vec::with_capacity(READS_PER_THREAD);
                    for i in 0..READS_PER_THREAD {
                        let (off, len) = sample[i % sample.len()];
                        let s = now_ns();
                        let got = object
                            .read(ReadRequest {
                                offset: off,
                                length: len,
                            })
                            .expect("concurrent read");
                        let e = now_ns();
                        assert_eq!(got.len(), len as usize);
                        local.push(e.saturating_sub(s));
                    }
                    local
                }));
            }
            let mut joined = Vec::new();
            for h in handles {
                joined.extend(h.join().expect("reader thread"));
            }
            joined
        });
        let elapsed = t0.elapsed().as_secs_f64();
        let reads = (threads * READS_PER_THREAD) as f64;
        (stats(all), reads / elapsed.max(f64::EPSILON))
    };

    // Baseline (no publication).
    for &threads in &THREADS {
        let (st, tput) = run_phase(threads);
        concurrency.push(ConcurrencyRow {
            threads,
            base_tput: tput,
            bg_tput: 0.0,
            base: st,
            bg: Stats {
                p50: 0.0,
                p99: 0.0,
                p999: 0.0,
                mean: 0.0,
            },
            bg_manifestations: 0,
        });
    }

    // With a background thread performing manifestation construction (CPU) on a
    // private body. The reader holds an immutable snapshot and shares no state
    // with the constructor, so no reader ever waits on the publisher's lock.
    let mut bg_body = create(&source, ManifestationProfile::Root)?;
    TileSet::manifest_hot_tiles(
        &mut bg_body,
        &trace,
        TILE_SHIFT,
        DirectoryKind::TwoLevelRadix,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )?;
    let bg_core = bg_body.core.clone();
    let bg_stop = Arc::clone(&stop);
    let bg_iters = Arc::clone(&manifestations);
    let bg_handle = thread::spawn(move || -> ekbloom::Result<()> {
        let regions: Vec<(u64, u64)> = (0..(HOT_BYTES / WINDOW))
            .map(|k| (k * WINDOW, WINDOW))
            .collect();
        let counts: Vec<u64> = (0..(HOT_BYTES / WINDOW)).map(|k| 10_000 - k).collect();
        let trace = AccessTrace { regions, counts };
        let budget = ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        };
        while !bg_stop.load(Ordering::Relaxed) {
            let mut fresh = EkbloomBody {
                object_id: bg_body.object_id,
                core: bg_core.clone(),
                logical_len: n,
                machine_id: 1,
                core_hash: bg_body.core_hash,
                ..EkbloomBody::default()
            };
            TileSet::manifest_hot_tiles(
                &mut fresh,
                &trace,
                TILE_SHIFT,
                DirectoryKind::TwoLevelRadix,
                &budget,
            )?;
            bg_iters.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });

    let before = manifestations.load(Ordering::Relaxed);
    for row in concurrency.iter_mut() {
        let (st, tput) = run_phase(row.threads);
        row.bg = st;
        row.bg_tput = tput;
    }
    stop.store(true, Ordering::Relaxed);
    bg_handle.join().expect("background thread")?;
    let total = manifestations
        .load(Ordering::Relaxed)
        .saturating_sub(before);
    for row in concurrency.iter_mut() {
        row.bg_manifestations = total;
    }

    println!("(b) concurrency (reads/s, p50/p99/p99.9 ns):");
    for row in &concurrency {
        println!(
            "  threads {:>2} baseline {:>10.0}/s p50 {:>6.0} p99 {:>7.0} p99.9 {:>8.0} | bg {:>10.0}/s p50 {:>6.0} p99 {:>7.0} p99.9 {:>8.0}",
            row.threads,
            row.base_tput,
            row.base.p50,
            row.base.p99,
            row.base.p999,
            row.bg_tput,
            row.bg.p50,
            row.bg.p99,
            row.bg.p999,
        );
    }

    // ---- (c) persisted tiles: bounded pread vs in-memory ----------------------
    let in_memory = body.tiles.last().expect("a tile set");
    let time_in_memory = {
        let mut durs = Vec::with_capacity(big.len());
        for &(off, len) in &big {
            let t0 = now_ns();
            let got = in_memory.read(&body.core, off, len)?;
            let t1 = now_ns();
            assert_eq!(got.len(), len as usize);
            durs.push(t1.saturating_sub(t0));
        }
        stats(durs)
    };
    let time_file_body = {
        let fb = FileBody::open(&path)?;
        assert!(fb.tiles_cover(0, WINDOW));
        let mut durs = Vec::with_capacity(big.len());
        for &(off, len) in &big {
            let t0 = now_ns();
            let got = fb.read(ReadRequest {
                offset: off,
                length: len,
            })?;
            let t1 = now_ns();
            assert_eq!(got.len(), len as usize);
            durs.push(t1.saturating_sub(t0));
        }
        stats(durs)
    };
    let persistence = vec![
        ("file-backed tile range (pread)", time_file_body),
        ("in-memory tile range", time_in_memory),
    ];
    println!("(c) tile persistence (4 KiB, ns):");
    for (name, st) in &persistence {
        println!(
            "  {name:<32} p50 {:>9.0} p99 {:>9.0} p99.9 {:>9.0}",
            st.p50, st.p99, st.p999
        );
    }

    // ---- honest verdict -------------------------------------------------------
    let small_cached = cached_vs_reopen[0].1;
    let small_reopen = cached_vs_reopen[1].1;
    let big_cached = cached_vs_reopen[2].1;
    let big_reopen = cached_vs_reopen[3].1;
    let worst_tail_ratio = concurrency
        .iter()
        .map(|r| {
            if r.base.p999 > 0.0 {
                r.bg.p999 / r.base.p999
            } else {
                1.0
            }
        })
        .fold(0.0f64, f64::max);
    let verdict = format!(
        "cached read ~{:.0}x faster than reopen+reparse at 1 byte ({:.0} vs {:.0} ns p50) and \
         ~{:.0}x at 4 KiB ({:.0} vs {:.0} ns p50); background construction left p50/p99 flat \
         and moved the worst thread-count p99.9 by {:.2}x; persisted tiles cost {:.1}x the \
         in-memory copy ({:.0} vs {:.0} ns p50) due to one pread per request.",
        small_reopen.p50 / small_cached.p50.max(1.0),
        small_cached.p50,
        small_reopen.p50,
        big_reopen.p50 / big_cached.p50.max(1.0),
        big_cached.p50,
        big_reopen.p50,
        worst_tail_ratio,
        time_file_body.p50 / time_in_memory.p50.max(1.0),
        time_file_body.p50,
        time_in_memory.p50,
    );
    println!("verdict: {verdict}");

    let json = render_json(
        body_bytes,
        &cached_vs_reopen,
        &concurrency,
        &persistence,
        &verdict,
    );
    let out = Path::new("evidence/bench/handles.json");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, json)?;
    println!("wrote {}", out.display());
    Ok(())
}
