//! Machine-9 long-run **endurance, memory-fragmentation, and physical-byte
//! growth** study — a *time-compressed* replacement for an unrun 7-day soak.
//!
//! One process, one loop, bounded by `--seconds` and/or `--cycles`. It sustains a
//! mixed read (`decode`) / write (`publish`) / non-withdrawal (`append`) workload
//! over a deterministic 20,000-record corpus of 100-byte records shared through a
//! trained machine-9 dictionary, and samples process memory after every batch:
//!
//! * a **counting global allocator** reports exactly how many bytes the process
//!   holds live, its peak, and the alloc/free counts;
//! * process RSS and `VmHWM` come from the library helpers
//!   ([`ekbloom::current_rss_bytes`] / [`ekbloom::peak_rss_bytes`]), which read
//!   `/proc/self/status` — this example never re-implements them.
//!
//! It answers: *does machine 9 leak, fragment, or grow unbounded under sustained
//! mixed use, and does `physical_bytes()` stay exact at scale?*
//!
//! The example **emits the JSON on stdout** (nothing else is written to stdout);
//! `scripts/run-endurance.sh` captures it into `evidence/bench/endurance.json`
//! inside the pinned, memory-capped container. Any harness failure — a byte-exact
//! sentinel mismatch, a non-monotone append series, or a failed reopen check —
//! exits non-zero so the gate is real.
//!
//! This is a **time-compressed** run, not a 7-day run: see the `caveat` field and
//! `docs/ENDURANCE.md`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::fmt::Write as _;
use std::hint::black_box;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ekbloom::metrics::now_ns;
use ekbloom::{
    collect_environment, create_with_dict, current_rss_bytes, fastpath, grow, peak_rss_bytes,
    percentiles, reader, train_dictionary, write_body, writer, Dictionary, EkbloomBody,
    ManifestationProfile,
};

/// `writeln!` into a `String`, discarding the (infallible) `fmt::Result`.
macro_rules! jline {
    ($dst:expr, $($arg:tt)*) => {{
        let _ = writeln!($dst, $($arg)*);
    }};
}

// ---------------------------------------------------------------------------
// Counting global allocator.
// ---------------------------------------------------------------------------

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

