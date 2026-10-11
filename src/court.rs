//! Rigorous performance court (directive §12).
//!
//! The sweep in [`crate::sweep`] reports *single* measurements per point. The
//! court is deliberately stricter and is built to be hard to fool:
//!
//! - working sets are sized around **real cache transitions** (L1 16/32/64 KiB,
//!   L2 256/512 KiB, 1/2 MiB, LLC 8/32/64/80/96/128 MiB, 256 MiB / 1 GiB) rather
//!   than nominal labels;
//! - every workload is measured with **multiple fresh repetitions** in an
//!   **interleaved, seed-randomised candidate order**;
//! - results are reported as **distributions with 95% confidence intervals**
//!   (bootstrap over the pooled per-operation samples), never a single sweep;
//! - a **raw-byte direct-access baseline at identical residency** is included for
//!   every workload;
//! - sub-100 ns operations are measured in **large batches** with the loop/clock
//!   overhead calibrated out, and every produced byte is observed (via
//!   [`std::hint::black_box`]) so the compiler cannot elide the access.
//!
//! Cycle/byte is reported using a **dependent-add-chain frequency estimate**
//! ([`calibrate_clock_ghz`]), which is explicitly an estimate, not a hardware
//! counter; the real counters live in `evidence/bench/perf.json`.
//!
//! The three-dimensional Pareto frontier over (min latency, max throughput,
//! physical-budget efficiency) is produced per working set and is **not**
//! collapsed into a single score.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

use crate::error::Result;
use crate::manifest::{create, manifest, EkbloomBody};
use crate::metrics::now_ns;
use crate::planner::Planner;
use crate::{ManifestationProfile, ReadRequest};

/// Configuration of one court run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CourtConfig {
    /// Fresh repetitions per (workload, candidate) point.
    pub reps: usize,
    /// Whether to randomise the candidate order within each repetition.
    pub interleave: bool,
    /// Seed for offset generation, candidate shuffling and bootstrap resampling.
    pub seed: u64,
}

impl Default for CourtConfig {
    fn default() -> CourtConfig {
        CourtConfig {
            reps: 5,
            interleave: true,
            seed: 0xE8B1_0000_C09B_7D15,
        }
    }
}

/// Number of bootstrap resamples for a 95% CI.
const BOOTSTRAP_RESAMPLES: usize = 1000;

/// A distribution summary of a nanosecond sample, with a bootstrap 95% CI of the
/// mean.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dist {
    /// 50th percentile (nearest rank).
    pub p50: f64,
    /// 90th percentile.
    pub p90: f64,
    /// 99th percentile.
    pub p99: f64,
    /// 99.9th percentile.
    pub p999: f64,
    /// Minimum.
    pub min: f64,
    /// Maximum.
    pub max: f64,
    /// Arithmetic mean.
    pub mean: f64,
    /// Lower bound of the 95% bootstrap confidence interval of the mean.
    pub ci95_lo: f64,
    /// Upper bound of the 95% bootstrap confidence interval of the mean.
    pub ci95_hi: f64,
}

impl Dist {
    /// An all-zero distribution (used for workloads with no latency samples).
    pub fn empty() -> Dist {
        Dist {
            p50: 0.0,
            p90: 0.0,
            p99: 0.0,
            p999: 0.0,
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            ci95_lo: 0.0,
            ci95_hi: 0.0,
        }
    }

    /// Summarise a nanosecond sample. The 95% CI is a deterministic bootstrap of
    /// the mean, seeded with `seed`.
    pub fn from_samples(samples: &[u64], seed: u64) -> Dist {
        if samples.is_empty() {
            return Dist::empty();
        }
        let mut v = samples.to_vec();
        v.sort_unstable();
        let n = v.len();
        let quantile = |q: f64| -> f64 {
            let rank = (q * n as f64).ceil();
            let idx = (rank as usize).saturating_sub(1).min(n - 1);
            v[idx] as f64
        };
        let sum: u128 = v.iter().map(|&x| u128::from(x)).sum();
        let mean = sum as f64 / n as f64;
        let (ci95_lo, ci95_hi) = bootstrap_ci(&v, seed);
        Dist {
            p50: quantile(0.50),
            p90: quantile(0.90),
            p99: quantile(0.99),
            p999: quantile(0.999),
            min: v[0] as f64,
            max: v[n - 1] as f64,
            mean,
            ci95_lo,
            ci95_hi,
        }
    }
}

