//! Manifestation sweep across physical-expansion budgets.
//!
//! This module answers the governing question directly: **how much speed does a
//! given amount of manifested space buy?** For each profile in
//! [`sweep_profiles`] it builds an [`EkbloomBody`], records the manifested
//! physical/visible accounting, and measures read performance through the
//! optimised paths in [`crate::fastpath`]:
//!
//! - sequential throughput (bytes/s),
//! - random single-byte latency percentiles (ns),
//! - random 4 KiB latency (ns),
//! - 64 KiB range throughput (bytes/s).
//!
//! All timing uses [`crate::metrics::now_ns`] and [`crate::metrics::percentiles`]
//! (nearest-rank). The random workload uses a small deterministic LCG seeded from
//! [`SweepConfig::seed`], so every profile is measured against the *same* offset
//! sequence and a run is reproducible.
//!
//! Runtime is bounded: a 1 MiB source completes in a few seconds with the
//! default [`SweepConfig`]. Tests clamp `iterations` down further.

use crate::bloom::SUPPORTED_DEPTHS;
use crate::checkpoint::SUPPORTED_INTERVALS;
use crate::error::Result;
use crate::fastpath::{self, CpuFeatures};
use crate::manifest::{create, EkbloomBody};
use crate::metrics::{now_ns, percentiles};
use crate::reader;
use crate::{ManifestationProfile, ReadRequest};
use std::fmt::Write as _;
use std::path::Path;

/// One measured point on the speed-vs-manifested-space frontier.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepPoint {
    /// Profile label (e.g. `"path(D=4)"`, `"max-speed"`).
    pub profile: String,
    /// Total manifested physical bytes.
    pub physical_bytes: u64,
    /// Visible bytes (`core.len()`).
    pub visible_bytes: u64,
    /// Physical/visible ratio (`0.0` when there is no visible byte).
    pub expansion_ratio: f64,
    /// Wall-clock nanoseconds spent building the manifested body.
    pub build_ns: u64,
    /// Sequential read throughput (bytes/s) over the whole object.
    pub seq_bytes_per_s: f64,
    /// Median single-byte random read latency (ns).
    pub random_1b_ns_p50: f64,
    /// 99th-percentile single-byte random read latency (ns).
    pub random_1b_ns_p99: f64,
    /// Median 4 KiB random range read latency (ns).
    pub random_4k_ns_p50: f64,
    /// 64 KiB range read throughput (bytes/s).
    pub range_64k_bytes_per_s: f64,
    /// Number of single-byte random samples collected for this point.
    pub samples: u64,
}

/// A full sweep: the CPU, the source size, and one point per profile.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepReport {
    /// Detected CPU features (for interpreting the numbers).
    pub cpu: CpuFeatures,
    /// Source length in bytes.
    pub source_len: u64,
    /// One point per profile in [`sweep_profiles`] order.
    pub points: Vec<SweepPoint>,
}

/// Sweep configuration. The defaults are modest so a 1 MiB source runs in a few
/// seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepConfig {
    /// Repetition/sample budget; each workload derives a bounded count from it.
    pub iterations: u64,
    /// Seed for the deterministic random-offset generator.
    pub seed: u64,
}

