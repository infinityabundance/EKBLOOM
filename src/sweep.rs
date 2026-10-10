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

use crate::error::Result;
use crate::fastpath::{self, CpuFeatures};
use crate::manifest::{create, EkbloomBody};
use crate::metrics::{now_ns, percentiles};
use crate::ManifestationProfile;
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
}