/// Bootstrap 95% CI of the mean over an ascending sample.
fn bootstrap_ci(sorted: &[u64], seed: u64) -> (f64, f64) {
    let n = sorted.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    if n == 1 {
        let m = sorted[0] as f64;
        return (m, m);
    }
    let mut rng = Lcg::new(seed ^ 0xB007_57A9_0000_0001);
    let mut means: Vec<f64> = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        let mut sum: u128 = 0;
        for _ in 0..n {
            let idx = rng.below(n as u64) as usize;
            sum += u128::from(sorted[idx]);
        }
        means.push(sum as f64 / n as f64);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let nb = means.len();
    let lo_idx = ((0.025 * nb as f64).floor() as usize).min(nb - 1);
    let hi_idx = (((0.975 * nb as f64).ceil() as usize).saturating_sub(1)).min(nb - 1);
    (means[lo_idx], means[hi_idx])
}

/// A tiny deterministic LCG (Numerical Recipes constants), for offset generation,
/// shuffling and bootstrap resampling.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Lcg {
        Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

/// Deterministic LCG corpus (identical generator to the other harnesses).
pub fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
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

/// Deterministic, reproducible sample offsets for a fixed-length range workload:
/// `count` offsets uniformly spread in `[0, logical_len - len]`, so every offset
/// admits a `len`-byte request inside the object.
///
/// Uses a SplitMix64 step so a seed reproduces the exact sample on any host — the
/// frontier harness needs the *same* offsets across contenders and reruns for the
/// comparison to be honest.
pub fn spread_offsets(seed: u64, count: usize, logical_len: u64, len: u64) -> Vec<u64> {
    let hi = logical_len.saturating_sub(len.max(1));
    let mut s = seed;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push(if hi == 0 {
            0
        } else {
            z % (hi.saturating_add(1))
        });
    }
    out
}

/// Estimate the core frequency in GHz from a dependent add chain.
///
/// A dependent `x = x + 1` chain retires at (roughly) one iteration per cycle on
/// the reference cores, so `iterations / elapsed_ns` estimates GHz. This is an
/// **estimate**, recorded as such; it is not a hardware performance counter.
pub fn calibrate_clock_ghz() -> f64 {
    let iters: u64 = 20_000_000;
    let mut x = 0u64;
    let start = now_ns();
    for _ in 0..iters {
        x = black_box(x.wrapping_add(1));
    }
    let elapsed = now_ns().saturating_sub(start).max(1);
    black_box(x);
    iters as f64 / elapsed as f64
}

/// Estimate per-iteration loop/clock overhead in nanoseconds.
pub fn loop_overhead_ns(iterations: u64) -> f64 {
    let iters = iterations.max(1);
    let start = now_ns();
    for _ in 0..iters {
        black_box(0u8);
    }
    let elapsed = now_ns().saturating_sub(start);
    elapsed as f64 / iters as f64
}

/// The configurable working-set ladder: real cache transitions, not nominal
/// labels. 16/32/64 KiB (L1), 256/512 KiB, 1/2 MiB (L2), 8/32/64/80/96/128 MiB
/// (LLC), 256 MiB and 1 GiB (DRAM).
pub fn default_working_sets() -> Vec<usize> {
    vec![
        16 * 1024,
        32 * 1024,
        64 * 1024,
        256 * 1024,
        512 * 1024,
        1024 * 1024,
        2 * 1024 * 1024,
        8 * 1024 * 1024,
        32 * 1024 * 1024,
        64 * 1024 * 1024,
        80 * 1024 * 1024,
        96 * 1024 * 1024,
        128 * 1024 * 1024,
        256 * 1024 * 1024,
        1024 * 1024 * 1024,
    ]
}

/// Which workload a row measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workload {
    /// Sequential full reconstruction (throughput, cycles/byte).
    Sequential,
    /// Uniform random single-byte reads (p50/p99/p99.9).
    Random1b,
    /// Uniform random 4 KiB range reads.
    Random4k,
    /// Uniform random 64 KiB range reads.
    Random64k,
    /// 90% hot / 10% cold single-byte reads.
    HotCold90_10,
    /// 99% hot / 1% cold single-byte reads.
    HotCold99_1,
    /// A moving 64-byte hot window sliding across the object.
    MovingWindow,
    /// Reads concurrent with background manifestation.
    Concurrent,
    /// Cold file-backed single-byte reads (pread through the file reader).
    FileBackedCold,
}

