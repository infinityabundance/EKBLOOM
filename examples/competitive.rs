//! Competitive experiment: EKBLOOM region-scoped manifested regions vs an
//! ordinary byte-budgeted LRU cache vs raw/no-cache.
//!
//! Under ONE corpus, ONE access trace and ONE byte budget, this harness measures:
//!
//! * **hot** latency — contiguous reads from the traced hot regions (the shape a
//!   region table can accelerate);
//! * **cold** latency — reads far outside the hot regions;
//! * **thrash** behaviour — a full sequential scan (which evicts an LRU) followed
//!   by the same hot reads again;
//! * **persistence / warmup** — close & reopen the object (EKBLOOM's manifested
//!   regions are reloaded from the file; the LRU cache is cleared).
//!
//! Everything is written to `evidence/bench/competitive.json`. Run inside the
//! pinned container with `sh scripts/run-competitive.sh`.

use std::fmt::Write as _;
use std::path::Path;

use ekbloom::{
    create, manifest_hot_regions, now_ns, percentiles, write_body, AccessTrace, Baseline,
    BaselineKind, ManifestationBudget, ManifestationProfile, ReadRequest, RegionReader,
    SweepConfig,
};

/// Deterministic corpus size (4 MiB): bounded, and large enough that a budgeted
/// LRU cannot hold the whole object.
const SRC_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed.
const SRC_SEED: u64 = 0xEB10_0000_C0DE_2026;
/// Trace region length in bytes.
const REGION_LEN: u64 = 64;
/// Number of hot regions named by the trace (front-loads a 32 KiB hot prefix).
const HOT_REGIONS: u64 = 512;
/// Budget headroom above the root body's physical bytes.
const BUDGET_EXTRA: u64 = 2 * 1024 * 1024;
/// Sample count per latency measurement.
const SAMPLES: usize = 512;
/// Cold-set window start (everything at/after this is "cold").
const COLD_BASE: u64 = 2 * 1024 * 1024;

/// Deterministic LCG corpus (identical generator to the other harnesses).
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

/// `count` deterministic offsets in `[lo, hi)`.
fn lcg_offsets(seed: u64, count: usize, lo: u64, hi: u64) -> Vec<u64> {
    let span = hi.saturating_sub(lo).max(1);
    let mut s = seed;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lo + (s >> 17) % span
        })
        .collect()
}

/// Time one read per offset; returns `(p50, p99)` nanoseconds.
fn time_reads<F: FnMut(u64) -> usize>(offsets: &[u64], mut read: F) -> (f64, f64) {
    let mut durs = Vec::with_capacity(offsets.len());
    for &off in offsets {
        let t0 = now_ns();
        let n = read(off);
        let t1 = now_ns();
        assert!(n > 0, "empty read at offset {off}");
        durs.push(t1.saturating_sub(t0));
    }
    durs.sort_unstable();
    let p = percentiles(&durs);
    (p.p50, p.p99)
}

/// Time a single read at `off` (used for the first read after a thrash/reopen,
/// where an LRU pays a cold miss that a manifested region does not).
fn first_read_ns<F: FnMut(u64) -> usize>(off: u64, mut read: F) -> u64 {
    let t0 = now_ns();
    let n = read(off);
    let t1 = now_ns();
    assert!(n > 0, "empty read at offset {off}");
    t1.saturating_sub(t0)
}

struct Metrics {
    hot_p50: f64,
    hot_p99: f64,
    cold_p50: f64,
    thrash_first_read_ns: u64,
    post_thrash_p50: f64,
    reopen_first_read_ns: u64,
    reopen_first_p50: f64,
    reopen_warm_p50: f64,
}

fn print_row(name: &str, accel: u64, m: &Metrics) {
    println!(
        "{name:<26} {accel:>10} {:>9.0} {:>9.0} {:>9.0} {:>10} {:>9.0} {:>11} {:>11.0}",
        m.hot_p50,
        m.hot_p99,
        m.cold_p50,
        m.thrash_first_read_ns,
        m.post_thrash_p50,
        m.reopen_first_read_ns,
        m.reopen_warm_p50
    );
}