impl Default for SweepConfig {
    fn default() -> Self {
        SweepConfig {
            iterations: 64,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// The profile frontier, ordered from least to most manifested space.
///
/// Covers `Root`, sparse restarts (`K=1024`, `K=64`), `Full`, a universal
/// `bloom(4)`, `regional(4, 4096)`, exact path blooms (`path(4)`, `path(8)`), and
/// the aggregate `MaxSpeed` configuration.
pub fn sweep_profiles() -> Vec<ManifestationProfile> {
    vec![
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 4096,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Path { depth: 8 },
        ManifestationProfile::MaxSpeed,
    ]
}

/// A tiny deterministic linear congruential generator (Numerical Recipes
/// constants), adequate for choosing benchmark offsets.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    /// A uniform value in `[0, n)`, or `0` when `n == 0`.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

/// A non-negative finite view of `v` (guards JSON validity and ratio math).
fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Build and measure one profile.
fn measure(source: &[u8], profile: ManifestationProfile, cfg: &SweepConfig) -> Result<SweepPoint> {
    let build_start = now_ns();
    let body: EkbloomBody = create(source, profile)?;
    let build_ns = now_ns().saturating_sub(build_start);

    let visible_bytes = body.visible_bytes();
    let physical_bytes = body.physical_bytes();
    let expansion_ratio = if visible_bytes == 0 {
        0.0
    } else {
        physical_bytes as f64 / visible_bytes as f64
    };
    let n = body.logical_len;
    let cap = usize::try_from(n).unwrap_or(0);

    // --- sequential throughput -------------------------------------------------
    let seq_reps = cfg.iterations.max(1);
    let mut seq_out: Vec<u8> = Vec::with_capacity(cap);
    let mut seq_bytes = 0u64;
    let seq_start = now_ns();
    for _ in 0..seq_reps {
        seq_out.clear();
        fastpath::read_sequential(&body, &mut seq_out)?;
        seq_bytes = seq_bytes.saturating_add(seq_out.len() as u64);
    }
    let seq_elapsed = now_ns().saturating_sub(seq_start).max(1);
    let seq_bytes_per_s = finite(seq_bytes as f64 * 1e9 / seq_elapsed as f64);

    // --- random single-byte latency -------------------------------------------
    let rand1_samples = cfg.iterations.saturating_mul(4).clamp(8, 4096);
    let mut rng = Lcg::new(cfg.seed);
    let mut one_out: Vec<u8> = Vec::with_capacity(1);
    let mut durs: Vec<u64> = Vec::with_capacity(usize::try_from(rand1_samples).unwrap_or(0));
    for _ in 0..rand1_samples {
        if n == 0 {
            break;
        }
        let offset = rng.below(n);
        one_out.clear();
        let a = now_ns();
        fastpath::read_random_1b(&body, &[offset], &mut one_out)?;
        let b = now_ns();
        durs.push(b.saturating_sub(a));
    }
    durs.sort_unstable();
    let p1 = percentiles(&durs);

    // --- random 4 KiB latency --------------------------------------------------
    let rand4k_samples = cfg.iterations.clamp(4, 256);
    let win4k = 4096u64.min(n);
    let mut rng = Lcg::new(cfg.seed ^ 0x00A5_A5A5_5A5A_5A5A);
    let mut out4k: Vec<u8> = Vec::with_capacity(usize::try_from(win4k).unwrap_or(0));
    let mut durs4k: Vec<u64> = Vec::with_capacity(usize::try_from(rand4k_samples).unwrap_or(0));
    for _ in 0..rand4k_samples {
        if n == 0 {
            break;
        }
        let offset = bounded_offset(&mut rng, n, win4k);
        out4k.clear();
        let a = now_ns();
        fastpath::read_ranges(&body, &[(offset, win4k)], &mut out4k)?;
        let b = now_ns();
        durs4k.push(b.saturating_sub(a));
    }
    durs4k.sort_unstable();
    let p4k = percentiles(&durs4k);

    // --- 64 KiB range throughput ----------------------------------------------
    let range_reps = cfg.iterations.max(1);
    let win64k = 65536u64.min(n);
    let mut rng = Lcg::new(cfg.seed ^ 0x0F0F_F0F0_3C3C_C3C3);
    let mut out64k: Vec<u8> = Vec::with_capacity(usize::try_from(win64k).unwrap_or(0));
    let mut range_bytes = 0u64;
    let range_start = now_ns();
    for _ in 0..range_reps {
        if n == 0 {
            break;
        }
        let offset = bounded_offset(&mut rng, n, win64k);
        out64k.clear();
        fastpath::read_ranges(&body, &[(offset, win64k)], &mut out64k)?;
        range_bytes = range_bytes.saturating_add(out64k.len() as u64);
    }
    let range_elapsed = now_ns().saturating_sub(range_start).max(1);
    let range_64k_bytes_per_s = finite(range_bytes as f64 * 1e9 / range_elapsed as f64);

    Ok(SweepPoint {
        profile: profile.label(),
        physical_bytes,
        visible_bytes,
        expansion_ratio,
        build_ns,
        seq_bytes_per_s,
        random_1b_ns_p50: p1.p50,
        random_1b_ns_p99: p1.p99,
        random_4k_ns_p50: p4k.p50,
        range_64k_bytes_per_s,
        samples: durs.len() as u64,
    })
}

/// A uniform offset in `[0, n - window]` (or `0` when the window fills `n`).
fn bounded_offset(rng: &mut Lcg, n: u64, window: u64) -> u64 {
    let span = n.saturating_sub(window);
    if span == 0 {
        0
    } else {
        rng.below(span.saturating_add(1)).min(span)
    }
}

/// Build and measure every profile in [`sweep_profiles`].
pub fn manifestation_sweep(source: &[u8], cfg: &SweepConfig) -> Result<SweepReport> {
    let cpu = fastpath::cpu_features();
    let mut points = Vec::new();
    for profile in sweep_profiles() {
        points.push(measure(source, profile, cfg)?);
    }
    Ok(SweepReport {
        cpu,
        source_len: source.len() as u64,
        points,
    })
}

/// Minimal JSON string escaping for the ASCII profile labels we emit.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Write a [`SweepReport`] as pretty JSON to `path`, creating parent directories.
pub fn write_evidence_json(path: &Path, report: &SweepReport) -> Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"cpu\": {{ \"sse2\": {}, \"avx2\": {}, \"avx512f\": {} }},",
        report.cpu.sse2, report.cpu.avx2, report.cpu.avx512f
    );
    let _ = writeln!(s, "  \"source_len\": {},", report.source_len);
    s.push_str("  \"points\": [\n");
    for (i, p) in report.points.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"profile\": \"{}\",", json_escape(&p.profile));
        let _ = writeln!(s, "      \"physical_bytes\": {},", p.physical_bytes);
        let _ = writeln!(s, "      \"visible_bytes\": {},", p.visible_bytes);
        let _ = writeln!(
            s,
            "      \"expansion_ratio\": {},",
            finite(p.expansion_ratio)
        );
        let _ = writeln!(s, "      \"build_ns\": {},", p.build_ns);
        let _ = writeln!(
            s,
            "      \"seq_bytes_per_s\": {},",
            finite(p.seq_bytes_per_s)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p50\": {},",
            finite(p.random_1b_ns_p50)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p99\": {},",
            finite(p.random_1b_ns_p99)
        );
        let _ = writeln!(
            s,
            "      \"random_4k_ns_p50\": {},",
            finite(p.random_4k_ns_p50)
        );
        let _ = writeln!(
            s,
            "      \"range_64k_bytes_per_s\": {},",
            finite(p.range_64k_bytes_per_s)
        );
        let _ = writeln!(s, "      \"samples\": {}", p.samples);
        s.push_str(if i + 1 == report.points.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, s)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Curves, budget frontier, and the large evidence report.
// ---------------------------------------------------------------------------

/// Minimum physical-byte floor applied to a budget: `max(visible * ratio, floor)`.
pub const BUDGET_MIN_FLOOR_BYTES: u64 = 4096;

/// Target `physical/visible` ratios used by [`large_sweep`], least to most space.
pub const DEFAULT_BUDGET_RATIOS: [f64; 8] = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0];

/// Region sizes (logical positions) swept by [`region_size_curve`].
pub const CURVE_REGION_SIZES: [u32; 5] = [256, 1024, 4096, 16384, 65536];

