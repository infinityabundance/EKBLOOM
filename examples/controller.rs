//! Online workload-adaptation controller vs strong baselines under a **shifting**
//! workload (a hot window that hops `WINDOWS` times to fresh, non-overlapping
//! regions spread across a heterogeneous corpus).
//!
//! At each equal physical budget this compares:
//!
//! * **EKBLOOM online controller** — [`Controller`]: observes accesses, and once per
//!   epoch additively manifests certified byte tiles for the current hot window
//!   under the memory, background-I/O and construction budgets, with hysteresis;
//! * **static** — plan once (window 0) and never adapt;
//! * **TinyLFU** — a conventional frequency-aware admission cache ([`TinyLfuCache`])
//!   over *LZ4* codec blocks (`lz4_flex`, an existing dev-dependency);
//! * **codec + LRU** — a conventional LRU block cache ([`HotBlockCache`]) over the
//!   same LZ4 codec.
//!
//! For every contender the harness reports p50/p95/p99 **at each workload shift**
//! (the *cold* epoch, before the contender has reacted) and after it has warmed
//! (*warm*), plus bytes spent, SLO-miss count, construction cost and retained
//! bytes. Reads are byte-exact against the source. Writes
//! `evidence/bench/controller.json`; run by `sh scripts/run-controller.sh`.

use std::fmt::Write as _;
use std::path::Path;

use ekbloom::{
    create, manifest, now_ns, percentiles, AccessPath, AccessTrace, ControlConfig, Controller,
    CostModel, CourtConfig, DirectoryKind, EkbloomBody, EkbloomError, HotBlockCache,
    ManifestationBudget, ManifestationProfile, Planner, ReadRequest, TileSet, TinyLfuCache,
};

/// Deterministic corpus size (4 MiB, well inside the memory-capped container).
const CORPUS_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed.
const CORPUS_SEED: u64 = 0x0C07_7A01_0000_5EED;
/// Observation/manifestation region size (bytes); equals the tile size.
const REGION_BYTES: u64 = 4096;
/// Decoded block size for the conventional codec+cache baselines.
const BLOCK_BYTES: u64 = 4096;
/// Regions in each hot window (16 regions = 64 KiB).
const WINDOW_REGIONS: usize = 16;
/// Number of distinct (non-overlapping) windows the hot region hops through.
const WINDOWS: usize = 6;
/// Page reads timed per epoch (16 full passes over the 16-region window).
const READS_PER_EPOCH: usize = 256;
/// Control epochs per window phase.
const EPOCHS_PER_PHASE: usize = 6;
/// The latency SLO threshold (ns): a slower access is an SLO miss.
const SLO_P99_NS: u64 = 1500;
/// Hysteresis (minimum epochs between changes **and** band width).
const HYSTERESIS: u32 = 2;
/// Background-I/O budget handed to the controller (bytes per virtual second).
const MAX_BG_IO_BYTES_PER_S: u64 = 8 * 1024 * 1024;
/// Total construction budget handed to the controller (ns).
const CONSTRUCTION_BUDGET_NS: u64 = 200_000_000;
/// `Tight` extra allowance: fits ~4 windows, so the additive controller saturates.
const TIGHT_ALLOWANCE: u64 = 256 * 1024;
/// `Generous` extra allowance: fits every window.
const GENEROUS_ALLOWANCE: u64 = 768 * 1024;

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

/// A heterogeneous corpus: a low-entropy first half (compressible) followed by a
/// uniform second half (incompressible) — the same generator as `examples/optimizer.rs`.
fn heterogeneous(seed: u64, len: usize) -> Vec<u8> {
    let mut next = lcg(seed, len).into_iter();
    let half = len / 2;
    (0..len)
        .map(|i| {
            let byte = next.next().unwrap_or(0);
            if i < half {
                byte & 0x07
            } else {
                byte
            }
        })
        .collect()
}

/// Region-aligned start of window `w`, spread evenly across the whole object.
fn window_start(w: usize, n: u64) -> u64 {
    let span = n / WINDOWS as u64;
    (w as u64 * span) & !(REGION_BYTES - 1)
}