impl Workload {
    /// Stable label.
    pub fn label(self) -> &'static str {
        match self {
            Workload::Sequential => "sequential",
            Workload::Random1b => "random-1b",
            Workload::Random4k => "random-4k",
            Workload::Random64k => "random-64k",
            Workload::HotCold90_10 => "hot-cold-90-10",
            Workload::HotCold99_1 => "hot-cold-99-1",
            Workload::MovingWindow => "moving-window",
            Workload::Concurrent => "concurrent-manifestation",
            Workload::FileBackedCold => "file-backed-cold",
        }
    }

    fn is_throughput(self) -> bool {
        matches!(self, Workload::Sequential)
    }

    /// The per-operation request length for latency workloads.
    fn request_len(self) -> u64 {
        match self {
            Workload::Random4k => 4096,
            Workload::Random64k => 65536,
            Workload::MovingWindow => 64,
            _ => 1,
        }
    }

    /// Batch size so each timed batch spans well over the clock resolution.
    fn batch(self) -> u64 {
        match self {
            Workload::Random1b | Workload::HotCold90_10 | Workload::HotCold99_1 => 256,
            Workload::MovingWindow => 128,
            Workload::Random4k => 32,
            Workload::Random64k => 1,
            _ => 1,
        }
    }

    fn salt(self) -> u64 {
        match self {
            Workload::Sequential => 0x01,
            Workload::Random1b => 0x02,
            Workload::Random4k => 0x03,
            Workload::Random64k => 0x04,
            Workload::HotCold90_10 => 0x05,
            Workload::HotCold99_1 => 0x06,
            Workload::MovingWindow => 0x07,
            Workload::Concurrent => 0x08,
            Workload::FileBackedCold => 0x09,
        }
    }
}

/// The data a candidate serves reads from.
enum CandidateKind {
    /// A raw in-memory `Vec<u8>` (the direct-access baseline).
    Raw(Vec<u8>),
    /// An in-memory EKBLOOM body read through the optimised fast paths.
    Body(Box<EkbloomBody>),
    /// An in-memory EKBLOOM body read through a pre-certified plan.
    Planned {
        planner: Planner,
        body: Box<EkbloomBody>,
    },
}

/// One measurable candidate: a name, its manifested byte footprint, and the data
/// it serves reads from.
pub struct Candidate {
    name: String,
    physical_bytes: u64,
    kind: CandidateKind,
}

impl Candidate {
    /// A raw-byte direct-access baseline at RAM residency.
    pub fn raw_vec(name: impl Into<String>, bytes: Vec<u8>) -> Candidate {
        let physical_bytes = bytes.len() as u64;
        Candidate {
            name: name.into(),
            physical_bytes,
            kind: CandidateKind::Raw(bytes),
        }
    }

    /// An EKBLOOM body read through the optimised fast paths.
    pub fn body(name: impl Into<String>, body: EkbloomBody) -> Candidate {
        let physical_bytes = body.physical_bytes();
        Candidate {
            name: name.into(),
            physical_bytes,
            kind: CandidateKind::Body(Box::new(body)),
        }
    }

    /// An EKBLOOM body read through a pre-certified [`Planner`].
    pub fn planned(name: impl Into<String>, planner: Planner, body: EkbloomBody) -> Candidate {
        let physical_bytes = body.physical_bytes();
        Candidate {
            name: name.into(),
            physical_bytes,
            kind: CandidateKind::Planned {
                planner,
                body: Box::new(body),
            },
        }
    }