/// One row of the budget frontier: for a target `M/N` ratio, the fastest valid
/// profile whose physical footprint fits the budget, and its measured speed.
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetPoint {
    /// Requested target `physical/visible` ratio.
    pub target_ratio: f64,
    /// Label of the chosen profile (`""` when nothing fits the budget).
    pub chosen_profile: String,
    /// Physical bytes of the chosen profile (0 when nothing fits).
    pub physical_bytes: u64,
    /// Achieved `physical/visible` ratio (`0.0` when nothing fits).
    pub achieved_ratio: f64,
    /// Sequential read throughput of the chosen profile (bytes/s).
    pub seq_bytes_per_s: f64,
    /// Median single-byte random read latency (ns).
    pub random_1b_ns_p50: f64,
    /// Median 4 KiB random range read latency (ns).
    pub random_4k_ns_p50: f64,
}

/// A labelled curve: one measured [`SweepPoint`] per axis label.
#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    /// Axis name (e.g. `"region-size"`).
    pub axis: String,
    /// One label per point, in `points` order.
    pub labels: Vec<String>,
    /// The measured points.
    pub points: Vec<SweepPoint>,
}

/// The full evidence of a [`large_sweep`] run.
#[derive(Clone, Debug, PartialEq)]
pub struct LargeSweepReport {
    /// Detected CPU features.
    pub cpu: CpuFeatures,
    /// Source length in bytes.
    pub source_len: u64,
    /// BLAKE3 hash of the exact source used.
    pub source_hash: [u8; 32],
    /// The budget frontier, one row per requested ratio.
    pub budget_frontier: Vec<BudgetPoint>,
    /// The measured curves (region size, depth, checkpoint interval).
    pub curves: Vec<Curve>,
    /// Cold-start (open + first 4 KiB read) nanoseconds per profile.
    pub cold_start: Vec<(String, f64)>,
    /// Cache-resident sequential throughput (bytes/s).
    pub cache_resident_seq_bytes_per_s: f64,
}

/// Tolerance, in octaves (factors of two), within which two profiles are treated
/// as tied for speed.
///
/// Profile selection must not depend on run-to-run timing noise. Measured speeds
/// whose combined score is within `2^SPEED_TOLERANCE_OCTAVES` are therefore a tie,
/// resolved by the (noise-free) smaller physical footprint and then the label. A
/// one-octave (twofold) window is wide enough to absorb benchmark noise while
/// still separating the frontier's real steps (which are many octaves apart).
const SPEED_TOLERANCE_OCTAVES: f64 = 1.0;

/// `log2` of a positive finite value, or a very low floor (so a missing metric
/// neither dominates nor panics).
fn lg(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        v.log2()
    } else {
        -60.0
    }
}

/// A single speed score in octave space: throughput helps, latency hurts.
fn speed_score(p: &SweepPoint) -> f64 {
    lg(p.seq_bytes_per_s) + lg(p.range_64k_bytes_per_s)
        - lg(p.random_1b_ns_p50)
        - lg(p.random_4k_ns_p50)
}

/// Whether `a` is strictly preferred over `b`: clearly faster, otherwise smaller
/// footprint, otherwise the lexicographically smaller profile label.
fn faster(a: &SweepPoint, b: &SweepPoint) -> bool {
    let sa = speed_score(a);
    let sb = speed_score(b);
    if sa - sb > SPEED_TOLERANCE_OCTAVES {
        return true;
    }
    if sb - sa > SPEED_TOLERANCE_OCTAVES {
        return false;
    }
    if a.physical_bytes != b.physical_bytes {
        return a.physical_bytes < b.physical_bytes;
    }
    a.profile < b.profile
}

/// The candidate profiles considered when filling the budget frontier.
fn budget_candidates() -> Vec<ManifestationProfile> {
    let mut v = vec![ManifestationProfile::Root];
    for interval in SUPPORTED_INTERVALS {
        v.push(ManifestationProfile::Sparse { interval });
    }
    v.push(ManifestationProfile::Full);
    v.push(ManifestationProfile::Bloom { depth: 4 });
    v.push(ManifestationProfile::Regional {
        depth: 4,
        region_bytes: 4096,
    });
    for depth in SUPPORTED_DEPTHS {
        v.push(ManifestationProfile::Path { depth });
    }
    v.push(ManifestationProfile::MaxSpeed);
    v
}

/// The physical-byte budget for `visible` bytes at `ratio`:
/// `max(visible * ratio, `[`BUDGET_MIN_FLOOR_BYTES`]`)`.
fn budget_bytes(visible: u64, ratio: f64) -> u64 {
    let r = if ratio.is_finite() && ratio > 0.0 {
        ratio
    } else {
        0.0
    };
    let scaled = visible as f64 * r;
    let cap = if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    };
    cap.max(BUDGET_MIN_FLOOR_BYTES)
}

/// For each target `M/N` ratio in `ratios`, build every candidate profile, measure
/// it, and select the **fastest valid** profile whose `physical_bytes` fits the
/// budget `max(visible * ratio, `[`BUDGET_MIN_FLOOR_BYTES`]`)`.
///
/// Selection is honest: every candidate is really built and measured, and only a
/// profile whose physical footprint is within budget is eligible. When nothing
/// fits, `chosen_profile` is `""` and the metrics are zero. Measured speeds within
/// a factor of `2^SPEED_TOLERANCE_OCTAVES` are treated as a tie; ties go to the
/// smaller physical footprint and then the profile label, so the result is
/// deterministic for a fixed seed.
pub fn budget_frontier(
    source: &[u8],
    ratios: &[f64],
    cfg: &SweepConfig,
) -> Result<Vec<BudgetPoint>> {
    let visible = source.len() as u64;

    let mut measured: Vec<SweepPoint> = Vec::new();
    for profile in budget_candidates() {
        measured.push(measure(source, profile, cfg)?);
    }

    let mut out = Vec::with_capacity(ratios.len());
    for &target_ratio in ratios {
        let budget = budget_bytes(visible, target_ratio);
        let mut best: Option<&SweepPoint> = None;
        for p in &measured {
            if p.physical_bytes > budget {
                continue;
            }
            match best {
                None => best = Some(p),
                Some(b) if faster(p, b) => best = Some(p),
                Some(_) => {}
            }
        }
        out.push(match best {
            Some(p) => BudgetPoint {
                target_ratio,
                chosen_profile: p.profile.clone(),
                physical_bytes: p.physical_bytes,
                achieved_ratio: if visible == 0 {
                    0.0
                } else {
                    finite(p.physical_bytes as f64 / visible as f64)
                },
                seq_bytes_per_s: finite(p.seq_bytes_per_s),
                random_1b_ns_p50: finite(p.random_1b_ns_p50),
                random_4k_ns_p50: finite(p.random_4k_ns_p50),
            },
            None => BudgetPoint {
                target_ratio,
                chosen_profile: String::new(),
                physical_bytes: 0,
                achieved_ratio: 0.0,
                seq_bytes_per_s: 0.0,
                random_1b_ns_p50: 0.0,
                random_4k_ns_p50: 0.0,
            },
        });
    }
    Ok(out)
}