fn main() -> ekbloom::Result<()> {
    let source = lcg_bytes(SRC_SEED, SRC_LEN);
    let n = source.len() as u64;
    let cfg = SweepConfig {
        iterations: 32,
        seed: 0x9E37_79B9_7F4A_7C15,
    };
    let hot_hi = HOT_REGIONS * REGION_LEN;
    let dir = Path::new("target/competitive-tmp");
    std::fs::create_dir_all(dir)?;

    // ---- ONE access trace: `HOT_REGIONS` regions, counts decreasing ----------
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    for k in 0..HOT_REGIONS {
        regions.push((k * REGION_LEN, REGION_LEN));
        counts.push(10_000u64.saturating_sub(k * 10));
    }
    let trace = AccessTrace { regions, counts };

    // ---- (a) EKBLOOM region-scoped manifestation -----------------------------
    let mut body = create(&source, ManifestationProfile::Root)?;
    let budget = ManifestationBudget {
        maximum_physical_bytes: body.physical_bytes() + BUDGET_EXTRA,
    };
    let report = manifest_hot_regions(&mut body, &trace, &budget, &cfg)?;
    let ekb_accel_bytes = report.added_physical_bytes;

    // ---- (b) LRU cache, (c) raw/no-cache — same byte budget for the cache ----
    let lru_capacity = ekb_accel_bytes.max(4096);
    let lru = Baseline::build_lru(&source, lru_capacity, dir)?;
    let raw = Baseline::build(BaselineKind::RawFile, &source, dir)?;

    // Hot reads are contiguous (REGION_LEN bytes) so a region table can leap a
    // whole window per lookup; cold reads are single bytes far out at `COLD_BASE`.
    let window = REGION_LEN;
    let hot_offsets: Vec<u64> = (0..SAMPLES)
        .map(|i| (i as u64 % HOT_REGIONS) * REGION_LEN)
        .collect();
    let cold_offsets = lcg_offsets(0xC01D_0000_0000_0001, SAMPLES, COLD_BASE, n - 1);

    // Build the region index once for the in-memory body, and once for the
    // reopened body (a consumer pays this setup cost once, not per read).
    let ekb_reader = RegionReader::new(&body);

    // ---- EKBLOOM phases ------------------------------------------------------
    // Warm.
    for _ in 0..2 {
        for &off in &hot_offsets {
            let _ = ekb_reader.read(off, window)?;
        }
    }
    let ekb_hot = time_reads(&hot_offsets, |off| {
        ekb_reader.read(off, window).map(|v| v.len()).unwrap_or(0)
    });
    let ekb_cold = time_reads(&cold_offsets, |off| {
        ekb_reader.read(off, 1).map(|v| v.len()).unwrap_or(0)
    });
    // Thrash: one full sequential scan (never evicts a manifested region).
    let _ = ekb_reader.read(0, n)?;
    let ekb_thrash_first = first_read_ns(hot_offsets[0], |off| {
        ekb_reader.read(off, window).map(|v| v.len()).unwrap_or(0)
    });
    let ekb_post_thrash = time_reads(&hot_offsets, |off| {
        ekb_reader.read(off, window).map(|v| v.len()).unwrap_or(0)
    });
    // Persistence: write, close, reopen from the `.ekb` file.
    let ekb_path = dir.join("competitive.ekb");
    write_body(&ekb_path, &body)?;
    drop(body);
    let reopened = ekbloom::open(&ekb_path)?;
    let ekb_reopen = RegionReader::new(&reopened);
    let ekb_reopen_first_read = first_read_ns(hot_offsets[0], |off| {
        ekb_reopen.read(off, window).map(|v| v.len()).unwrap_or(0)
    });
    let ekb_reopen_first = time_reads(&hot_offsets, |off| {
        ekb_reopen.read(off, window).map(|v| v.len()).unwrap_or(0)
    });
    let ekb_reopen_warm = time_reads(&hot_offsets, |off| {
        ekb_reopen.read(off, window).map(|v| v.len()).unwrap_or(0)
    });
    let ekb = Metrics {
        hot_p50: ekb_hot.0,
        hot_p99: ekb_hot.1,
        cold_p50: ekb_cold.0,
        thrash_first_read_ns: ekb_thrash_first,
        post_thrash_p50: ekb_post_thrash.0,
        reopen_first_read_ns: ekb_reopen_first_read,
        reopen_first_p50: ekb_reopen_first.0,
        reopen_warm_p50: ekb_reopen_warm.0,
    };

    // ---- LRU cache phases ----------------------------------------------------
    let lru_read = |off: u64, len: u64| -> usize {
        let mut out = Vec::new();
        lru.read_into(
            ReadRequest {
                offset: off,
                length: len,
            },
            &mut out,
        )
        .expect("lru read");
        out.len()
    };
    for _ in 0..2 {
        for &off in &hot_offsets {
            let _ = lru_read(off, window);
        }
    }
    let lru_hot = time_reads(&hot_offsets, |off| lru_read(off, window));
    let lru_cold = time_reads(&cold_offsets, |off| lru_read(off, 1));
    // Thrash: full sequential scan — this evicts the hot blocks.
    let mut sink = Vec::new();
    lru.read_into(
        ReadRequest {
            offset: 0,
            length: n,
        },
        &mut sink,
    )?;
    sink.clear();
    let lru_thrash_first = first_read_ns(hot_offsets[0], |off| lru_read(off, window));
    let lru_post_thrash = time_reads(&hot_offsets, |off| lru_read(off, window));
    // Reopen: clearing the cache models a process restart (backing file persists).
    lru.clear_cache();
    let lru_reopen_first_read = first_read_ns(hot_offsets[0], |off| lru_read(off, window));
    let lru_reopen_first = time_reads(&hot_offsets, |off| lru_read(off, window));
    let lru_reopen_warm = time_reads(&hot_offsets, |off| lru_read(off, window));
    let lru_metrics = Metrics {
        hot_p50: lru_hot.0,
        hot_p99: lru_hot.1,
        cold_p50: lru_cold.0,
        thrash_first_read_ns: lru_thrash_first,
        post_thrash_p50: lru_post_thrash.0,
        reopen_first_read_ns: lru_reopen_first_read,
        reopen_first_p50: lru_reopen_first.0,
        reopen_warm_p50: lru_reopen_warm.0,
    };

    // ---- raw / no-cache phases ----------------------------------------------
    let raw_read = |off: u64, len: u64| -> usize {
        let mut out = Vec::new();
        raw.read_into(
            ReadRequest {
                offset: off,
                length: len,
            },
            &mut out,
        )
        .expect("raw read");
        out.len()
    };
    let raw_hot = time_reads(&hot_offsets, |off| raw_read(off, window));
    let raw_cold = time_reads(&cold_offsets, |off| raw_read(off, 1));
    let mut sink = Vec::new();
    raw.read_into(
        ReadRequest {
            offset: 0,
            length: n,
        },
        &mut sink,
    )?;
    sink.clear();
    let raw_thrash_first = first_read_ns(hot_offsets[0], |off| raw_read(off, window));
    let raw_post_thrash = time_reads(&hot_offsets, |off| raw_read(off, window));
    let raw_metrics = Metrics {
        hot_p50: raw_hot.0,
        hot_p99: raw_hot.1,
        cold_p50: raw_cold.0,
        thrash_first_read_ns: raw_thrash_first,
        post_thrash_p50: raw_post_thrash.0,
        // Raw has no state, so "reopen" is the same direct access.
        reopen_first_read_ns: raw_hot.0 as u64,
        reopen_first_p50: raw_hot.0,
        reopen_warm_p50: raw_hot.0,
    };

    // ---- report --------------------------------------------------------------
    println!(
        "corpus={} B  hot={} B ({} regions x {})  budget_extra={} B  lru_capacity={} B",
        n, hot_hi, HOT_REGIONS, REGION_LEN, BUDGET_EXTRA, lru_capacity
    );
    println!(
        "ekbloom region-adapt: selected={} certified={} added={} B gen={} cold_K={:?}",
        report.selected_regions,
        report.certified_regions,
        ekb_accel_bytes,
        report.new_generation,
        report.cold_checkpoint_interval
    );
    println!(
        "lru: capacity={} resident={} hits={} misses={}",
        lru.capacity_bytes(),
        lru.resident_bytes(),
        lru.hits(),
        lru.misses()
    );
    println!(
        "{:<26} {:>10} {:>9} {:>9} {:>9} {:>10} {:>9} {:>11} {:>11}",
        "name",
        "accel B",
        "hot p50",
        "hot p99",
        "cold p50",
        "thrash 1st",
        "thr p50",
        "reopen 1st",
        "warm p50"
    );
    print_row("ekbloom region-adapt", ekb_accel_bytes, &ekb);
    print_row("lru-cache", lru_capacity, &lru_metrics);
    print_row("raw-file (no cache)", 0, &raw_metrics);

    let json = render_json(
        &source,
        &cfg,
        &report,
        ekb_accel_bytes,
        lru_capacity,
        &ekb,
        &lru_metrics,
        &raw_metrics,
        lru.hits(),
        lru.misses(),
    );
    let out = Path::new("evidence/bench/competitive.json");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, json)?;
    println!("wrote {}", out.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_json(
    source: &[u8],
    cfg: &SweepConfig,
    report: &ekbloom::RegionAdaptReport,
    ekb_accel_bytes: u64,
    lru_capacity: u64,
    ekb: &Metrics,
    lru: &Metrics,
    raw: &Metrics,
    lru_hits: u64,
    lru_misses: u64,
) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(s, "  \"corpus_bytes\": {},", source.len());
    let _ = writeln!(s, "  \"corpus_seed\": {},", SRC_SEED);
    let _ = writeln!(s, "  \"iterations\": {},", cfg.iterations);
    let _ = writeln!(s, "  \"sweep_seed\": {},", cfg.seed);
    let _ = writeln!(s, "  \"hot_regions\": {},", HOT_REGIONS);
    let _ = writeln!(s, "  \"region_len\": {},", REGION_LEN);
    let _ = writeln!(s, "  \"hot_bytes\": {},", HOT_REGIONS * REGION_LEN);
    let _ = writeln!(s, "  \"samples\": {},", SAMPLES);
    let _ = writeln!(s, "  \"budget_bytes\": {},", report.budget_bytes);
    s.push_str("  \"ekbloom_region_adapt\": {\n");
    let _ = writeln!(s, "    \"selected_regions\": {},", report.selected_regions);
    let _ = writeln!(
        s,
        "    \"certified_regions\": {},",
        report.certified_regions
    );
    let _ = writeln!(s, "    \"added_physical_bytes\": {ekb_accel_bytes},");
    let _ = writeln!(s, "    \"new_generation\": {},", report.new_generation);
    let _ = writeln!(
        s,
        "    \"cold_checkpoint_interval\": {},",
        report
            .cold_checkpoint_interval
            .map(|k| k.to_string())
            .unwrap_or_else(|| "null".to_string())
    );
    let _ = writeln!(s, "    \"improved\": {},", report.improved);
    s.push_str("    \"metrics\": ");
    s.push_str(&metrics_json(ekb));
    s.push_str("  },\n");
    s.push_str("  \"lru_cache\": {\n");
    let _ = writeln!(s, "    \"capacity_bytes\": {lru_capacity},");
    let _ = writeln!(s, "    \"hits\": {lru_hits},");
    let _ = writeln!(s, "    \"misses\": {lru_misses},");
    s.push_str("    \"metrics\": ");
    s.push_str(&metrics_json(lru));
    s.push_str("  },\n");
    s.push_str("  \"raw_no_cache\": {\n");
    let _ = writeln!(s, "    \"accel_bytes\": 0,");
    s.push_str("    \"metrics\": ");
    s.push_str(&metrics_json(raw));
    s.push_str("  }\n}\n");
    s
}

fn metrics_json(m: &Metrics) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(s, "      \"hot_ns_p50\": {:.1},", m.hot_p50);
    let _ = writeln!(s, "      \"hot_ns_p99\": {:.1},", m.hot_p99);
    let _ = writeln!(s, "      \"cold_ns_p50\": {:.1},", m.cold_p50);
    let _ = writeln!(
        s,
        "      \"post_thrash_first_read_ns\": {},",
        m.thrash_first_read_ns
    );
    let _ = writeln!(
        s,
        "      \"post_thrash_hot_ns_p50\": {:.1},",
        m.post_thrash_p50
    );
    let _ = writeln!(
        s,
        "      \"reopen_first_read_ns\": {},",
        m.reopen_first_read_ns
    );
    let _ = writeln!(
        s,
        "      \"reopen_first_hot_ns_p50\": {:.1},",
        m.reopen_first_p50
    );
    let _ = writeln!(
        s,
        "      \"reopen_warm_hot_ns_p50\": {:.1}",
        m.reopen_warm_p50
    );
    s.push_str("    }\n");
    s
}