    /// The candidate label.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The manifested (or retained) physical bytes.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.physical_bytes
    }

    /// The logical length this candidate serves.
    pub fn logical_len(&self) -> u64 {
        match &self.kind {
            CandidateKind::Raw(b) => b.len() as u64,
            CandidateKind::Body(b) => b.logical_len,
            CandidateKind::Planned { body, .. } => body.logical_len,
        }
    }

    /// Read `[offset, offset + len)` appending into `out`.
    pub fn read_into(&self, offset: u64, len: u64, out: &mut Vec<u8>) -> Result<()> {
        match &self.kind {
            CandidateKind::Raw(bytes) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
                let slice =
                    bytes
                        .get(start..end)
                        .ok_or(crate::error::EkbloomError::OutOfBounds {
                            offset,
                            length: len,
                            logical_len: bytes.len() as u64,
                        })?;
                out.extend_from_slice(slice);
                Ok(())
            }
            CandidateKind::Body(body) => crate::fastpath::read_ranges(body, &[(offset, len)], out),
            CandidateKind::Planned { planner, body } => {
                let bytes = planner.read(
                    body,
                    ReadRequest {
                        offset,
                        length: len,
                    },
                )?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
        }
    }

    /// Read the whole logical object appending into `out`.
    pub fn read_all_into(&self, out: &mut Vec<u8>) -> Result<()> {
        match &self.kind {
            CandidateKind::Raw(bytes) => {
                out.extend_from_slice(bytes);
                Ok(())
            }
            CandidateKind::Body(body) => crate::fastpath::read_sequential(body, out),
            CandidateKind::Planned { planner, body } => {
                let bytes = planner.read(
                    body,
                    ReadRequest {
                        offset: 0,
                        length: body.logical_len,
                    },
                )?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
        }
    }
}

/// One measured court row.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Working-set (logical) size in bytes.
    pub working_set: u64,
    /// Workload label.
    pub workload: String,
    /// Candidate label.
    pub candidate: String,
    /// Memory residency (`in-memory`, `file-backed`).
    pub residency: String,
    /// Candidate manifested/retained physical bytes.
    pub physical_bytes: u64,
    /// Per-operation latency distribution (empty for throughput workloads).
    pub dist_ns: Dist,
    /// Throughput in bytes/s (0 for latency workloads).
    pub throughput_bytes_per_s: f64,
    /// Estimated cycles per byte (0 when not a throughput row).
    pub cycles_per_byte: f64,
    /// Number of fresh repetitions.
    pub reps: usize,
    /// Number of pooled per-operation samples.
    pub samples: u64,
}

/// One candidate point on the 3-D Pareto frontier for a working set.
#[derive(Clone, Debug, PartialEq)]
pub struct ParetoPoint {
    /// Working set.
    pub working_set: u64,
    /// Candidate label.
    pub candidate: String,
    /// Minimum latency across latency workloads (ns); `inf` when none.
    pub min_latency_ns: f64,
    /// Maximum throughput across throughput workloads (bytes/s).
    pub max_throughput_bytes_per_s: f64,
    /// Manifested/retained physical bytes.
    pub physical_bytes: u64,
    /// Physical-budget efficiency: throughput per manifested byte.
    pub efficiency: f64,
    /// Whether this point is on the Pareto frontier.
    pub on_frontier: bool,
}

/// Environment provenance recorded alongside the numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CourtEnv {
    /// CPU affinity (from `/proc/self/status` `Cpus_allowed_list`).
    pub pinning: String,
    /// CPU frequency governor (or `unavailable`).
    pub governor: String,
    /// SMT status (or `unavailable`).
    pub smt: String,
    /// Page size in bytes (or `unknown`).
    pub page_size: String,
}

/// The full court report.
#[derive(Clone, Debug, PartialEq)]
pub struct CourtReport {
    /// The configuration used.
    pub config: CourtConfig,
    /// Estimated clock in GHz (dependent-add-chain estimate).
    pub clock_ghz: f64,
    /// Calibrated loop/clock overhead per iteration in nanoseconds.
    pub loop_overhead_ns: f64,
    /// Environment provenance.
    pub env: CourtEnv,
    /// Every measured row.
    pub rows: Vec<Row>,
    /// The per-working-set 3-D Pareto frontier.
    pub pareto: Vec<ParetoPoint>,
}

/// The court harness: accumulates rows across working sets.
pub struct Court {
    cfg: CourtConfig,
    clock_ghz: f64,
    overhead_ns: f64,
    samples: usize,
    rows: Vec<Row>,
}

impl Court {
    /// Create a court, calibrating the clock and loop overhead.
    pub fn new(cfg: CourtConfig) -> Court {
        Court {
            cfg,
            clock_ghz: calibrate_clock_ghz(),
            overhead_ns: loop_overhead_ns(2_000_000),
            samples: 384,
            rows: Vec::new(),
        }
    }