/// Sweep the regional `T_4^(reg)[x]` construction over `region_sizes` (logical
/// positions per region); one [`SweepPoint`] per requested size.
pub fn region_size_curve(source: &[u8], cfg: &SweepConfig, region_sizes: &[u32]) -> Result<Curve> {
    let mut labels = Vec::with_capacity(region_sizes.len());
    let mut points = Vec::with_capacity(region_sizes.len());
    for &region_bytes in region_sizes {
        labels.push(format!("R={region_bytes}"));
        points.push(measure(
            source,
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes,
            },
            cfg,
        )?);
    }
    Ok(Curve {
        axis: "region-size".to_string(),
        labels,
        points,
    })
}

/// Sweep the exact path bloom `P_D[j]` over `depths`; one [`SweepPoint`] per depth.
pub fn depth_curve(source: &[u8], cfg: &SweepConfig, depths: &[u16]) -> Result<Curve> {
    let mut labels = Vec::with_capacity(depths.len());
    let mut points = Vec::with_capacity(depths.len());
    for &depth in depths {
        labels.push(format!("D={depth}"));
        points.push(measure(source, ManifestationProfile::Path { depth }, cfg)?);
    }
    Ok(Curve {
        axis: "path-depth".to_string(),
        labels,
        points,
    })
}

/// Sweep sparse checkpoints `C_K` over intervals `K`; one [`SweepPoint`] per
/// interval (each must be a member of `SUPPORTED_INTERVALS`).
pub fn checkpoint_interval_curve(
    source: &[u8],
    cfg: &SweepConfig,
    intervals: &[u32],
) -> Result<Curve> {
    let mut labels = Vec::with_capacity(intervals.len());
    let mut points = Vec::with_capacity(intervals.len());
    for &interval in intervals {
        labels.push(format!("K={interval}"));
        points.push(measure(
            source,
            ManifestationProfile::Sparse { interval },
            cfg,
        )?);
    }
    Ok(Curve {
        axis: "checkpoint-interval".to_string(),
        labels,
        points,
    })
}

/// Cold-start nanoseconds: time to open the object (build/encode the manifested
/// body) and perform the first 4 KiB read. A single cold sample; always `>= 0`.
pub fn cold_start_ns(
    source: &[u8],
    profile: ManifestationProfile,
    cfg: &SweepConfig,
) -> Result<f64> {
    // `cfg` is accepted for API symmetry; a cold start is a single measurement.
    let _ = cfg;
    let start = now_ns();
    let body = create(source, profile)?;
    let window = 4096u64.min(body.logical_len);
    if window > 0 {
        let _ = reader::read(
            &body,
            ReadRequest {
                offset: 0,
                length: window,
            },
        )?;
    }
    let end = now_ns();
    Ok(end.saturating_sub(start) as f64)
}

/// A deterministic LCG byte corpus (useful to size-independent measurements).
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

/// Sequential throughput for a small, cache-resident corpus of `corpus_len`
/// bytes. The corpus is encoded with an exact depth-32 path bloom, warmed once,
/// then read sequentially many times.
pub fn cache_resident_seq_bytes_per_s(corpus_len: usize, cfg: &SweepConfig) -> Result<f64> {
    let source = lcg_bytes(cfg.seed ^ 0x00C0_FFEE_1234_5678, corpus_len);
    let body = create(&source, ManifestationProfile::Path { depth: 32 })?;

    let cap = usize::try_from(body.logical_len).unwrap_or(0);
    let mut out: Vec<u8> = Vec::with_capacity(cap);
    // Warm up so the first-touch page faults are not counted.
    fastpath::read_sequential(&body, &mut out)?;
    out.clear();

    let reps = cfg.iterations.max(16).saturating_mul(8).min(100_000);
    let mut bytes: u64 = 0;
    let start = now_ns();
    for _ in 0..reps {
        out.clear();
        fastpath::read_sequential(&body, &mut out)?;
        bytes = bytes.saturating_add(out.len() as u64);
    }
    let elapsed = now_ns().saturating_sub(start).max(1);
    Ok(finite(bytes as f64 * 1e9 / elapsed as f64))
}

/// Run the extended manifestation sweep: the budget frontier, the region-size /
/// depth / checkpoint-interval curves, cold starts, and cache-resident throughput.
pub fn large_sweep(source: &[u8], cfg: &SweepConfig) -> Result<LargeSweepReport> {
    let cpu = fastpath::cpu_features();
    let source_hash = *blake3::hash(source).as_bytes();

    let frontier = budget_frontier(source, &DEFAULT_BUDGET_RATIOS, cfg)?;

    let curves = vec![
        region_size_curve(source, cfg, &CURVE_REGION_SIZES)?,
        depth_curve(source, cfg, &SUPPORTED_DEPTHS)?,
        checkpoint_interval_curve(source, cfg, &SUPPORTED_INTERVALS)?,
    ];

    let cold_profiles = [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Path { depth: 32 },
        ManifestationProfile::MaxSpeed,
    ];
    let mut cold_start = Vec::with_capacity(cold_profiles.len());
    for profile in cold_profiles {
        let ns = cold_start_ns(source, profile, cfg)?;
        cold_start.push((profile.label(), ns));
    }

    let cache = cache_resident_seq_bytes_per_s(64 * 1024, cfg)?;

    Ok(LargeSweepReport {
        cpu,
        source_len: source.len() as u64,
        source_hash,
        budget_frontier: frontier,
        curves,
        cold_start,
        cache_resident_seq_bytes_per_s: cache,
    })
}