/// A `GlobalAlloc` wrapper that tracks live bytes, the high-water mark, and the
/// alloc/free counts, delegating the actual work to the system allocator.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            account_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(layout);
        if !p.is_null() {
            account_alloc(layout.size());
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            let old = layout.size();
            if new_size >= old {
                let delta = new_size - old;
                let cur = IN_USE
                    .fetch_add(delta, Ordering::Relaxed)
                    .wrapping_add(delta);
                PEAK.fetch_max(cur, Ordering::Relaxed);
            } else {
                IN_USE.fetch_sub(old - new_size, Ordering::Relaxed);
            }
            // A realloc is one new live block replacing one old one, so it is
            // counted as one alloc and one free (`alloc_count - free_count` is
            // then the number of live allocations).
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            FREES.fetch_add(1, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        FREES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Shared accounting for a successful allocation of `size` bytes.
fn account_alloc(size: usize) {
    let cur = IN_USE.fetch_add(size, Ordering::Relaxed).wrapping_add(size);
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    PEAK.fetch_max(cur, Ordering::Relaxed);
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// ---------------------------------------------------------------------------
// Deterministic corpus, dictionary, and access skew.
// ---------------------------------------------------------------------------

/// Records in the endurance corpus.
const RECORDS: usize = 20_000;
/// Target record size (bytes).
const RECORD_BYTES: usize = 100;
/// Decode operations per sampled batch.
const OPS_PER_SAMPLE: usize = 4096;
/// Every `SENTINEL_EVERY`-th decode is checked byte-exact against the source.
const SENTINEL_EVERY: usize = 256;
/// Fraction of draws (percent) from the hot set.
const HOT_READ_PCT: u32 = 80;
/// Fraction of records (percent) that form the hot set.
const HOT_SET_PCT: usize = 20;
/// Decode batches interleaved per `mix` round.
const DECODE_BATCHES_PER_ROUND: usize = 8;
/// Records concatenated to form the logical object of the append file.
const APPEND_RECORDS: usize = 64;
/// Hard cap on appended generations (bounds the O(n) re-read per append).
const APPEND_CAP: u64 = 128;
/// Cap on the time-ordered decode-latency sample (entries).
const LAT_CAP: usize = 1 << 18;
/// Cap on the time-ordered RSS sample used for the median and plateau halves.
const RSS_CAP: usize = 1 << 16;
/// Cap on the time-ordered full sample series kept for downsampled emission.
const SERIES_HIST_CAP: usize = 4096;
/// Maximum entries retained in any emitted time series.
const SERIES_MAX: usize = 64;
/// Cap on the retained publish physical/on-disk value samples. The publish phase
/// runs millions of iterations, so these MUST be bounded, not `Vec`s that grow
/// with the run (an earlier revision's unbounded publish `Vec`s were the entire
/// apparent "publish leak" in the phase matrix).
const PUBLISH_CAP: usize = 1 << 14;
/// Cap on the retained append series / verification offsets (also bounded).
const APPEND_SERIES_CAP: usize = 1 << 12;
/// Default wall-clock budget (seconds).
const DEFAULT_SECONDS: u64 = 120;
/// Fixed default seed (machine-9 themed).
const DEFAULT_SEED: u64 = 0x5D3B_100E_0000_0009;

/// A text-like record of exactly `target` bytes drawn from a small token set (a
/// skewed, repetitive alphabet), deterministically seeded by `seed`. Copied
/// verbatim from `examples/dictionary.rs::make_record` (same LCG).
fn make_record(seed: usize, target: usize) -> Vec<u8> {
    let parts = [
        "id=",
        "user",
        "kind=",
        "alpha",
        "beta",
        "gamma",
        "region=",
        "us-east-1",
        "eu-west-1",
        "ap-south-1",
        "score=",
        "000",
        "111",
        "222",
        "status=",
        "active",
        "idle",
        "|",
    ];
    let mut out = Vec::with_capacity(target + 16);
    let mut k = seed as u64;
    while out.len() < target {
        k = k
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(parts[((k >> 33) as usize) % parts.len()].as_bytes());
    }
    out.truncate(target);
    out
}

/// `n` deterministic records of exactly `size` bytes each.
fn corpus(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| make_record(i, size)).collect()
}

/// A deterministic 80/20 skew sampler over `[0, total)` (the concept of
/// `tests/common/rocksdb.rs::Skew`: `HOT_READ_PCT`% of draws from the head-heavy
/// hot `HOT_SET_PCT`%, the rest uniform over the cold set).
struct Skew {
    state: u64,
    total: usize,
    hot: usize,
}

impl Skew {
    fn new(seed: u64, total: usize, hot: usize) -> Skew {
        let total = total.max(1);
        let hot = hot.clamp(1, total);
        Skew {
            state: seed | 1,
            total,
            hot,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }

    /// Inverse-CDF of a rank-1 Zipf (`p(r) ∝ 1/(r+1)`) over `[0, n)`.
    fn zipf(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let u = self.unit();
        let r = ((n as f64 + 1.0).powf(u) - 1.0).floor();
        (r.max(0.0) as usize).min(n - 1)
    }

    fn next_index(&mut self) -> usize {
        if self.unit() < f64::from(HOT_READ_PCT) / 100.0 {
            self.zipf(self.hot)
        } else {
            let cold = self.total - self.hot;
            if cold == 0 {
                self.zipf(self.hot)
            } else {
                self.hot + (self.next_u64() as usize % cold)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Halving sampler: a bounded, time-ordered uniform sample of a stream.
// ---------------------------------------------------------------------------

/// Keeps a uniform-in-time sample of a stream, capped at `cap` entries: when the
/// cap is hit, every other entry is dropped and the stride doubles, so the
/// retained sample stays uniform in time and bounded in memory while preserving
/// the first and last regions.
#[derive(Clone)]
struct Halving<T: Copy> {
    items: Vec<T>,
    cap: usize,
    stride: u64,
    count: u64,
}

impl<T: Copy> Halving<T> {
    fn new(cap: usize) -> Self {
        Halving {
            items: Vec::with_capacity(cap),
            cap,
            stride: 1,
            count: 0,
        }
    }

    fn push(&mut self, v: T) {
        self.count += 1;
        if !self.count.is_multiple_of(self.stride) {
            return;
        }
        if self.items.len() >= self.cap {
            // Compress in place (keep even indices) instead of allocating a new
            // Vec: the old form freed and reallocated a multi-MB buffer on every
            // halving, which churned the allocator and could masquerade as RSS
            // creep in a long run.
            let mut w = 0usize;
            for r in 0..self.items.len() {
                if r % 2 == 0 {
                    self.items[w] = self.items[r];
                    w += 1;
                }
            }
            self.items.truncate(w);
            self.stride *= 2;
        }
        self.items.push(v);
    }

    fn items(&self) -> &[T] {
        &self.items
    }

    fn last(&self) -> Option<&T> {
        self.items.last()
    }

    /// Drop the retained sample and restart sampling: used when the append phase
    /// begins a fresh file, so the retained series cannot grow with the run.
    fn clear(&mut self) {
        self.items.clear();
        self.stride = 1;
        self.count = 0;
    }
}

// ---------------------------------------------------------------------------
// Time series.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Sample {
    t_ms: u64,
    rss_bytes: u64,
    in_use_bytes: u64,
    peak_alloc_bytes: u64,
    alloc_count: u64,
    free_count: u64,
    ops_total: u64,
    physical_bytes: Option<u64>,
}

#[derive(Clone, Copy)]
struct AppendPoint {
    generation: u64,
    physical_bytes: u64,
    on_disk_bytes: u64,
    returned: u64,
}

// ---------------------------------------------------------------------------
// Arguments.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Decode,
    Publish,
    Append,
    Mix,
}

impl Phase {
    fn parse(s: &str) -> Result<Phase, String> {
        match s {
            "decode" => Ok(Phase::Decode),
            "publish" => Ok(Phase::Publish),
            "append" => Ok(Phase::Append),
            "mix" => Ok(Phase::Mix),
            other => Err(format!(
                "unknown --phase {other} (want decode|publish|append|mix)"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Phase::Decode => "decode",
            Phase::Publish => "publish",
            Phase::Append => "append",
            Phase::Mix => "mix",
        }
    }
}

struct Args {
    seconds: u64,
    cycles: u64,
    phase: Phase,
    seed: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seconds: DEFAULT_SECONDS,
        cycles: 0,
        phase: Phase::Mix,
        seed: DEFAULT_SEED,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--seconds" => {
                i += 1;
                let v = argv.get(i).ok_or("--seconds needs a value")?;
                args.seconds = v
                    .parse()
                    .map_err(|_| format!("--seconds: {v} is not a number"))?;
            }
            "--cycles" => {
                i += 1;
                let v = argv.get(i).ok_or("--cycles needs a value")?;
                args.cycles = v
                    .parse()
                    .map_err(|_| format!("--cycles: {v} is not a number"))?;
            }
            "--phase" => {
                i += 1;
                let v = argv.get(i).ok_or("--phase needs a value")?;
                args.phase = Phase::parse(v)?;
            }
            "--seed" => {
                i += 1;
                let v = argv.get(i).ok_or("--seed needs a value")?;
                args.seed = v
                    .parse()
                    .map_err(|_| format!("--seed: {v} is not a number"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// The run.
// ---------------------------------------------------------------------------

struct Run {
    args: Args,
    dict: Dictionary,
    records: Vec<Vec<u8>>,
    cores: Vec<Vec<u8>>,
    skew: Skew,
    start: Instant,
    lat: Halving<u64>,
    rss_hist: Halving<u64>,
    series: Halving<Sample>,
    samples_seen: u64,
    rss_start: u64,
    rss_end: u64,
    rss_max: u64,
    in_use_start: u64,
    in_use_end: u64,
    in_use_max: u64,
    rss_warm: u64,
    in_use_warm: u64,
    decode_ops: u64,
    publish_ops: u64,
    append_gens: u64,
    sentinel_mismatches: u64,
    publish_physical: Halving<u64>,
    publish_on_disk: Halving<u64>,
    append_series: Halving<AppendPoint>,
    append_monotone: bool,
    append_reopen_verified: bool,
    append_max_generation: u64,
    append_checks: Halving<(u64, u64)>,
    append_gens_file: u64,
    append_cycles: u64,
    append_body: Option<EkbloomBody>,
    append_source: Vec<u8>,
    last_physical: u64,
    publish_path: PathBuf,
    append_path: PathBuf,
    ended_reason: String,
    cycles_done: u64,
    elapsed_s: f64,
}

impl Run {
    fn new(args: Args) -> Result<Run, Box<dyn Error>> {
        let records = corpus(RECORDS, RECORD_BYTES);
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let dict = train_dictionary(&refs)?;
        // `Dictionary::decode` takes the *bare* header-less record core produced
        // by `Dictionary::encode` (not the dict-id-prefixed `encode_record` form),
        // so the read path and the decode call describe the same bytes.
        let cores = records
            .iter()
            .map(|r| dict.encode(r))
            .collect::<ekbloom::Result<Vec<_>>>()?;

        let hot = (RECORDS * HOT_SET_PCT / 100).max(1);
        let skew = Skew::new(args.seed ^ 0x9E37_79B9_7F4A_7C15, RECORDS, hot);

        let mut append_source = Vec::with_capacity(APPEND_RECORDS * RECORD_BYTES);
        for r in records.iter().take(APPEND_RECORDS) {
            append_source.extend_from_slice(r);
        }

        let dir = std::env::temp_dir();
        let tag = format!("ekbloom-endurance-{}-{}", args.seed, std::process::id());
        let publish_path = dir.join(format!("{tag}-publish.ekb"));
        let append_path = dir.join(format!("{tag}-append.ekb"));

        Ok(Run {
            args,
            dict,
            records,
            cores,
            skew,
            start: Instant::now(),
            lat: Halving::new(LAT_CAP),
            rss_hist: Halving::new(RSS_CAP),
            series: Halving::new(SERIES_HIST_CAP),
            samples_seen: 0,
            rss_start: 0,
            rss_end: 0,
            rss_max: 0,
            in_use_start: 0,
            in_use_end: 0,
            in_use_max: 0,
            rss_warm: 0,
            in_use_warm: 0,
            decode_ops: 0,
            publish_ops: 0,
            append_gens: 0,
            sentinel_mismatches: 0,
            publish_physical: Halving::new(PUBLISH_CAP),
            publish_on_disk: Halving::new(PUBLISH_CAP),
            append_series: Halving::new(APPEND_SERIES_CAP),
            append_monotone: true,
            append_reopen_verified: true,
            append_max_generation: 0,
            append_checks: Halving::new(APPEND_SERIES_CAP),
            append_gens_file: 0,
            append_cycles: 0,
            append_body: None,
            append_source,
            last_physical: 0,
            publish_path,
            append_path,
            ended_reason: "seconds".to_string(),
            cycles_done: 0,
            elapsed_s: 0.0,
        })
    }

    fn ops_total(&self) -> u64 {
        self.decode_ops + self.publish_ops + self.append_gens
    }

    /// Push one time-series sample: RSS, allocator bytes, counts, and (in append
    /// rounds) the object's physical bytes.
    fn push_sample(&mut self, physical: Option<u64>) {
        let rss = current_rss_bytes();
        let in_use = IN_USE.load(Ordering::Relaxed) as u64;
        if self.samples_seen == 0 {
            self.rss_start = rss;
            self.in_use_start = in_use;
        }
        self.samples_seen += 1;
        self.rss_end = rss;
        self.in_use_end = in_use;
        self.rss_max = self.rss_max.max(rss);
        self.in_use_max = self.in_use_max.max(in_use);
        self.rss_hist.push(rss);
        let ops_total = self.ops_total();
        self.series.push(Sample {
            t_ms: self.start.elapsed().as_millis() as u64,
            rss_bytes: rss,
            in_use_bytes: in_use,
            peak_alloc_bytes: PEAK.load(Ordering::Relaxed) as u64,
            alloc_count: ALLOCS.load(Ordering::Relaxed) as u64,
            free_count: FREES.load(Ordering::Relaxed) as u64,
            ops_total,
            physical_bytes: physical,
        });
    }

    /// The decode (read) path: one batch of `OPS_PER_SAMPLE` skewed indices, each
    /// a `Dictionary::decode`, with a byte-exact sentinel every `SENTINEL_EVERY`.
    fn decode_batch(&mut self) -> Result<(), Box<dyn Error>> {
        for k in 0..OPS_PER_SAMPLE {
            let i = self.skew.next_index();
            let t0 = now_ns();
            let out = self.dict.decode(&self.cores[i])?;
            let dt = now_ns().saturating_sub(t0);
            black_box(&out);
            if k % SENTINEL_EVERY == 0 && out.as_slice() != self.records[i].as_slice() {
                self.sentinel_mismatches += 1;
            }
            self.lat.push(dt);
            self.decode_ops += 1;
        }
        Ok(())
    }

    /// The publish (write) path: create a machine-9 body, write it, reopen it,
    /// read one byte through the fast path, then drop and remove the file.
    fn publish_one(&mut self) -> Result<(), Box<dyn Error>> {
        let i = self.skew.next_index();
        let body = create_with_dict(&self.dict, &self.records[i])?;
        let physical = body.physical_bytes();
        write_body(&self.publish_path, &body)?;
        let on_disk = alloc_bytes(&self.publish_path);
        let reopened = reader::open(&self.publish_path)?;
        let first = fastpath::byte_at(&reopened, 0)?;
        if first != self.records[i][0] {
            self.sentinel_mismatches += 1;
        }
        self.publish_physical.push(physical);
        self.publish_on_disk.push(on_disk);
        self.publish_ops += 1;
        std::fs::remove_file(&self.publish_path)?;
        Ok(())
    }

    /// The append (non-withdrawal) path: grow the body by one generation and
    /// append it, then record the reopened physical bytes, the on-disk bytes, and
    /// a running logical offset for this generation.
    fn append_one(&mut self) -> Result<(), Box<dyn Error>> {
        if self.append_body.is_none() {
            let body = create_with_dict(&self.dict, &self.append_source)?;
            write_body(&self.append_path, &body)?;
            self.last_physical = reader::open(&self.append_path)?.physical_bytes();
            self.append_body = Some(body);
        }
        let body = self.append_body.as_mut().ok_or("append body missing")?;
        let next = body.generation + 1;
        grow(body, growth_profile(next))?;
        let returned = writer::append_generation(&self.append_path, body)?;

        let reopened = reader::open(&self.append_path)?;
        let physical = reopened.physical_bytes();
        let on_disk = alloc_bytes(&self.append_path);
        if physical < self.last_physical {
            self.append_monotone = false;
        }
        self.last_physical = physical;

        let len = self.append_source.len() as u64;
        let off = match self.append_checks.last() {
            Some(&(_, prev)) => (prev + 131) % len,
            None => 0,
        };
        self.append_checks.push((returned, off));
        self.append_gens += 1;
        self.append_gens_file += 1;
        self.append_max_generation = self.append_max_generation.max(returned);
        self.append_series.push(AppendPoint {
            generation: returned,
            physical_bytes: physical,
            on_disk_bytes: on_disk,
            returned,
        });
        Ok(())
    }

    /// Verify every recorded generation offset of the current append file against
    /// the source, byte-exact. Sticky: a failure is never cleared.
    fn append_verify(&mut self) -> Result<(), Box<dyn Error>> {
        let checks: Vec<(u64, u64)> = self.append_checks.items().to_vec();
        if checks.is_empty() {
            return Ok(());
        }
        let reopened = reader::open(&self.append_path)?;
        let mut ok = true;
        for &(_, off) in &checks {
            if fastpath::byte_at(&reopened, off)? != self.append_source[off as usize] {
                ok = false;
            }
        }
        if !ok {
            self.append_reopen_verified = false;
        }
        Ok(())
    }

    /// Start a fresh append file (the append phase recycles once the generation
    /// cap is reached, so the phase can fill its time budget). Non-withdrawal is a
    /// per-file property: a new file never rewrites an old one.
    fn append_reset(&mut self) {
        let _ = std::fs::remove_file(&self.append_path);
        self.append_body = None;
        self.last_physical = 0;
        self.append_checks.clear();
        self.append_gens_file = 0;
        self.append_cycles += 1;
    }

    fn execute(&mut self) -> Result<(), Box<dyn Error>> {
        self.start = Instant::now();
        let mut cycle = 0u64;
        loop {
            if self.args.cycles != 0 && cycle >= self.args.cycles {
                self.ended_reason = "cycles".to_string();
                break;
            }
            if self.start.elapsed().as_secs_f64() >= self.args.seconds as f64 {
                self.ended_reason = "seconds".to_string();
                break;
            }

            match self.args.phase {
                Phase::Mix => {
                    for _ in 0..DECODE_BATCHES_PER_ROUND {
                        self.decode_batch()?;
                        self.push_sample(None);
                    }
                    self.publish_one()?;
                    self.push_sample(None);
                    if self.append_gens_file < APPEND_CAP {
                        self.append_one()?;
                        self.push_sample(Some(self.last_physical));
                    } else {
                        self.push_sample(None);
                    }
                }
                Phase::Decode => {
                    self.decode_batch()?;
                    self.push_sample(None);
                }
                Phase::Publish => {
                    self.publish_one()?;
                    self.push_sample(None);
                }
                Phase::Append => {
                    if self.append_gens_file >= APPEND_CAP {
                        // Verify the completed file, then start a fresh one so the
                        // phase can fill its time budget rather than ending at the
                        // generation cap.
                        self.append_verify()?;
                        self.append_reset();
                    }
                    self.append_one()?;
                    self.push_sample(Some(self.last_physical));
                }
            }
            cycle += 1;
            if cycle == 1 {
                self.rss_warm = self.rss_end;
                self.in_use_warm = self.in_use_end;
            }
        }
        self.cycles_done = cycle;
        self.elapsed_s = self.start.elapsed().as_secs_f64();

        // Reopen the append file once and verify every recorded generation's
        // logical offset against the source (the helper is sticky on failure).
        self.append_verify()?;

        // ---- real gates: fail loudly, never publish a misleading artifact ----
        if self.sentinel_mismatches != 0 {
            let n = self.sentinel_mismatches;
            return Err(format!("{n} byte-exact sentinel mismatch(es)").into());
        }
        if self.append_gens > 0 && !self.append_monotone {
            return Err("append physical_bytes series is not monotone non-decreasing".into());
        }
        if self.append_gens > 0 && !self.append_reopen_verified {
            return Err("append reopen byte-exactness check failed".into());
        }
        Ok(())
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.publish_path);
        let _ = std::fs::remove_file(&self.append_path);
    }

    /// Build and print the evidence JSON on stdout.
    fn emit(&self) -> Result<(), Box<dyn Error>> {
        let n = self.samples_seen;
        let rss_start = self.rss_start;
        let rss_max = self.rss_max;
        let rss_p50 = median_u64(self.rss_hist.items().to_vec());
        let in_use_start = self.in_use_start;
        let in_use_end = self.in_use_end;
        let in_use_max = self.in_use_max;
        let alloc_peak = PEAK.load(Ordering::Relaxed) as u64;
        let rss_drift = self.rss_end as i64 - self.rss_warm as i64;
        let leak_bytes = self.in_use_end as i64 - self.in_use_warm as i64;

        // Plateau over the time-uniform RSS sample (first half vs last half).
        let hist = self.rss_hist.items();
        let mid = hist.len() / 2;
        let first_half_max = hist[..mid].iter().copied().max().unwrap_or(0);
        let last_half_max = hist[mid..].iter().copied().max().unwrap_or(0);
        let growth = last_half_max.saturating_sub(first_half_max);
        let plateau = first_half_max == 0 || (growth as f64) < 0.05 * first_half_max as f64;

        // A plateau test on half-maxima can hide a slow monotone creep, so report
        // the creep itself: a least-squares RSS slope (B/s) over the second half of
        // the time-uniform sample, and how far that projects over 7 days. Separately
        // report the exact sampled time after which the counting allocator's
        // *live* bytes never change again -- live bytes flat with a creeping RSS is
        // fragmentation (freed-but-retained arenas), not a leak.
        let ser = self.series.items();
        let half = ser.len() / 2;
        let rss_creep_b_per_s = least_squares_slope(ser.get(half..).unwrap_or(&[]));
        let rss_creep_7day_bytes = (rss_creep_b_per_s.max(0.0) * 604_800.0) as u64;
        let alloc_flat_after_ms = {
            let mut last_differing: Option<usize> = None;
            for (i, s) in ser.iter().enumerate() {
                if s.in_use_bytes != in_use_end {
                    last_differing = Some(i);
                }
            }
            match last_differing {
                Some(i) if i + 1 < ser.len() => ser[i + 1].t_ms,
                Some(_) => (self.elapsed_s * 1000.0) as u64,
                None => ser.first().map(|s| s.t_ms).unwrap_or(0),
            }
        };
        let alloc_end_equals_max = in_use_end == in_use_max;
        // Second-half ranges. For an oscillating (sawtooth) live-byte series,
        // "the time after which it never changes again" is the wrong test, so also
        // report the range over the second half and a bounded-oscillation flag:
        // alloc_live_flat = second-half live-byte range <= 1% of its second-half max.
        let tail = ser.get(half..).unwrap_or(&[]);
        let in_use_tail_min = tail.iter().map(|s| s.in_use_bytes).min().unwrap_or(0);
        let in_use_tail_max = tail.iter().map(|s| s.in_use_bytes).max().unwrap_or(0);
        let in_use_tail_range = in_use_tail_max.saturating_sub(in_use_tail_min);
        let alloc_live_flat =
            in_use_tail_max == 0 || (in_use_tail_range as f64) <= 0.01 * in_use_tail_max as f64;
        let rss_tail_min = tail.iter().map(|s| s.rss_bytes).min().unwrap_or(0);
        let rss_tail_max = tail.iter().map(|s| s.rss_bytes).max().unwrap_or(0);
        let rss_tail_range = rss_tail_max.saturating_sub(rss_tail_min);

        // Decode latency: first vs last decile (by time) of the uniform sample.
        let lat = self.lat.items();
        let take = (lat.len() / 10).max(1).min(lat.len());
        let mut fd = lat[..take].to_vec();
        let mut ld = lat[lat.len() - take..].to_vec();
        fd.sort_unstable();
        ld.sort_unstable();
        let fd_p = percentiles(&fd);
        let ld_p = percentiles(&ld);
        let fd_p999 = nearest_rank(&fd, 0.999);
        let ld_p999 = nearest_rank(&ld, 0.999);

        let decode_per_s = self.decode_ops as f64 / self.elapsed_s.max(1e-9);
        let seven_day_ops = (decode_per_s * 86_400.0 * 7.0) as u64;
        let total_ops = self.ops_total();

        let (pmin, pmed, pmax) = min_median_max(self.publish_physical.items());
        let (dmin, dmed, dmax) = min_median_max(self.publish_on_disk.items());
        let alloc_count = ALLOCS.load(Ordering::Relaxed);
        let free_count = FREES.load(Ordering::Relaxed);

        let env = collect_environment();

        let mut js = String::with_capacity(8192);
        js.push_str("{\n");
        jline!(
            js,
            "  \"generated_by\": \"examples/endurance.rs (sh scripts/run-endurance.sh)\","
        );
        jline!(js, "  \"method\": \"{}\",", json_escape(METHOD));
        jline!(js, "  \"caveat\": \"{}\",", json_escape(CAVEAT));
        jline!(js, "  \"seed\": {},", self.args.seed);
        jline!(js, "  \"records\": {RECORDS},");
        jline!(js, "  \"record_bytes\": {RECORD_BYTES},");
        jline!(js, "  \"hot_read_pct\": {HOT_READ_PCT},");
        jline!(js, "  \"hot_set_pct\": {HOT_SET_PCT},");
        jline!(js, "  \"hot_fraction\": {:.4},", self.hot_fraction());
        jline!(js, "  \"seconds_budget\": {},", self.args.seconds);
        jline!(js, "  \"cycles_budget\": {},", self.args.cycles);
        jline!(js, "  \"phase\": \"{}\",", self.args.phase.as_str());
        js.push_str("  \"host\": {\n");
        jline!(js, "    \"pinning\": \"{}\",", json_escape(&env.pinning));
        jline!(js, "    \"governor\": \"{}\",", json_escape(&env.governor));
        jline!(js, "    \"smt\": \"{}\",", json_escape(&env.smt));
        jline!(js, "    \"page_size\": \"{}\"", json_escape(&env.page_size));
        js.push_str("  },\n");

        js.push_str("  \"summary\": {\n");
        jline!(js, "    \"elapsed_s\": {:.3},", self.elapsed_s);
        jline!(js, "    \"cycles\": {},", self.cycles_done);
        jline!(js, "    \"total_ops\": {total_ops},");
        jline!(js, "    \"decode_ops\": {},", self.decode_ops);
        jline!(js, "    \"publish_ops\": {},", self.publish_ops);
        jline!(js, "    \"append_generations\": {},", self.append_gens);
        jline!(js, "    \"decode_per_s\": {decode_per_s:.1},");
        jline!(
            js,
            "    \"sentinel_mismatches\": {},",
            self.sentinel_mismatches
        );
        jline!(js, "    \"ended_reason\": \"{}\"", self.ended_reason);
        js.push_str("  },\n");

        js.push_str("  \"decode_latency\": {\n");
        jline!(js, "    \"sample_rule\": \"{}\",", json_escape(LAT_RULE));
        jline!(js, "    \"sampled\": {},", lat.len());
        jline!(
            js,
            "    \"first_decile\": {{ \"ops\": {take}, \"p50\": {:.1}, \"p99\": {:.1}, \"p99_9\": {fd_p999:.1} }},",
            fd_p.p50,
            fd_p.p99
        );
        jline!(
            js,
            "    \"last_decile\": {{ \"ops\": {take}, \"p50\": {:.1}, \"p99\": {:.1}, \"p99_9\": {ld_p999:.1} }}",
            ld_p.p50,
            ld_p.p99
        );
        js.push_str("  },\n");

        js.push_str("  \"memory\": {\n");
        jline!(js, "    \"rss_start\": {rss_start},");
        jline!(js, "    \"rss_p50\": {rss_p50},");
        jline!(js, "    \"rss_max\": {rss_max},");
        jline!(js, "    \"rss_drift_bytes\": {rss_drift},");
        jline!(js, "    \"peak_rss\": {},", peak_rss_bytes());
        jline!(js, "    \"alloc_in_use_start\": {in_use_start},");
        jline!(js, "    \"alloc_in_use_end\": {in_use_end},");
        jline!(js, "    \"alloc_in_use_max\": {in_use_max},");
        jline!(js, "    \"alloc_peak\": {alloc_peak},");
        jline!(js, "    \"leak_bytes\": {leak_bytes},");
        jline!(js, "    \"alloc_count\": {alloc_count},");
        jline!(js, "    \"free_count\": {free_count},");
        jline!(
            js,
            "    \"rss_minus_in_use_max\": {},",
            rss_max.saturating_sub(in_use_max)
        );
        jline!(js, "    \"samples\": {n},");
        jline!(js, "    \"plateau\": {plateau},");
        jline!(
            js,
            "    \"plateau_rule\": \"{}\",",
            json_escape(PLATEAU_RULE)
        );
        jline!(
            js,
            "    \"rss_creep_bytes_per_s_last_half\": {rss_creep_b_per_s:.3},"
        );
        jline!(js, "    \"rss_creep_7day_bytes\": {rss_creep_7day_bytes},");
        jline!(
            js,
            "    \"rss_creep_rule\": \"{}\",",
            json_escape(CREEP_RULE)
        );
        jline!(js, "    \"alloc_flat_after_ms\": {alloc_flat_after_ms},");
        jline!(js, "    \"alloc_end_equals_max\": {alloc_end_equals_max},");
        jline!(js, "    \"alloc_live_tail_min\": {in_use_tail_min},");
        jline!(js, "    \"alloc_live_tail_max\": {in_use_tail_max},");
        jline!(js, "    \"alloc_live_tail_range\": {in_use_tail_range},");
        jline!(js, "    \"alloc_live_flat\": {alloc_live_flat},");
        jline!(js, "    \"rss_second_half_range\": {rss_tail_range},");
        js.push_str("    \"series\": [\n");
        let series = downsample(self.series.items(), SERIES_MAX);
        for (i, s) in series.iter().enumerate() {
            let comma = if i + 1 == series.len() { "" } else { "," };
            let physical = match s.physical_bytes {
                Some(p) => p.to_string(),
                None => "null".to_string(),
            };
            jline!(
                js,
                "      {{ \"t_ms\": {}, \"rss_bytes\": {}, \"in_use_bytes\": {}, \"peak_alloc_bytes\": {}, \"alloc_count\": {}, \"free_count\": {}, \"ops_total\": {}, \"physical_bytes\": {physical} }}{comma}",
                s.t_ms,
                s.rss_bytes,
                s.in_use_bytes,
                s.peak_alloc_bytes,
                s.alloc_count,
                s.free_count,
                s.ops_total
            );
        }
        js.push_str("    ]\n");
        js.push_str("  },\n");

        js.push_str("  \"physical_bytes\": {\n");
        jline!(
            js,
            "    \"publish_physical_bytes\": {{ \"min\": {pmin}, \"median\": {pmed}, \"max\": {pmax} }},"
        );
        jline!(
            js,
            "    \"publish_on_disk_bytes\": {{ \"min\": {dmin}, \"median\": {dmed}, \"max\": {dmax} }},"
        );
        jline!(js, "    \"append_monotone\": {},", self.append_monotone);
        jline!(
            js,
            "    \"append_reopen_verified\": {},",
            self.append_reopen_verified
        );
        jline!(js, "    \"append_generations\": {},", self.append_gens);
        jline!(js, "    \"append_files\": {},", self.append_cycles + 1);
        jline!(
            js,
            "    \"append_max_generation\": {},",
            self.append_max_generation
        );
        js.push_str("    \"append_series\": [\n");
        let points = downsample(self.append_series.items(), SERIES_MAX);
        for (i, p) in points.iter().enumerate() {
            let comma = if i + 1 == points.len() { "" } else { "," };
            jline!(
                js,
                "      {{ \"generation\": {}, \"physical_bytes\": {}, \"on_disk_bytes\": {}, \"returned\": {} }}{comma}",
                p.generation,
                p.physical_bytes,
                p.on_disk_bytes,
                p.returned
            );
        }
        js.push_str("    ]\n");
        js.push_str("  },\n");

        js.push_str("  \"extrapolation\": {\n");
        jline!(js, "    \"basis\": \"decode_per_s * 86400 * 7\",");
        jline!(js, "    \"seven_day_seconds\": 604800,");
        jline!(js, "    \"seven_day_decode_ops\": {seven_day_ops}");
        js.push_str("  },\n");

        jline!(
            js,
            "  \"fragmentation_method\": \"{}\"",
            json_escape(FRAG_METHOD)
        );
        js.push_str("}\n");

        print!("{js}");
        Ok(())
    }

    /// The measured hot fraction (fraction of draws landing in the hot set).
    fn hot_fraction(&self) -> f64 {
        let hot = (RECORDS * HOT_SET_PCT / 100).max(1);
        let mut probe = Skew::new(self.args.seed ^ 0x9E37_79B9_7F4A_7C15, RECORDS, hot);
        let draws = 100_000usize;
        let mut hot_draws = 0usize;
        for _ in 0..draws {
            if probe.next_index() < hot {
                hot_draws += 1;
            }
        }
        hot_draws as f64 / draws as f64
    }
}

/// A deterministic growth profile for generation `generation` (non-withdrawal:
/// each call appends structures and never removes them).
fn growth_profile(generation: u64) -> ManifestationProfile {
    match generation % 4 {
        0 => ManifestationProfile::Sparse { interval: 256 },
        1 => ManifestationProfile::Sparse { interval: 1024 },
        2 => ManifestationProfile::Dense { interval: 4096 },
        _ => ManifestationProfile::Sparse { interval: 4096 },
    }
}

/// `du`-style on-disk bytes of `path` (`st_blocks * 512`, at least `len`).
fn alloc_bytes(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.blocks().saturating_mul(512).max(m.len()),
        Err(_) => 0,
    }
}

/// Least-squares slope of RSS (bytes) against time (seconds), in B/s. `xs` must be
/// time-ordered (the halving sampler preserves time order); an empty or
/// single-point slice, or a degenerate time span, yields `0.0`.
fn least_squares_slope(xs: &[Sample]) -> f64 {
    let n = xs.len() as f64;
    if xs.len() < 2 {
        return 0.0;
    }
    let t0 = xs[0].t_ms as f64;
    let mean_x = xs
        .iter()
        .map(|s| (s.t_ms as f64 - t0) / 1000.0)
        .sum::<f64>()
        / n;
    let mean_y = xs.iter().map(|s| s.rss_bytes as f64).sum::<f64>() / n;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for s in xs {
        let x = (s.t_ms as f64 - t0) / 1000.0 - mean_x;
        let y = s.rss_bytes as f64 - mean_y;
        num += x * y;
        den += x * x;
    }
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

fn median_u64(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn min_median_max(v: &[u64]) -> (u64, u64, u64) {
    if v.is_empty() {
        return (0, 0, 0);
    }
    let min = v.iter().copied().min().unwrap_or(0);
    let max = v.iter().copied().max().unwrap_or(0);
    (min, median_u64(v.to_vec()), max)
}

/// Nearest-rank quantile `q` over an ascending sample (the same definition as
/// `ekbloom::percentiles`, extended to p99.9, which that summary does not carry).
fn nearest_rank(sorted: &[u64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let rank = (q * n as f64).ceil();
    let idx = (rank as usize).saturating_sub(1).min(n - 1);
    sorted[idx] as f64
}

/// Evenly-spaced downsample of `items` to at most `max` entries (endpoints kept).
fn downsample<T: Copy>(items: &[T], max: usize) -> Vec<T> {
    if items.len() <= max {
        return items.to_vec();
    }
    let step = (items.len() - 1) as f64 / (max - 1) as f64;
    let mut out = Vec::with_capacity(max);
    for k in 0..max {
        let idx = (k as f64 * step).round() as usize;
        out.push(items[idx.min(items.len() - 1)]);
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
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

const METHOD: &str = "TIME-COMPRESSED machine-9 endurance: one process sustaining a deterministic mixed read (decode) / write (publish) / append (non-withdrawal) workload over 20,000 x 100-byte records shared through a trained OrANS-DICT dictionary; a counting global allocator reports live bytes and counts, RSS/VmHWM come from /proc/self/status via ekbloom::current_rss_bytes / peak_rss_bytes; every batch pushes a time-series sample";

const CAVEAT: &str = "This is a TIME-COMPRESSED run, not a 7-day run. The operation scheduling is deterministic from the seed regardless of the budget (only the count varies with wall-clock), but the absolute rates and the 7-day decode-operation count are host-dependent and are an operation-count projection only: they do not model sustained filesystem growth, allocator arena retention, page-cache pressure, or thermal/clock drift over days.";

const LAT_RULE: &str = "per-decode-op latency, one time-ordered uniform sample capped at 2^18 entries (each time the cap is hit, every other sample is dropped and the stride doubles, preserving the first and last regions); first_decile = the first 10% of the sample by time, last_decile = the last 10% by time; p50/p99 from ekbloom::percentiles, p99_9 by the same nearest-rank definition";

const PLATEAU_RULE: &str = "plateau = (max RSS over the second half of samples - max RSS over the first half) < 5% of the first-half max";

const CREEP_RULE: &str = "rss_creep_bytes_per_s_last_half = least-squares slope of rss_bytes against time over the second half of the time-uniform sample (B/s); rss_creep_7day_bytes = max(0, slope) * 604800, an operation-count-style projection ONLY. The live-memory leak test is alloc_live_flat: the second-half live-byte range must be <= 1% of its second-half max (a bounded sawtooth counts as flat, which a 'never changes again' test would wrongly reject); alloc_end_equals_max and alloc_flat_after_ms are reported alongside";

const FRAG_METHOD: &str = "alloc_in_use is what the counting global allocator holds live; rss_bytes is the kernel resident set (VmRSS) read from /proc/self/status; rss_minus_in_use_max = rss_max - alloc_in_use_max is the non-allocator resident proxy (stacks, code, page tables, allocator metadata and retained arenas); leak_bytes = alloc_in_use_end - alloc_in_use_at_end_of_first_cycle; rss_drift_bytes = last RSS - RSS at end of the first cycle (after warmup); alloc_flat_after_ms = the first sampled time after which in_use_bytes never changes again (the exact live-memory plateau); alloc_end_equals_max = in_use_end == alloc_in_use_max";

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("endurance: {e}");
            std::process::exit(2);
        }
    };
    let mut run = match Run::new(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("endurance: {e}");
            std::process::exit(1);
        }
    };
    let result = run.execute().and_then(|()| run.emit());
    run.cleanup();
    if let Err(e) = result {
        eprintln!("endurance: {e}");
        std::process::exit(1);
    }
}