    /// The estimated clock in GHz.
    pub fn clock_ghz(&self) -> f64 {
        self.clock_ghz
    }

    /// The calibrated loop/clock overhead per iteration in nanoseconds.
    pub fn report_loop_overhead(&self) -> f64 {
        self.overhead_ns
    }

    /// Append a pre-measured row (used for workloads measured outside the court).
    pub fn push_row(&mut self, row: Row) {
        self.rows.push(row);
    }

    /// Measure one workload for every candidate on one working set.
    pub fn measure(&mut self, working_set: u64, workload: Workload, candidates: &[&Candidate]) {
        if working_set == 0 || candidates.is_empty() {
            return;
        }
        let reps = self.cfg.reps.max(1);
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        let mut rng = Lcg::new(self.cfg.seed ^ workload.salt());

        if workload.is_throughput() {
            let mut total_bytes = vec![0u64; candidates.len()];
            let mut total_ns = vec![0u64; candidates.len()];
            let mut rep_ns: Vec<Vec<u64>> = vec![Vec::new(); candidates.len()];
            let mut out = Vec::new();
            for _ in 0..reps {
                if self.cfg.interleave {
                    shuffle(&mut order, &mut rng);
                }
                for &ci in &order {
                    out.clear();
                    let a = now_ns();
                    let _ = candidates[ci].read_all_into(&mut out);
                    let e = now_ns().saturating_sub(a).max(1);
                    black_box(out.as_slice());
                    total_bytes[ci] = total_bytes[ci].saturating_add(out.len() as u64);
                    total_ns[ci] = total_ns[ci].saturating_add(e);
                    rep_ns[ci].push(e);
                }
            }
            for (ci, c) in candidates.iter().enumerate() {
                let tb = total_bytes[ci].max(1);
                let tn = total_ns[ci].max(1);
                let throughput = tb as f64 * 1e9 / tn as f64;
                let cycles_per_byte = if throughput > 0.0 {
                    self.clock_ghz * 1e9 / throughput
                } else {
                    0.0
                };
                self.rows.push(Row {
                    working_set,
                    workload: workload.label().to_string(),
                    candidate: c.name().to_string(),
                    residency: "in-memory".to_string(),
                    physical_bytes: c.physical_bytes(),
                    dist_ns: Dist::from_samples(&rep_ns[ci], self.cfg.seed),
                    throughput_bytes_per_s: throughput,
                    cycles_per_byte,
                    reps,
                    samples: rep_ns[ci].len() as u64,
                });
            }
            return;
        }

        let offsets = self.offsets(workload, working_set);
        let batch = workload.batch();
        let len = workload.request_len();
        let mut pooled: Vec<Vec<u64>> = vec![Vec::new(); candidates.len()];
        let mut out = Vec::new();
        for _ in 0..reps {
            if self.cfg.interleave {
                shuffle(&mut order, &mut rng);
            }
            for &ci in &order {
                let c = candidates[ci];
                let mut samples = Vec::with_capacity(offsets.len());
                for &off in &offsets {
                    let a = now_ns();
                    for _ in 0..batch {
                        out.clear();
                        let _ = c.read_into(off, len, &mut out);
                    }
                    let total = now_ns().saturating_sub(a) as f64;
                    let per = ((total - self.overhead_ns * batch as f64) / batch as f64).max(0.0);
                    samples.push(per as u64);
                    black_box(out.as_slice());
                }
                pooled[ci].extend(samples);
            }
        }
        for (ci, c) in candidates.iter().enumerate() {
            self.rows.push(Row {
                working_set,
                workload: workload.label().to_string(),
                candidate: c.name().to_string(),
                residency: "in-memory".to_string(),
                physical_bytes: c.physical_bytes(),
                dist_ns: Dist::from_samples(&pooled[ci], self.cfg.seed),
                throughput_bytes_per_s: 0.0,
                cycles_per_byte: 0.0,
                reps,
                samples: pooled[ci].len() as u64,
            });
        }
    }