/// Lowercase hex of a 32-byte digest.
fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Serialize a [`SweepPoint`] as a JSON object (no trailing comma/newline).
fn point_json(p: &SweepPoint, pad: &str) -> String {
    let mut s = String::new();
    let inner = format!("{pad}  ");
    let _ = writeln!(s, "{pad}{{");
    let _ = writeln!(s, "{inner}\"profile\": \"{}\",", json_escape(&p.profile));
    let _ = writeln!(s, "{inner}\"physical_bytes\": {},", p.physical_bytes);
    let _ = writeln!(s, "{inner}\"visible_bytes\": {},", p.visible_bytes);
    let _ = writeln!(
        s,
        "{inner}\"expansion_ratio\": {},",
        finite(p.expansion_ratio)
    );
    let _ = writeln!(s, "{inner}\"build_ns\": {},", p.build_ns);
    let _ = writeln!(
        s,
        "{inner}\"seq_bytes_per_s\": {},",
        finite(p.seq_bytes_per_s)
    );
    let _ = writeln!(
        s,
        "{inner}\"random_1b_ns_p50\": {},",
        finite(p.random_1b_ns_p50)
    );
    let _ = writeln!(
        s,
        "{inner}\"random_1b_ns_p99\": {},",
        finite(p.random_1b_ns_p99)
    );
    let _ = writeln!(
        s,
        "{inner}\"random_4k_ns_p50\": {},",
        finite(p.random_4k_ns_p50)
    );
    let _ = writeln!(
        s,
        "{inner}\"range_64k_bytes_per_s\": {},",
        finite(p.range_64k_bytes_per_s)
    );
    let _ = writeln!(s, "{inner}\"samples\": {}", p.samples);
    let _ = write!(s, "{pad}}}");
    s
}

/// Write a [`LargeSweepReport`] as pretty JSON to `path`, creating parent dirs.
pub fn write_large_evidence_json(path: &Path, report: &LargeSweepReport) -> Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"cpu\": {{ \"sse2\": {}, \"avx2\": {}, \"avx512f\": {} }},",
        report.cpu.sse2, report.cpu.avx2, report.cpu.avx512f
    );
    let _ = writeln!(s, "  \"source_len\": {},", report.source_len);
    let _ = writeln!(s, "  \"source_hash\": \"{}\",", hex32(&report.source_hash));

    s.push_str("  \"budget_frontier\": [\n");
    for (i, bp) in report.budget_frontier.iter().enumerate() {
        let comma = if i + 1 == report.budget_frontier.len() {
            ""
        } else {
            ","
        };
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"target_ratio\": {},", bp.target_ratio);
        let _ = writeln!(
            s,
            "      \"chosen_profile\": \"{}\",",
            json_escape(&bp.chosen_profile)
        );
        let _ = writeln!(s, "      \"physical_bytes\": {},", bp.physical_bytes);
        let _ = writeln!(
            s,
            "      \"achieved_ratio\": {},",
            finite(bp.achieved_ratio)
        );
        let _ = writeln!(
            s,
            "      \"seq_bytes_per_s\": {},",
            finite(bp.seq_bytes_per_s)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p50\": {},",
            finite(bp.random_1b_ns_p50)
        );
        let _ = writeln!(
            s,
            "      \"random_4k_ns_p50\": {}",
            finite(bp.random_4k_ns_p50)
        );
        let _ = writeln!(s, "    }}{comma}");
    }
    s.push_str("  ],\n");

    s.push_str("  \"curves\": [\n");
    for (ci, curve) in report.curves.iter().enumerate() {
        let comma = if ci + 1 == report.curves.len() {
            ""
        } else {
            ","
        };
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"axis\": \"{}\",", json_escape(&curve.axis));
        s.push_str("      \"labels\": [");
        for (li, label) in curve.labels.iter().enumerate() {
            if li > 0 {
                s.push_str(", ");
            }
            let _ = write!(s, "\"{}\"", json_escape(label));
        }
        s.push_str("],\n");
        s.push_str("      \"points\": [\n");
        for (pi, p) in curve.points.iter().enumerate() {
            let pcomma = if pi + 1 == curve.points.len() {
                ""
            } else {
                ","
            };
            s.push_str(&point_json(p, "        "));
            s.push_str(pcomma);
            s.push('\n');
        }
        s.push_str("      ]\n");
        let _ = writeln!(s, "    }}{comma}");
    }
    s.push_str("  ],\n");

    s.push_str("  \"cold_start\": [\n");
    for (i, (profile, ns)) in report.cold_start.iter().enumerate() {
        let comma = if i + 1 == report.cold_start.len() {
            ""
        } else {
            ","
        };
        let _ = writeln!(
            s,
            "    {{ \"profile\": \"{}\", \"ns\": {} }}{comma}",
            json_escape(profile),
            finite(*ns)
        );
    }
    s.push_str("  ],\n");

    let _ = writeln!(
        s,
        "  \"cache_resident_seq_bytes_per_s\": {}",
        finite(report.cache_resident_seq_bytes_per_s)
    );
    s.push_str("}\n");

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, s)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-depth metrics (§10) and memory-tier manifestation (O9).
// ---------------------------------------------------------------------------