/// A deterministic schedule of `(offset, length)` page reads, grouped by epoch;
/// epoch `g` belongs to window `g / EPOCHS_PER_PHASE`. **Every** region of the
/// window is read in every epoch (rotated), so a shift exposes the whole new
/// window to the contender within its first (cold) epoch.
fn build_schedule(n: u64) -> Vec<Vec<(u64, u64)>> {
    let mut out = Vec::with_capacity(WINDOWS * EPOCHS_PER_PHASE);
    for w in 0..WINDOWS {
        let base = window_start(w, n);
        for e in 0..EPOCHS_PER_PHASE {
            let mut v = Vec::with_capacity(READS_PER_EPOCH);
            for k in 0..READS_PER_EPOCH {
                let r = ((k + e) % WINDOW_REGIONS) as u64;
                v.push((base + r * REGION_BYTES, REGION_BYTES));
            }
            out.push(v);
        }
    }
    out
}

fn make_planner(body: &EkbloomBody) -> ekbloom::Result<Planner> {
    let plan = ekbloom::AccessPlan::plan_with(
        body,
        &CourtConfig::default(),
        REGION_BYTES,
        CostModel::measured_default(),
    )?;
    Ok(Planner::new(plan))
}

/// Per-window cold/warm latency buckets.
#[derive(Clone, Default)]
struct Buckets {
    cold: Vec<Vec<u64>>,
    warm: Vec<Vec<u64>>,
}

impl Buckets {
    fn new() -> Buckets {
        Buckets {
            cold: vec![Vec::new(); WINDOWS],
            warm: vec![Vec::new(); WINDOWS],
        }
    }

    fn push(&mut self, window: usize, in_phase: usize, local: Vec<u64>) {
        if in_phase == 0 {
            self.cold[window].extend(local);
        } else {
            self.warm[window].extend(local);
        }
    }
}

/// One contender's measured result under one budget.
#[derive(Clone)]
struct Contender {
    name: &'static str,
    retained_bytes: u64,
    construction_ns: u64,
    adaptations: u64,
    slo_misses: u64,
    bytes_spent: u64,
    hot_fraction: f64,
    p50_cold: Vec<f64>,
    p95_cold: Vec<f64>,
    p99_cold: Vec<f64>,
    p50_warm: Vec<f64>,
    p95_warm: Vec<f64>,
    p99_warm: Vec<f64>,
    cold_p50_ns: f64,
    cold_p99_ns: f64,
    warm_p50_ns: f64,
    warm_p99_ns: f64,
    note: String,
}