    /// Measure reads concurrent with background manifestation.
    ///
    /// A background thread repeatedly builds a fresh body and manifests a
    /// `Sparse{interval:64}` checkpoint set over `source` while the caller samples
    /// single-byte reads through each candidate.
    pub fn measure_concurrent(&mut self, source: &[u8], candidates: &[&Candidate]) {
        if candidates.is_empty() || source.is_empty() {
            return;
        }
        let reps = self.cfg.reps.max(1);
        let offsets = self.offsets(Workload::Random1b, source.len() as u64);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut pooled: Vec<Vec<u64>> = vec![Vec::new(); candidates.len()];
        let mut manifestations = 0u64;
        {
            let stop = &stop;
            std::thread::scope(|scope| {
                let source_ref = source;
                let counter = &mut manifestations;
                scope.spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Ok(mut b) = create(source_ref, ManifestationProfile::Root) {
                            let _ = manifest(&mut b, ManifestationProfile::Sparse { interval: 64 });
                            black_box(&b);
                            *counter = counter.wrapping_add(1);
                        } else {
                            break;
                        }
                    }
                });
                let mut out = Vec::new();
                for (ci, c) in candidates.iter().enumerate() {
                    for _ in 0..reps {
                        for &off in &offsets {
                            let a = now_ns();
                            out.clear();
                            let _ = c.read_into(off, 1, &mut out);
                            let e = now_ns().saturating_sub(a);
                            pooled[ci].push(e);
                            black_box(out.as_slice());
                        }
                    }
                }
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            });
        }
        let _ = manifestations;
        for (ci, c) in candidates.iter().enumerate() {
            self.rows.push(Row {
                working_set: source.len() as u64,
                workload: Workload::Concurrent.label().to_string(),
                candidate: c.name().to_string(),
                residency: "in-memory".to_string(),
                physical_bytes: c.physical_bytes(),
                dist_ns: Dist::from_samples(&pooled[ci], self.cfg.seed),
                throughput_bytes_per_s: 0.0,
                cycles_per_byte: 0.0,
                reps,
                samples: pooled[ci].len() as u64,
            });
        }
    }

    /// Deterministic offsets for a latency workload.
    fn offsets(&self, wl: Workload, n: u64) -> Vec<u64> {
        let count = self.samples;
        let mut rng = Lcg::new(self.cfg.seed ^ wl.salt() ^ 0xA5A5_5A5A);
        let bounded = |rng: &mut Lcg, window: u64| -> u64 {
            let window = window.min(n);
            let span = n.saturating_sub(window);
            if span == 0 {
                0
            } else {
                rng.below(span.saturating_add(1)).min(span)
            }
        };
        match wl {
            Workload::Sequential
            | Workload::Concurrent
            | Workload::FileBackedCold
            | Workload::Random1b => (0..count).map(|_| rng.below(n)).collect(),
            Workload::Random4k => (0..count).map(|_| bounded(&mut rng, 4096)).collect(),
            Workload::Random64k => (0..count).map(|_| bounded(&mut rng, 65536)).collect(),
            Workload::HotCold90_10 => {
                let hot = (n / 16).max(1).min(n);
                (0..count)
                    .map(|_| {
                        if rng.below(100) < 90 {
                            rng.below(hot)
                        } else {
                            rng.below(n)
                        }
                    })
                    .collect()
            }
            Workload::HotCold99_1 => {
                let hot = (n / 16).max(1).min(n);
                (0..count)
                    .map(|_| {
                        if rng.below(100) < 99 {
                            rng.below(hot)
                        } else {
                            rng.below(n)
                        }
                    })
                    .collect()
            }
            Workload::MovingWindow => {
                let win = (n / 8).max(64).min(n);
                let stride = (n / count as u64).max(1);
                (0..count)
                    .map(|i| (i as u64).wrapping_mul(stride) % win.max(1))
                    .collect()
            }
        }
    }

    /// Finish the run, compute the Pareto frontier, and return the report.
    pub fn report(self, env: CourtEnv) -> CourtReport {
        let pareto = pareto_by_working_set(&self.rows);
        CourtReport {
            config: self.cfg,
            clock_ghz: self.clock_ghz,
            loop_overhead_ns: self.overhead_ns,
            env,
            rows: self.rows,
            pareto,
        }
    }
}

/// Fisher-Yates shuffle of `items` using `rng`.
fn shuffle(items: &mut [usize], rng: &mut Lcg) {
    let n = items.len();
    if n < 2 {
        return;
    }
    for i in (1..n).rev() {
        let j = rng.below((i + 1) as u64) as usize;
        items.swap(i, j);
    }
}