/// Exact key semantics of every path bloom: the key is the block ordinal `j`.
pub const PATH_KEY_SEMANTICS: &str = "block ordinal j";
/// Successor semantics of every path bloom: `cold[j] = x_{D*(j+1)}` (a final
/// partial block stores `x_N`).
pub const PATH_SUCCESSOR_SEMANTICS: &str = "cold[j] = x_{D*(j+1)}";
/// Certification of every path bloom: exact by construction, never rejected.
pub const PATH_CERTIFICATION: &str = "exact-by-construction";

/// Nominal L1 data-cache size (bytes) targeted by [`DEFAULT_MEMORY_TIERS`].
pub const TIER_L1_BYTES: u64 = 32 * 1024;
/// Nominal L2 cache size (bytes) targeted by [`DEFAULT_MEMORY_TIERS`].
pub const TIER_L2_BYTES: u64 = 1024 * 1024;
/// Nominal last-level cache size (bytes) targeted by [`DEFAULT_MEMORY_TIERS`].
pub const TIER_LLC_BYTES: u64 = 32 * 1024 * 1024;
/// DRAM-resident corpus size (bytes) targeted by [`DEFAULT_MEMORY_TIERS`]; an
/// order of magnitude beyond the LLC so the working set cannot be cache-resident.
pub const TIER_DRAM_BYTES: u64 = 256 * 1024 * 1024;

/// The default memory-tier ladder (label, nominal cache size, corpus bytes).
pub const DEFAULT_MEMORY_TIERS: [(&str, u64, usize); 4] = [
    ("L1", TIER_L1_BYTES, TIER_L1_BYTES as usize),
    ("L2", TIER_L2_BYTES, TIER_L2_BYTES as usize),
    ("LLC", TIER_LLC_BYTES, TIER_LLC_BYTES as usize),
    ("DRAM", TIER_DRAM_BYTES, TIER_DRAM_BYTES as usize),
];

/// A one-line cache-behaviour note for a working set of `working_set` bytes.
fn cache_note(working_set: u64) -> String {
    if working_set == 0 {
        "empty working set".to_string()
    } else if working_set <= TIER_L1_BYTES {
        format!("hot array {working_set} B fits L1 (32 KiB): L1-bandwidth bound")
    } else if working_set <= TIER_L2_BYTES {
        format!("hot array {working_set} B fits L2 (1 MiB): L2-bandwidth bound")
    } else if working_set <= TIER_LLC_BYTES {
        format!("hot array {working_set} B fits LLC (32 MiB): LLC-bandwidth bound")
    } else {
        format!("hot array {working_set} B is DRAM-resident: memory-bandwidth bound")
    }
}

/// One row of the per-depth metrics table: fixed semantics plus the measured
/// construction cost and read speeds for a path bloom at depth `D`.
#[derive(Clone, Debug, PartialEq)]
pub struct DepthPoint {
    /// Path-bloom depth `D`.
    pub depth: u16,
    /// Exact key semantics (constant across depths).
    pub key_semantics: String,
    /// Successor semantics (constant across depths).
    pub successor_semantics: String,
    /// Certification status (constant across depths).
    pub certification: String,
    /// Manifested physical bytes.
    pub physical_bytes: u64,
    /// Wall-clock construction time in nanoseconds.
    pub build_ns: u64,
    /// Sequential read throughput (bytes/s).
    pub seq_bytes_per_s: f64,
    /// Median single-byte random read latency (ns).
    pub random_1b_ns_p50: f64,
    /// Cache-behaviour note for this depth's read surface.
    pub cache_note: String,
}

/// Per-depth metrics over the supported path-bloom depths (§10).
#[derive(Clone, Debug, PartialEq)]
pub struct DepthMetrics {
    /// Detected CPU features.
    pub cpu: CpuFeatures,
    /// Source length in bytes.
    pub source_len: u64,
    /// BLAKE3 hash of the exact source.
    pub source_hash: [u8; 32],
    /// One point per supported depth, in `SUPPORTED_DEPTHS` order.
    pub points: Vec<DepthPoint>,
}

/// Measure construction time and read speed for the exact path bloom at every
/// supported depth (`D` in `{1,2,4,8,16,32}`).
///
/// Every row has identical, exact semantics (`key = block ordinal j`,
/// `cold[j] = x_{D*(j+1)}`, certified by construction); only the manifested
/// footprint and the measured cost change with depth.
pub fn depth_metrics(source: &[u8], cfg: &SweepConfig) -> Result<DepthMetrics> {
    let cpu = fastpath::cpu_features();
    let mut points = Vec::with_capacity(SUPPORTED_DEPTHS.len());
    for &depth in SUPPORTED_DEPTHS.iter() {
        let p = measure(source, ManifestationProfile::Path { depth }, cfg)?;
        points.push(DepthPoint {
            depth,
            key_semantics: PATH_KEY_SEMANTICS.to_string(),
            successor_semantics: PATH_SUCCESSOR_SEMANTICS.to_string(),
            certification: PATH_CERTIFICATION.to_string(),
            physical_bytes: p.physical_bytes,
            build_ns: p.build_ns,
            seq_bytes_per_s: finite(p.seq_bytes_per_s),
            random_1b_ns_p50: finite(p.random_1b_ns_p50),
            cache_note: cache_note(p.visible_bytes),
        });
    }
    Ok(DepthMetrics {
        cpu,
        source_len: source.len() as u64,
        source_hash: *blake3::hash(source).as_bytes(),
        points,
    })
}