fn pct(v: &[u64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_unstable();
    let p = percentiles(&s);
    (p.p50, p.p95, p.p99)
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

#[allow(clippy::too_many_arguments)]
fn finalize(
    name: &'static str,
    retained_bytes: u64,
    construction_ns: u64,
    adaptations: u64,
    slo_misses: u64,
    bytes_spent: u64,
    hot_reads: u64,
    total_reads: u64,
    buckets: Buckets,
    note: String,
) -> Contender {
    let mut p50_cold = Vec::with_capacity(WINDOWS);
    let mut p95_cold = Vec::with_capacity(WINDOWS);
    let mut p99_cold = Vec::with_capacity(WINDOWS);
    let mut p50_warm = Vec::with_capacity(WINDOWS);
    let mut p95_warm = Vec::with_capacity(WINDOWS);
    let mut p99_warm = Vec::with_capacity(WINDOWS);
    for w in 0..WINDOWS {
        let (a, b, c) = pct(&buckets.cold[w]);
        p50_cold.push(a);
        p95_cold.push(b);
        p99_cold.push(c);
        let (a, b, c) = pct(&buckets.warm[w]);
        p50_warm.push(a);
        p95_warm.push(b);
        p99_warm.push(c);
    }
    let cold_p50_ns = median(p50_cold.clone());
    let cold_p99_ns = median(p99_cold.clone());
    let warm_p50_ns = median(p50_warm.clone());
    let warm_p99_ns = median(p99_warm.clone());
    let hot_fraction = if total_reads == 0 {
        0.0
    } else {
        hot_reads as f64 / total_reads as f64
    };
    Contender {
        name,
        retained_bytes,
        construction_ns,
        adaptations,
        slo_misses,
        bytes_spent,
        hot_fraction,
        p50_cold,
        p95_cold,
        p99_cold,
        p50_warm,
        p95_warm,
        p99_warm,
        cold_p50_ns,
        cold_p99_ns,
        warm_p50_ns,
        warm_p99_ns,
        note,
    }
}

/// The base committed codec body: `Root` plus a bounded cold-restart checkpoint set.
fn base_body(source: &[u8]) -> ekbloom::Result<EkbloomBody> {
    let mut body = create(source, ManifestationProfile::Root)?;
    manifest(&mut body, ManifestationProfile::Sparse { interval: 1024 })?;
    Ok(body)
}

fn run_controller(
    base: &EkbloomBody,
    source: &[u8],
    sched: &[Vec<(u64, u64)>],
    memory_budget: u64,
) -> ekbloom::Result<Contender> {
    let mut body = base.clone();
    let cfg = ControlConfig {
        memory_budget,
        slo_p99_ns: SLO_P99_NS,
        hysteresis: HYSTERESIS,
        max_bg_io_bytes_per_s: MAX_BG_IO_BYTES_PER_S,
        construction_budget_ns: CONSTRUCTION_BUDGET_NS,
    };
    let mut controller = Controller::new(cfg, &body)?;
    let mut planner = make_planner(&body)?;
    let mut buckets = Buckets::new();
    let mut hot_reads = 0u64;
    let mut total_reads = 0u64;

    for (g, reads) in sched.iter().enumerate() {
        let window = g / EPOCHS_PER_PHASE;
        let in_phase = g % EPOCHS_PER_PHASE;
        let mut local = Vec::with_capacity(reads.len());
        for &(off, len) in reads {
            let is_hot = planner.plan().path_for(off) == AccessPath::HotBytes;
            let t0 = now_ns();
            let got = planner.read(
                &body,
                ReadRequest {
                    offset: off,
                    length: len,
                },
            )?;
            let dt = now_ns().saturating_sub(t0);
            assert_eq!(got.len() as u64, len);
            assert_eq!(&got, &source[off as usize..(off + len) as usize]);
            controller.observe(
                ReadRequest {
                    offset: off,
                    length: len,
                },
                dt,
            );
            if is_hot {
                hot_reads += 1;
            }
            total_reads += 1;
            local.push(dt);
        }
        buckets.push(window, in_phase, local);
        let action = controller.step(&mut body)?;
        if action.changed {
            planner = make_planner(&body)?;
        }
    }

    let note = format!(
        "online controller: {} adaptation(s), hysteresis {HYSTERESIS}, {} B committed",
        controller.adaptations(),
        body.physical_bytes()
    );
    Ok(finalize(
        "controller",
        body.physical_bytes(),
        controller.construction_ns(),
        controller.adaptations(),
        controller.slo_misses(),
        controller.bytes_spent(),
        hot_reads,
        total_reads,
        buckets,
        note,
    ))
}

fn run_static(
    base: &EkbloomBody,
    source: &[u8],
    sched: &[Vec<(u64, u64)>],
    memory_budget: u64,
) -> ekbloom::Result<Contender> {
    // Plan once: manifest the window-0 regions and never adapt again.
    let mut body = base.clone();
    let start = window_start(0, source.len() as u64);
    let regions: Vec<(u64, u64)> = (0..WINDOW_REGIONS)
        .map(|r| (start + r as u64 * REGION_BYTES, REGION_BYTES))
        .collect();
    let counts = vec![1_000u64; WINDOW_REGIONS];
    let trace = AccessTrace { regions, counts };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &ManifestationBudget {
            maximum_physical_bytes: memory_budget,
        },
    )?;
    let planner = make_planner(&body)?;

    let mut buckets = Buckets::new();
    let mut hot_reads = 0u64;
    let mut total_reads = 0u64;
    let mut slo_misses = 0u64;
    for (g, reads) in sched.iter().enumerate() {
        let window = g / EPOCHS_PER_PHASE;
        let in_phase = g % EPOCHS_PER_PHASE;
        let mut local = Vec::with_capacity(reads.len());
        for &(off, len) in reads {
            let is_hot = planner.plan().path_for(off) == AccessPath::HotBytes;
            let t0 = now_ns();
            let got = planner.read(
                &body,
                ReadRequest {
                    offset: off,
                    length: len,
                },
            )?;
            let dt = now_ns().saturating_sub(t0);
            assert_eq!(&got, &source[off as usize..(off + len) as usize]);
            if dt > SLO_P99_NS {
                slo_misses += 1;
            }
            if is_hot {
                hot_reads += 1;
            }
            total_reads += 1;
            local.push(dt);
        }
        buckets.push(window, in_phase, local);
    }
    Ok(finalize(
        "static",
        body.physical_bytes(),
        0,
        0,
        slo_misses,
        0,
        hot_reads,
        total_reads,
        buckets,
        "static: plan once (window 0), never adapt".to_string(),
    ))
}

/// LZ4-block-compress `source` into independently decodable 4 KiB blocks.
fn lz4_blocks(source: &[u8]) -> (Vec<Vec<u8>>, u64) {
    let mut blocks = Vec::with_capacity(source.len().div_ceil(BLOCK_BYTES as usize));
    let mut total = 0u64;
    for chunk in source.chunks(BLOCK_BYTES as usize) {
        let c = lz4_flex::compress_prepend_size(chunk);
        total += c.len() as u64;
        blocks.push(c);
    }
    (blocks, total)
}

fn run_tinylfu(
    source: &[u8],
    sched: &[Vec<(u64, u64)>],
    allowance: u64,
) -> ekbloom::Result<Contender> {
    let (blocks, codec_bytes) = lz4_blocks(source);
    let mut cache = TinyLfuCache::new(BLOCK_BYTES, allowance);
    let mut buckets = Buckets::new();
    let mut slo_misses = 0u64;
    let mut total_reads = 0u64;
    for (g, reads) in sched.iter().enumerate() {
        let window = g / EPOCHS_PER_PHASE;
        let in_phase = g % EPOCHS_PER_PHASE;
        let mut local = Vec::with_capacity(reads.len());
        for &(off, len) in reads {
            let block = off >> 12;
            let block_off = block << 12;
            let t0 = now_ns();
            let data = cache.get_or_load(block, || {
                blocks
                    .get(block as usize)
                    .and_then(|c| lz4_flex::decompress_size_prepended(c).ok())
            });
            let start = (off - block_off) as usize;
            let bytes = data.and_then(|b| b.get(start..start + len as usize));
            let dt = now_ns().saturating_sub(t0);
            let bytes = bytes.ok_or_else(|| EkbloomError::InvalidArgument {
                what: "tinylfu cache read out of range".to_string(),
            })?;
            let out = bytes.to_vec();
            assert_eq!(out, &source[off as usize..(off + len) as usize]);
            if dt > SLO_P99_NS {
                slo_misses += 1;
            }
            total_reads += 1;
            local.push(dt);
            std::hint::black_box(out.len());
        }
        buckets.push(window, in_phase, local);
    }
    let note = format!(
        "LZ4 ({} B, {:.2}x) + TinyLFU: {} hits / {} misses, {} evictions, {} admissions, {} rejections",
        codec_bytes,
        source.len() as f64 / codec_bytes.max(1) as f64,
        cache.hits(),
        cache.misses(),
        cache.evictions(),
        cache.admissions(),
        cache.rejections(),
    );
    Ok(finalize(
        "tinylfu",
        codec_bytes.saturating_add(allowance),
        0,
        0,
        slo_misses,
        0,
        0,
        total_reads,
        buckets,
        note,
    ))
}

fn run_codec_lru(
    source: &[u8],
    sched: &[Vec<(u64, u64)>],
    allowance: u64,
) -> ekbloom::Result<Contender> {
    let (blocks, codec_bytes) = lz4_blocks(source);
    let mut cache = HotBlockCache::new(BLOCK_BYTES, allowance);
    let mut buckets = Buckets::new();
    let mut slo_misses = 0u64;
    let mut total_reads = 0u64;
    for (g, reads) in sched.iter().enumerate() {
        let window = g / EPOCHS_PER_PHASE;
        let in_phase = g % EPOCHS_PER_PHASE;
        let mut local = Vec::with_capacity(reads.len());
        for &(off, len) in reads {
            let block = off >> 12;
            let block_off = block << 12;
            let t0 = now_ns();
            let data = cache.get_or_load(block, || {
                blocks
                    .get(block as usize)
                    .and_then(|c| lz4_flex::decompress_size_prepended(c).ok())
            });
            let start = (off - block_off) as usize;
            let bytes = data.and_then(|b| b.get(start..start + len as usize));
            let dt = now_ns().saturating_sub(t0);
            let bytes = bytes.ok_or_else(|| EkbloomError::InvalidArgument {
                what: "lru cache read out of range".to_string(),
            })?;
            let out = bytes.to_vec();
            assert_eq!(out, &source[off as usize..(off + len) as usize]);
            if dt > SLO_P99_NS {
                slo_misses += 1;
            }
            total_reads += 1;
            local.push(dt);
            std::hint::black_box(out.len());
        }
        buckets.push(window, in_phase, local);
    }
    let note = format!(
        "LZ4 ({} B, {:.2}x) + LRU: {} hits / {} misses, {} evictions",
        codec_bytes,
        source.len() as f64 / codec_bytes.max(1) as f64,
        cache.hits(),
        cache.misses(),
        cache.evictions(),
    );
    Ok(finalize(
        "codec_lru",
        codec_bytes.saturating_add(allowance),
        0,
        0,
        slo_misses,
        0,
        0,
        total_reads,
        buckets,
        note,
    ))
}

fn run_rawvec(source: &[u8], sched: &[Vec<(u64, u64)>]) -> Contender {
    let mut buckets = Buckets::new();
    let mut slo_misses = 0u64;
    let mut total_reads = 0u64;
    for (g, reads) in sched.iter().enumerate() {
        let window = g / EPOCHS_PER_PHASE;
        let in_phase = g % EPOCHS_PER_PHASE;
        let mut local = Vec::with_capacity(reads.len());
        for &(off, len) in reads {
            let t0 = now_ns();
            let bytes = &source[off as usize..(off + len) as usize];
            let dt = now_ns().saturating_sub(t0);
            std::hint::black_box(bytes.len());
            if dt > SLO_P99_NS {
                slo_misses += 1;
            }
            total_reads += 1;
            local.push(dt);
        }
        buckets.push(window, in_phase, local);
    }
    finalize(
        "raw_vec",
        source.len() as u64,
        0,
        0,
        slo_misses,
        0,
        0,
        total_reads,
        buckets,
        "raw in-memory floor (reference)".to_string(),
    )
}

fn write_contender(s: &mut String, c: &Contender, last: bool) {
    let _ = writeln!(s, "        \"{}\": {{", c.name);
    let _ = writeln!(s, "          \"retained_bytes\": {},", c.retained_bytes);
    let _ = writeln!(s, "          \"bytes_spent\": {},", c.bytes_spent);
    let _ = writeln!(s, "          \"construction_ns\": {},", c.construction_ns);
    let _ = writeln!(s, "          \"adaptations\": {},", c.adaptations);
    let _ = writeln!(s, "          \"slo_misses\": {},", c.slo_misses);
    let _ = writeln!(s, "          \"hot_fraction\": {:.4},", c.hot_fraction);
    let _ = writeln!(s, "          \"cold_p50_ns\": {:.0},", c.cold_p50_ns);
    let _ = writeln!(s, "          \"cold_p99_ns\": {:.0},", c.cold_p99_ns);
    let _ = writeln!(s, "          \"warm_p50_ns\": {:.0},", c.warm_p50_ns);
    let _ = writeln!(s, "          \"warm_p99_ns\": {:.0},", c.warm_p99_ns);
    write_arr(s, "p50_cold", &c.p50_cold);
    write_arr(s, "p95_cold", &c.p95_cold);
    write_arr(s, "p99_cold", &c.p99_cold);
    write_arr(s, "p50_warm", &c.p50_warm);
    write_arr(s, "p95_warm", &c.p95_warm);
    write_arr(s, "p99_warm", &c.p99_warm);
    let _ = writeln!(s, "          \"note\": \"{}\"", c.note);
    s.push_str(if last { "        }\n" } else { "        },\n" });
}

fn write_arr(s: &mut String, name: &str, v: &[f64]) {
    let items: Vec<String> = v.iter().map(|x| format!("{x:.0}")).collect();
    let _ = writeln!(s, "          \"{name}\": [{}],", items.join(", "));
}

fn main() -> ekbloom::Result<()> {
    let source = heterogeneous(CORPUS_SEED, CORPUS_LEN);
    let n = source.len() as u64;
    let base = base_body(&source)?;
    let base_physical = base.physical_bytes();
    let sched = build_schedule(n);

    println!(
        "controller benchmark: corpus {} B, base {} B, {} windows x {} regions ({:.0} KiB), {} reads/epoch x {} epochs",
        source.len(),
        base_physical,
        WINDOWS,
        WINDOW_REGIONS,
        (WINDOW_REGIONS as u64 * REGION_BYTES) as f64 / 1024.0,
        READS_PER_EPOCH,
        EPOCHS_PER_PHASE
    );

    let budgets: [(&str, u64); 2] = [("tight", TIGHT_ALLOWANCE), ("generous", GENEROUS_ALLOWANCE)];

    let mut out = String::new();
    out.push_str("{\n");
    let _ = writeln!(
        out,
        "  \"generated_by\": \"examples/controller.rs (sh scripts/run-controller.sh)\","
    );
    let _ = writeln!(out, "  \"corpus_len\": {},", source.len());
    let _ = writeln!(out, "  \"corpus_seed\": {CORPUS_SEED},");
    let _ = writeln!(out, "  \"base_physical_bytes\": {base_physical},");
    let _ = writeln!(out, "  \"region_bytes\": {REGION_BYTES},");
    let _ = writeln!(out, "  \"window_regions\": {WINDOW_REGIONS},");
    let _ = writeln!(out, "  \"windows\": {WINDOWS},");
    let _ = writeln!(out, "  \"reads_per_epoch\": {READS_PER_EPOCH},");
    let _ = writeln!(out, "  \"epochs_per_phase\": {EPOCHS_PER_PHASE},");
    let _ = writeln!(out, "  \"slo_p99_ns\": {SLO_P99_NS},");
    let _ = writeln!(out, "  \"hysteresis\": {HYSTERESIS},");
    let _ = writeln!(out, "  \"max_bg_io_bytes_per_s\": {MAX_BG_IO_BYTES_PER_S},");
    let _ = writeln!(
        out,
        "  \"construction_budget_ns\": {CONSTRUCTION_BUDGET_NS},"
    );
    out.push_str("  \"budgets\": [\n");

    let mut verdicts = Vec::new();
    for (bi, (label, allowance)) in budgets.iter().enumerate() {
        let memory_budget = base_physical.saturating_add(*allowance);
        let controller = run_controller(&base, &source, &sched, memory_budget)?;
        let staticp = run_static(&base, &source, &sched, memory_budget)?;
        let tinylfu = run_tinylfu(&source, &sched, *allowance)?;
        let lru = run_codec_lru(&source, &sched, *allowance)?;
        let raw = run_rawvec(&source, &sched);

        println!(
            "  [{label}] controller warm p50 {:.0}/p99 {:.0} ns, cold p50 {:.0}/p99 {:.0} ns, spent {} B, {} adapt, {} SLO misses",
            controller.warm_p50_ns,
            controller.warm_p99_ns,
            controller.cold_p50_ns,
            controller.cold_p99_ns,
            controller.bytes_spent,
            controller.adaptations,
            controller.slo_misses
        );
        println!(
            "  [{label}] static warm p99 {:.0} | tinylfu warm p99 {:.0} | lru warm p99 {:.0} ns",
            staticp.warm_p99_ns, tinylfu.warm_p99_ns, lru.warm_p99_ns
        );

        out.push_str("    {\n");
        let _ = writeln!(out, "      \"label\": \"{label}\",");
        let _ = writeln!(out, "      \"allowance_bytes\": {allowance},");
        let _ = writeln!(out, "      \"memory_budget_bytes\": {memory_budget},");
        out.push_str("      \"contenders\": {\n");
        write_contender(&mut out, &controller, false);
        write_contender(&mut out, &staticp, false);
        write_contender(&mut out, &tinylfu, false);
        write_contender(&mut out, &lru, false);
        write_contender(&mut out, &raw, true);
        out.push_str("      },\n");

        let beats_static = controller.warm_p99_ns < staticp.warm_p99_ns;
        let beats_tinylfu = controller.warm_p99_ns < tinylfu.warm_p99_ns;
        let beats_lru = controller.warm_p99_ns < lru.warm_p99_ns;
        let saturates =
            controller.bytes_spent < (WINDOWS as u64) * (WINDOW_REGIONS as u64) * REGION_BYTES;
        let _ = writeln!(out, "      \"beats_static_warm_p99\": {beats_static},");
        let _ = writeln!(out, "      \"beats_tinylfu_warm_p99\": {beats_tinylfu},");
        let _ = writeln!(out, "      \"beats_lru_warm_p99\": {beats_lru},");
        let _ = writeln!(out, "      \"additive_budget_saturated\": {saturates},");
        let _ = writeln!(
            out,
            "      \"controller_adaptation_recovery_p50_ratio\": {:.3}",
            if controller.warm_p50_ns > 0.0 {
                controller.cold_p50_ns / controller.warm_p50_ns
            } else {
                0.0
            }
        );
        out.push_str(if bi + 1 == budgets.len() {
            "    }\n"
        } else {
            "    },\n"
        });
        verdicts.push((label.to_string(), beats_static, beats_tinylfu, beats_lru));
    }
    out.push_str("  ]\n}\n");

    let path = Path::new("evidence/bench/controller.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, out)?;
    println!("wrote {}", path.display());
    for (label, bs, bt, bl) in verdicts {
        println!(
            "  verdict [{label}]: beats static={bs}, beats TinyLFU={bt}, beats LRU={bl} (warm p99)"
        );
    }
    Ok(())
}