/// Compute the per-working-set 3-D Pareto frontier over (min latency,
/// max throughput, physical-budget efficiency) — latency lower is better, the
/// other two higher is better.
fn pareto_by_working_set(rows: &[Row]) -> Vec<ParetoPoint> {
    let sets: BTreeSet<u64> = rows.iter().map(|r| r.working_set).collect();
    let mut out = Vec::new();
    for ws in sets {
        // Aggregate per candidate: best (min latency, max throughput, footprint).
        let mut names: Vec<String> = Vec::new();
        let mut agg: Vec<(f64, f64, u64)> = Vec::new();
        for r in rows.iter().filter(|r| r.working_set == ws) {
            let idx = match names.iter().position(|n| n == &r.candidate) {
                Some(i) => i,
                None => {
                    names.push(r.candidate.clone());
                    agg.push((f64::INFINITY, 0.0, r.physical_bytes));
                    names.len() - 1
                }
            };
            let slot = &mut agg[idx];
            slot.2 = r.physical_bytes;
            if r.throughput_bytes_per_s > 0.0 {
                if r.throughput_bytes_per_s > slot.1 {
                    slot.1 = r.throughput_bytes_per_s;
                }
            } else if r.dist_ns.p50 > 0.0 && r.dist_ns.p50 < slot.0 {
                slot.0 = r.dist_ns.p50;
            }
        }
        let points: Vec<(String, f64, f64, u64, f64)> = names
            .iter()
            .zip(agg.iter())
            .map(|(name, &(lat, thr, phys))| {
                let efficiency = if phys > 0 { thr / phys as f64 } else { 0.0 };
                (name.clone(), lat, thr, phys, efficiency)
            })
            .collect();
        let frontier = pareto_flags(&points);
        for (i, (candidate, lat, thr, phys, eff)) in points.into_iter().enumerate() {
            out.push(ParetoPoint {
                working_set: ws,
                candidate,
                min_latency_ns: lat,
                max_throughput_bytes_per_s: thr,
                physical_bytes: phys,
                efficiency: eff,
                on_frontier: frontier[i],
            });
        }
    }
    out
}

/// Dominance flags for a 3-D minimax frontier (lower latency, higher throughput,
/// higher efficiency are better).
fn pareto_flags(points: &[(String, f64, f64, u64, f64)]) -> Vec<bool> {
    let n = points.len();
    let mut on = vec![true; n];
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let a = &points[i];
            let b = &points[j];
            let lag = b.1 <= a.1;
            let thr = b.2 >= a.2;
            let eff = b.4 >= a.4;
            let strict = b.1 < a.1 || b.2 > a.2 || b.4 > a.4;
            if lag && thr && eff && strict {
                on[i] = false;
                break;
            }
        }
    }
    on
}

/// Collect environment provenance from inside the (container) process.
pub fn collect_environment() -> CourtEnv {
    CourtEnv {
        pinning: read_cpus_allowed(),
        governor: read_first_line("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        smt: read_first_line("/sys/devices/system/cpu/smt/active"),
        page_size: std::env::var("EKB_PAGE_SIZE").unwrap_or_else(|_| "unknown".to_string()),
    }
}

/// Read `Cpus_allowed_list` from `/proc/self/status`, or `unavailable`.
fn read_cpus_allowed() -> String {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
                return rest.trim().to_string();
            }
        }
    }
    "unavailable".to_string()
}

/// Read the first line of a sysfs file, or `unavailable`.
fn read_first_line(path: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unavailable".to_string())
}