/// Write a [`DepthMetrics`] report as pretty JSON to `path`, creating parents.
pub fn write_depth_metrics_json(path: &Path, report: &DepthMetrics) -> Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"cpu\": {{ \"sse2\": {}, \"avx2\": {}, \"avx512f\": {} }},",
        report.cpu.sse2, report.cpu.avx2, report.cpu.avx512f
    );
    let _ = writeln!(s, "  \"source_len\": {},", report.source_len);
    let _ = writeln!(s, "  \"source_hash\": \"{}\",", hex32(&report.source_hash));
    s.push_str("  \"points\": [\n");
    for (i, p) in report.points.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"depth\": {},", p.depth);
        let _ = writeln!(
            s,
            "      \"key_semantics\": \"{}\",",
            json_escape(&p.key_semantics)
        );
        let _ = writeln!(
            s,
            "      \"successor_semantics\": \"{}\",",
            json_escape(&p.successor_semantics)
        );
        let _ = writeln!(
            s,
            "      \"certification\": \"{}\",",
            json_escape(&p.certification)
        );
        let _ = writeln!(s, "      \"physical_bytes\": {},", p.physical_bytes);
        let _ = writeln!(s, "      \"build_ns\": {},", p.build_ns);
        let _ = writeln!(
            s,
            "      \"seq_bytes_per_s\": {},",
            finite(p.seq_bytes_per_s)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p50\": {},",
            finite(p.random_1b_ns_p50)
        );
        let _ = writeln!(
            s,
            "      \"cache_note\": \"{}\"",
            json_escape(&p.cache_note)
        );
        s.push_str(if i + 1 == report.points.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ]\n}\n");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, s)?;
    Ok(())
}

/// One row of the memory-tier curve: a path-bloom corpus sized to a cache tier
/// (`label`, nominal cache size) and its measured sequential + random-1b speed.
#[derive(Clone, Debug, PartialEq)]
pub struct TierPoint {
    /// Tier label (`"L1"`, `"L2"`, `"LLC"`, `"DRAM"`).
    pub tier: String,
    /// Nominal cache size targeted by this tier, in bytes.
    pub target_bytes: u64,
    /// Actual corpus length `N` in bytes.
    pub corpus_bytes: u64,
    /// Manifested physical bytes (a depth-32 exact path bloom).
    pub physical_bytes: u64,
    /// Sequential read throughput (bytes/s).
    pub seq_bytes_per_s: f64,
    /// Median single-byte random read latency (ns).
    pub random_1b_ns_p50: f64,
    /// 99th-percentile single-byte random read latency (ns).
    pub random_1b_ns_p99: f64,
    /// Median 4 KiB random range read latency (ns).
    pub random_4k_ns_p50: f64,
    /// Number of single-byte samples.
    pub samples: u64,
    /// Cache-behaviour note for this tier's working set.
    pub cache_note: String,
}

/// The measured memory-tier curve (O9).
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryTiersReport {
    /// Detected CPU features.
    pub cpu: CpuFeatures,
    /// BLAKE3 hash of the subject corpus (the largest DRAM-tier corpus).
    pub source_hash: [u8; 32],
    /// One point per tier, in the order supplied.
    pub tiers: Vec<TierPoint>,
}

/// Build an exact depth-32 path bloom for a deterministic LCG corpus of each
/// `(label, nominal_bytes)` tier and measure its sequential + random-1b speed.
///
/// The corpus is sized to the *nominal cache size* of the tier (L1 32 KiB, L2
/// 1 MiB, LLC 32 MiB, DRAM 256 MiB by default), so the path-bloom hot array is
/// the intended working set. Each body is dropped before the next tier is built,
/// so peak memory stays at one tier.
pub fn memory_tiers(tiers: &[(&str, u64, usize)], cfg: &SweepConfig) -> Result<MemoryTiersReport> {
    let cpu = fastpath::cpu_features();
    let mut points = Vec::with_capacity(tiers.len());
    let mut source_hash = [0u8; 32];
    for &(label, target_bytes, corpus_bytes) in tiers {
        let source = lcg_bytes(cfg.seed ^ 0x00C0_FFEE_1234_5678, corpus_bytes);
        source_hash = *blake3::hash(&source).as_bytes();
        let p = measure(&source, ManifestationProfile::Path { depth: 32 }, cfg)?;
        points.push(TierPoint {
            tier: label.to_string(),
            target_bytes,
            corpus_bytes: p.visible_bytes,
            physical_bytes: p.physical_bytes,
            seq_bytes_per_s: finite(p.seq_bytes_per_s),
            random_1b_ns_p50: finite(p.random_1b_ns_p50),
            random_1b_ns_p99: finite(p.random_1b_ns_p99),
            random_4k_ns_p50: finite(p.random_4k_ns_p50),
            samples: p.samples,
            cache_note: cache_note(p.visible_bytes),
        });
    }
    Ok(MemoryTiersReport {
        cpu,
        source_hash,
        tiers: points,
    })
}