/// Write a court report as pretty JSON, creating parent directories.
pub fn write_court_json(path: &Path, report: &CourtReport) -> Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/court.rs (sh scripts/run-court.sh)\","
    );
    let _ = writeln!(s, "  \"config\": {{");
    let _ = writeln!(s, "    \"reps\": {},", report.config.reps);
    let _ = writeln!(s, "    \"interleave\": {},", report.config.interleave);
    let _ = writeln!(s, "    \"seed\": {}", report.config.seed);
    let _ = writeln!(s, "  }},");
    let _ = writeln!(
        s,
        "  \"clock_ghz_estimate\": {:.6},",
        finite(report.clock_ghz)
    );
    let _ = writeln!(
        s,
        "  \"loop_overhead_ns\": {:.6},",
        finite(report.loop_overhead_ns)
    );
    let _ = writeln!(s, "  \"environment\": {{");
    let _ = writeln!(
        s,
        "    \"pinning\": \"{}\",",
        json_escape(&report.env.pinning)
    );
    let _ = writeln!(
        s,
        "    \"governor\": \"{}\",",
        json_escape(&report.env.governor)
    );
    let _ = writeln!(s, "    \"smt\": \"{}\",", json_escape(&report.env.smt));
    let _ = writeln!(
        s,
        "    \"page_size\": \"{}\"",
        json_escape(&report.env.page_size)
    );
    let _ = writeln!(s, "  }},");
    let _ = writeln!(
        s,
        "  \"caveats\": \"container CPU quotas and a shared host make absolute numbers \
         host-dependent; frequency governor and SMT are recorded above; cycles/byte uses a \
         dependent-add-chain frequency estimate, not hardware counters (see perf.json)\","
    );

    s.push_str("  \"rows\": [\n");
    for (i, r) in report.rows.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"working_set\": {},", r.working_set);
        let _ = writeln!(s, "      \"workload\": \"{}\",", json_escape(&r.workload));
        let _ = writeln!(s, "      \"candidate\": \"{}\",", json_escape(&r.candidate));
        let _ = writeln!(s, "      \"residency\": \"{}\",", json_escape(&r.residency));
        let _ = writeln!(s, "      \"physical_bytes\": {},", r.physical_bytes);
        let _ = writeln!(s, "      \"latency_ns\": {{");
        let _ = writeln!(s, "        \"p50\": {:.3},", finite(r.dist_ns.p50));
        let _ = writeln!(s, "        \"p90\": {:.3},", finite(r.dist_ns.p90));
        let _ = writeln!(s, "        \"p99\": {:.3},", finite(r.dist_ns.p99));
        let _ = writeln!(s, "        \"p999\": {:.3},", finite(r.dist_ns.p999));
        let _ = writeln!(s, "        \"min\": {:.3},", finite(r.dist_ns.min));
        let _ = writeln!(s, "        \"max\": {:.3},", finite(r.dist_ns.max));
        let _ = writeln!(s, "        \"mean\": {:.3},", finite(r.dist_ns.mean));
        let _ = writeln!(s, "        \"ci95_lo\": {:.3},", finite(r.dist_ns.ci95_lo));
        let _ = writeln!(s, "        \"ci95_hi\": {:.3}", finite(r.dist_ns.ci95_hi));
        let _ = writeln!(s, "      }},");
        let _ = writeln!(
            s,
            "      \"throughput_bytes_per_s\": {:.3},",
            finite(r.throughput_bytes_per_s)
        );
        let _ = writeln!(
            s,
            "      \"cycles_per_byte\": {:.6},",
            finite(r.cycles_per_byte)
        );
        let _ = writeln!(s, "      \"reps\": {},", r.reps);
        let _ = writeln!(s, "      \"samples\": {}", r.samples);
        s.push_str(if i + 1 == report.rows.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ],\n");

    s.push_str("  \"pareto\": [\n");
    for (i, p) in report.pareto.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"working_set\": {},", p.working_set);
        let _ = writeln!(s, "      \"candidate\": \"{}\",", json_escape(&p.candidate));
        let _ = writeln!(
            s,
            "      \"min_latency_ns\": {},",
            json_number(p.min_latency_ns)
        );
        let _ = writeln!(
            s,
            "      \"max_throughput_bytes_per_s\": {:.3},",
            finite(p.max_throughput_bytes_per_s)
        );
        let _ = writeln!(s, "      \"physical_bytes\": {},", p.physical_bytes);
        let _ = writeln!(s, "      \"efficiency\": {:.6},", finite(p.efficiency));
        let _ = writeln!(s, "      \"on_frontier\": {}", p.on_frontier);
        s.push_str(if i + 1 == report.pareto.len() {
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

/// A finite, non-negative view of `v`.
fn finite(v: f64) -> f64 {
    if v.is_finite() && v >= 0.0 {
        v
    } else {
        0.0
    }
}

/// Render a number, using `null` for non-finite values (so JSON stays valid).
fn json_number(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.3}")
    } else {
        "null".to_string()
    }
}

/// Minimal JSON string escaping for ASCII labels.
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