/// Write a [`MemoryTiersReport`] as pretty JSON to `path`, creating parents.
pub fn write_memory_tiers_json(path: &Path, report: &MemoryTiersReport) -> Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"cpu\": {{ \"sse2\": {}, \"avx2\": {}, \"avx512f\": {} }},",
        report.cpu.sse2, report.cpu.avx2, report.cpu.avx512f
    );
    let _ = writeln!(s, "  \"source_hash\": \"{}\",", hex32(&report.source_hash));
    s.push_str("  \"tiers\": [\n");
    for (i, p) in report.tiers.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"tier\": \"{}\",", json_escape(&p.tier));
        let _ = writeln!(s, "      \"target_bytes\": {},", p.target_bytes);
        let _ = writeln!(s, "      \"corpus_bytes\": {},", p.corpus_bytes);
        let _ = writeln!(s, "      \"physical_bytes\": {},", p.physical_bytes);
        let _ = writeln!(
            s,
            "      \"seq_bytes_per_s\": {},",
            finite(p.seq_bytes_per_s)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p50\": {},",
            finite(p.random_1b_ns_p50)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p99\": {},",
            finite(p.random_1b_ns_p99)
        );
        let _ = writeln!(
            s,
            "      \"random_4k_ns_p50\": {},",
            finite(p.random_4k_ns_p50)
        );
        let _ = writeln!(s, "      \"samples\": {},", p.samples);
        let _ = writeln!(
            s,
            "      \"cache_note\": \"{}\"",
            json_escape(&p.cache_note)
        );
        s.push_str(if i + 1 == report.tiers.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ]\n}\n");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn profiles_cover_the_documented_frontier() {
        let ps = sweep_profiles();
        assert!(ps.contains(&ManifestationProfile::Root));
        assert!(ps.contains(&ManifestationProfile::MaxSpeed));
        assert!(ps
            .iter()
            .any(|p| matches!(p, ManifestationProfile::Path { depth: 4 })));
        assert!(ps
            .iter()
            .any(|p| matches!(p, ManifestationProfile::Path { depth: 8 })));
        assert_eq!(ps.len(), 9);
    }

    #[test]
    fn sweep_reports_a_finite_positive_frontier() {
        let src = lcg(0x1234_ABCD, 256 * 1024);
        let cfg = SweepConfig {
            iterations: 8,
            seed: 0x42,
        };
        let report = manifestation_sweep(&src, &cfg).unwrap();
        assert_eq!(report.source_len, src.len() as u64);
        assert_eq!(report.points.len(), sweep_profiles().len());
        for p in &report.points {
            assert!(p.physical_bytes > 0, "{} has no physical bytes", p.profile);
            assert!(p.visible_bytes > 0, "{} has no visible bytes", p.profile);
            assert!(p.seq_bytes_per_s.is_finite() && p.seq_bytes_per_s > 0.0);
            assert!(
                p.range_64k_bytes_per_s.is_finite() && p.range_64k_bytes_per_s > 0.0,
                "{} range throughput",
                p.profile
            );
            assert!(p.random_1b_ns_p50.is_finite());
            assert!(p.random_1b_ns_p99.is_finite());
            assert!(p.random_4k_ns_p50.is_finite());
            assert!(p.samples > 0);
            let expect = p.physical_bytes as f64 / p.visible_bytes as f64;
            assert!(
                (p.expansion_ratio - expect).abs() < 1e-9,
                "{} expansion ratio {} != {}",
                p.profile,
                p.expansion_ratio,
                expect
            );
        }
    }

    #[test]
    fn evidence_json_is_written_and_well_formed() {
        let src = lcg(7, 8192);
        let cfg = SweepConfig {
            iterations: 2,
            seed: 1,
        };
        let report = manifestation_sweep(&src, &cfg).unwrap();
        let dir = std::env::temp_dir().join(format!("ekbloom-sweep-{}", std::process::id()));
        let path = dir.join("nested/sweep.json");
        write_evidence_json(&path, &report).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"cpu\""));
        assert!(text.contains("\"source_len\""));
        assert!(text.contains("\"points\""));
        assert!(text.contains("\"seq_bytes_per_s\""));
        // Balanced braces and no NaN/inf tokens.
        assert_eq!(
            text.matches('{').count(),
            text.matches('}').count(),
            "unbalanced braces"
        );
        assert!(!text.contains("NaN") && !text.contains("inf"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn depth_metrics_cover_all_depths_and_shrink_with_depth() {
        let src = lcg(0x0DEF_0000_0001, 64 * 1024);
        let cfg = SweepConfig {
            iterations: 2,
            seed: 3,
        };
        let m = depth_metrics(&src, &cfg).unwrap();
        assert_eq!(m.points.len(), SUPPORTED_DEPTHS.len());
        assert_eq!(m.source_len, src.len() as u64);
        let mut prev = u64::MAX;
        for (i, p) in m.points.iter().enumerate() {
            assert_eq!(p.depth, SUPPORTED_DEPTHS[i]);
            assert_eq!(p.key_semantics, PATH_KEY_SEMANTICS);
            assert_eq!(p.successor_semantics, PATH_SUCCESSOR_SEMANTICS);
            assert_eq!(p.certification, PATH_CERTIFICATION);
            assert!(p.physical_bytes > 0);
            assert!(
                p.physical_bytes <= prev,
                "depth {} physical {} exceeds previous {}",
                p.depth,
                p.physical_bytes,
                prev
            );
            prev = p.physical_bytes;
            assert!(p.seq_bytes_per_s.is_finite() && p.seq_bytes_per_s > 0.0);
            assert!(p.random_1b_ns_p50.is_finite());
            assert!(!p.cache_note.is_empty());
        }
    }

    #[test]
    fn memory_tiers_are_finite_and_positive() {
        let cfg = SweepConfig {
            iterations: 2,
            seed: 5,
        };
        let tiers = [
            ("L1", 16 * 1024u64, 16 * 1024usize),
            ("L2ish", 128 * 1024u64, 128 * 1024usize),
        ];
        let m = memory_tiers(&tiers, &cfg).unwrap();
        assert_eq!(m.tiers.len(), 2);
        for t in &m.tiers {
            assert!(t.corpus_bytes > 0);
            assert!(t.physical_bytes >= t.corpus_bytes);
            assert!(t.seq_bytes_per_s.is_finite() && t.seq_bytes_per_s > 0.0);
            assert!(t.random_1b_ns_p50.is_finite());
            assert!(t.random_4k_ns_p50.is_finite());
            assert!(t.samples > 0);
            assert!(!t.cache_note.is_empty());
        }
    }

    #[test]
    fn depth_metrics_json_is_well_formed() {
        let src = lcg(7, 8192);
        let cfg = SweepConfig {
            iterations: 1,
            seed: 1,
        };
        let m = depth_metrics(&src, &cfg).unwrap();
        let dir = std::env::temp_dir().join(format!("ekbloom-depth-{}", std::process::id()));
        let path = dir.join("depth-metrics.json");
        write_depth_metrics_json(&path, &m).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"depth\""));
        assert!(text.contains("\"cache_note\""));
        assert_eq!(text.matches('{').count(), text.matches('}').count());
        assert!(!text.contains("NaN") && !text.contains("inf"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
