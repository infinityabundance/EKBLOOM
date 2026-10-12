//! End-to-end EKBLOOM/OrFS proof on **real data**.
//!
//! One EKBLOOM/OrFS object is measured from **creation** through **persistence**,
//! **reopening** and **sustained use**, and compared at an **equal total on-disk
//! budget** against three strong conventional file stores built under the same
//! rules:
//!
//! * a **raw file** store (the source bytes, `pread`),
//! * a **pinned-hot indexed LZ4** file store, and
//! * a **pinned-hot indexed Zstandard** file store.
//!
//! The last two retain their decoded hot prefix **permanently** ([`PinnedHotCache`])
//! and count those decoded bytes in their footprint — the same retained-hot policy,
//! and the strongest conventional *non-evicting* competitor.
//!
//! Corpora are **real files inside the container** ([`common::load`]): a real ELF
//! executable, a text/licence corpus, and a mixed/binary aggregate. Each records
//! its BLAKE3 digest and size. On-disk bytes are the real `du`-style allocation
//! ([`common::du_bytes`]), asserted equal to `OrFs::inspect_physical`.
//!
//! Writes `evidence/bench/e2e.json`; run by `sh scripts/run-e2e.sh`. The optional
//! FUSE section mounts the object and measures end-to-end filesystem read latency
//! (skipped, and recorded as such, when `/dev/fuse` is unavailable).
//!
//! Every EKBLOOM read (direct `ReadyObject`, persistent `OpenObject`, and the FUSE
//! projection) is asserted **byte-exact** against the source before any timing.

#[path = "../tests/common/e2e.rs"]
mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[cfg(all(feature = "fuse", target_os = "linux"))]
use std::os::unix::fs::FileExt;

use ekbloom::engine::ReadyObject;
use ekbloom::{
    now_ns, AccessTrace, CourtConfig, DirectoryKind, EkbloomBody, ManifestationBudget,
    OptimizedPlan, OrFs, ReadRequest, TileSet,
};

#[cfg(all(feature = "fuse", target_os = "linux"))]
use ekbloom::mount;

use common::{Corpus, Store};

/// Representative read length for the hot/random/scan measurements (a page).
const READ_BYTES: u64 = 4096;
/// Hot working set measured/retained at the start of each object.
const HOT_BYTES: u64 = 256 * 1024;
/// Hot 4 KiB samples per row.
const HOT_SAMPLES: usize = 96;
/// Random 4 KiB samples per row.
const RANDOM_SAMPLES: usize = 96;
/// Warmup passes over the hot set before sampling.
const WARMUP: usize = 2;
/// Sequential-read chunk size.
const SEQ_CHUNK: u64 = 64 * 1024;
/// FUSE full-read samples.
#[cfg(all(feature = "fuse", target_os = "linux"))]
const FUSE_FULL_SAMPLES: usize = 8;

/// `(p50, p99)` and throughput for one contender's sustained workload.
#[derive(Clone, Copy, Debug)]
struct Sustained {
    seq_mbps: f64,
    hot_p50: u64,
    hot_p99: u64,
    rand_p50: u64,
    rand_p99: u64,
    scan_hot_p50: u64,
    scan_hot_p99: u64,
}

impl Sustained {
    fn json(&self, indent: &str) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{indent}\"seq_mbps\": {:.1},", finite(self.seq_mbps));
        let _ = writeln!(s, "{indent}\"hot4k_p50_ns\": {},", self.hot_p50);
        let _ = writeln!(s, "{indent}\"hot4k_p99_ns\": {},", self.hot_p99);
        let _ = writeln!(s, "{indent}\"random4k_p50_ns\": {},", self.rand_p50);
        let _ = writeln!(s, "{indent}\"random4k_p99_ns\": {},", self.rand_p99);
        let _ = writeln!(
            s,
            "{indent}\"scan_then_hot4k_p50_ns\": {},",
            self.scan_hot_p50
        );
        let _ = writeln!(
            s,
            "{indent}\"scan_then_hot4k_p99_ns\": {}",
            self.scan_hot_p99
        );
        s
    }
}

/// The chosen plan's identity, for the evidence record.
#[derive(Clone, Debug)]
struct PlanMeta {
    optimized: bool,
    machine: u32,
    region_bytes: u64,
    restart: String,
    tile_shift: Option<u8>,
    hot_regions: usize,
    committed_bytes: u64,
    expected_p99_ns: f64,
    worst_case_steps: u64,
    over_budget: bool,
    summary: String,
}

/// A non-negative finite view of `v` (guards JSON validity and ratio math).
fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// JSON-escape a string into a quoted JSON string literal.
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

/// A hot-dominant held-out access trace: the plan is scored where the workload
/// actually concentrates (the hot prefix), so hot manifestation is the thing the
/// optimiser can improve. Cold coverage of every region is still mandatory for a
/// candidate to be admitted, so a bounded cold path is always planned.
fn access_trace(hot_bytes: u64) -> AccessTrace {
    AccessTrace {
        regions: vec![(0, hot_bytes.max(1))],
        counts: vec![1000],
    }
}

/// Measure one contender's sustained workload: warm hot 4 KiB reads, random 4 KiB
/// reads, a full sequential scan (throughput), then hot reads **again** (the
/// non-eviction check). Byte-exact before timing.
fn measure_sustained<F: FnMut(u64, u64) -> Vec<u8>>(
    source: &[u8],
    hot: &[u64],
    rand: &[u64],
    mut read: F,
) -> Sustained {
    let n = source.len() as u64;
    for &off in hot.iter().chain(rand.iter()).take(16) {
        let got = read(off, READ_BYTES);
        assert_eq!(
            got,
            &source[off as usize..(off + READ_BYTES) as usize],
            "read at offset {off} is not byte-exact"
        );
    }
    for _ in 0..WARMUP {
        for &off in hot {
            std::hint::black_box(read(off, READ_BYTES));
        }
    }
    let mut hot_samples = Vec::with_capacity(hot.len());
    for &off in hot {
        let t0 = now_ns();
        let got = read(off, READ_BYTES);
        hot_samples.push(now_ns().saturating_sub(t0));
        std::hint::black_box(got);
    }
    let mut rand_samples = Vec::with_capacity(rand.len());
    for &off in rand {
        let t0 = now_ns();
        let got = read(off, READ_BYTES);
        rand_samples.push(now_ns().saturating_sub(t0));
        std::hint::black_box(got);
    }
    let t0 = now_ns();
    let mut pos = 0u64;
    while pos < n {
        let len = SEQ_CHUNK.min(n - pos);
        std::hint::black_box(read(pos, len));
        pos += len;
    }
    let seq_ns = now_ns().saturating_sub(t0).max(1);
    let seq_mbps = (n as f64 / (1024.0 * 1024.0)) / (seq_ns as f64 / 1e9);
    let mut scan_samples = Vec::with_capacity(hot.len());
    for &off in hot {
        let t0 = now_ns();
        let got = read(off, READ_BYTES);
        scan_samples.push(now_ns().saturating_sub(t0));
        std::hint::black_box(got);
    }
    let (hot_p50, hot_p99) = common::summarize(hot_samples);
    let (rand_p50, rand_p99) = common::summarize(rand_samples);
    let (scan_hot_p50, scan_hot_p99) = common::summarize(scan_samples);
    Sustained {
        seq_mbps,
        hot_p50,
        hot_p99,
        rand_p50,
        rand_p99,
        scan_hot_p50,
        scan_hot_p99,
    }
}

/// The workload the bound engine plans against: hot-set reads at the object start.
fn workload_for(hot_bytes: u64) -> ekbloom::Workload {
    ekbloom::Workload {
        kind: ekbloom::WorkloadKind::HotSet,
        hot_prefix: Some(hot_bytes),
        sample: vec![(0, READ_BYTES)],
    }
}

/// A built plan: the body, its metadata, and the engine regions it manifested as
/// tiles (empty when the plan is cold-only).
struct BuiltPlan {
    body: EkbloomBody,
    meta: PlanMeta,
    tiled: Vec<(u64, u64)>,
}

/// Build the best body the joint optimiser admits under `budget`. When it cannot
/// admit any plan (an incompressible corpus under a raw-size budget), fall back to
/// the bounded-access engine configuration and flag that the body exceeds budget.
fn build_plan(source: &[u8], trace: &AccessTrace, budget: u64) -> ekbloom::Result<BuiltPlan> {
    let manifest_budget = ManifestationBudget {
        maximum_physical_bytes: budget,
    };
    let cfg = CourtConfig {
        reps: 1,
        interleave: true,
        seed: 0x0E2E_0000_0000_BEEF,
    };
    if let Ok((plan, _receipts)) = OptimizedPlan::optimize(source, trace, &manifest_budget, &cfg) {
        let body = plan.build(source)?;
        let meta = PlanMeta {
            optimized: true,
            machine: plan.machine_id(),
            region_bytes: plan.region_bytes(),
            restart: plan.restart().label(),
            tile_shift: plan.tile_shift(),
            hot_regions: plan.hot_regions().len(),
            committed_bytes: plan.committed_bytes(),
            expected_p99_ns: plan.expected_p99(),
            worst_case_steps: 0,
            over_budget: plan.committed_bytes() > budget,
            summary: plan.summary(),
        };
        let tiled = plan.hot_regions().to_vec();
        return Ok(BuiltPlan { body, meta, tiled });
    }
    // Fallback: the bounded-access engine configuration (hot tiles + a cold block
    // index) at the frontier budget `max(n, core) + n/8`.
    let n = source.len() as u64;
    let mut body = ekbloom::create_auto(source, ekbloom::ManifestationProfile::Root)?;
    let core_len = body.core.len() as u64;
    let frontier = n.max(core_len) + n / 8;
    let fbudget = ManifestationBudget {
        maximum_physical_bytes: frontier,
    };
    let tile_shift = READ_BYTES.trailing_zeros() as u8;
    TileSet::manifest_hot_tiles(
        &mut body,
        trace,
        tile_shift,
        DirectoryKind::TwoLevelRadix,
        &fbudget,
    )?;
    let block = ekbloom::BlockIndex::build_machine(&body.core, n, 1024, body.machine_id)?;
    body.blocks.push(block);
    body.generation += 1;
    let meta = PlanMeta {
        optimized: false,
        machine: body.machine_id,
        region_bytes: READ_BYTES,
        restart: "B1024".to_string(),
        tile_shift: Some(tile_shift),
        hot_regions: trace.regions.len(),
        committed_bytes: body.physical_bytes(),
        expected_p99_ns: 0.0,
        worst_case_steps: 0,
        over_budget: body.physical_bytes() > budget,
        summary: format!(
            "fallback: machine {} / hot tiles + a 1024-byte cold block index / {} committed B",
            body.machine_id,
            body.physical_bytes()
        ),
    };
    let tiled = trace.regions.clone();
    Ok(BuiltPlan { body, meta, tiled })
}

/// The FUSE end-to-end latency row for one object.
#[derive(Clone, Debug)]
struct FuseMetrics {
    available: bool,
    error: String,
    hot_p50: u64,
    hot_p99: u64,
    rand_p50: u64,
    rand_p99: u64,
    full_mbps: f64,
    direct_ready_hot_p50: u64,
    raw_hot_p50: u64,
}

/// One corpus's complete end-to-end measurement.
struct CorpusReport {
    name: &'static str,
    kind: &'static str,
    origin: String,
    source_len: u64,
    source_blake3: String,
    hot_bytes: u64,
    plan: PlanMeta,
    plan_build_ns: u64,
    create_persist_ns: u64,
    orfs_open_ns: u64,
    openobject_open_ns: u64,
    ready_prepare_ns: u64,
    orfs_object_bytes: u64,
    inspect_total_bytes: u64,
    manifests_allocated_bytes: u64,
    raw_bytes: u64,
    lz4_bytes: u64,
    zstd_bytes: u64,
    budget_bytes: u64,
    lz4_pinned_bytes: u64,
    zstd_pinned_bytes: u64,
    orfs: Sustained,
    ready: Sustained,
    raw: Sustained,
    lz4: Sustained,
    zstd: Sustained,
    fuse: Option<FuseMetrics>,
}

impl CorpusReport {
    fn orfs_within_budget(&self) -> bool {
        self.orfs_object_bytes <= self.budget_bytes
    }
    fn lz4_within_budget(&self) -> bool {
        self.lz4_bytes <= self.budget_bytes
    }
    fn zstd_within_budget(&self) -> bool {
        self.zstd_bytes <= self.budget_bytes
    }
}

/// Measure one corpus from creation through persistence, reopening and sustained
/// use, against the three conventional stores at one equal on-disk budget.
fn measure_corpus(
    corpus: &Corpus,
    scratch: &Path,
    want_fuse: bool,
) -> ekbloom::Result<CorpusReport> {
    let source = &corpus.source;
    let n = source.len() as u64;
    let hot_bytes = HOT_BYTES.min(n / 2).max(READ_BYTES);
    let logical = "data/obj.bin";

    // (0) Conventional stores, built on real files.
    let mut raw = Store::raw(&scratch.join(format!("{}-raw", corpus.name)), source)?;
    let mut lz4 = Store::lz4(
        &scratch.join(format!("{}-lz4", corpus.name)),
        source,
        hot_bytes,
    )?;
    let mut zstd = Store::zstd(
        &scratch.join(format!("{}-zstd", corpus.name)),
        source,
        hot_bytes,
    )?;
    let raw_bytes = raw.on_disk_bytes();
    let lz4_bytes = lz4.on_disk_bytes();
    let zstd_bytes = zstd.on_disk_bytes();
    // The equal total on-disk budget is the raw file store's real allocation.
    let budget = raw_bytes;

    // (1) Create/persist: an optimal plan, materialised into a real OrFS object.
    let trace = access_trace(hot_bytes);
    let t0 = now_ns();
    let built = build_plan(source, &trace, budget)?;
    let plan_build_ns = now_ns().saturating_sub(t0);
    let mut plan = built.meta;
    let body = built.body;
    let tiled = built.tiled;

    let root = scratch.join(format!("root-{}", corpus.name));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    let fs = OrFs::open(&root)?;
    let t0 = now_ns();
    fs.create_body(logical, &body)?;
    let create_persist_ns = now_ns().saturating_sub(t0);
    drop(body);

    // Real on-disk bytes, asserted equal to the administrative report.
    let object_dir_bytes = common::du_bytes(&root.join(".ekbloom"));
    let pointer_bytes = common::du_bytes(&root.join(logical));
    let orfs_object_bytes = object_dir_bytes.saturating_add(pointer_bytes);
    let inspect_total_bytes = fs.inspect_physical()?.total_bytes;
    assert_eq!(
        inspect_total_bytes, orfs_object_bytes,
        "inspect_physical must equal the real concealed+artifact bytes"
    );
    let ekb = common::concealed_ekb(&root);
    let manifests_allocated_bytes = common::du_bytes(&ekb);
    drop(fs);

    // (2) Reopen the OrFS namespace, the persistent handle, and a ReadyObject.
    let t0 = now_ns();
    let fs = OrFs::open(&root)?;
    let orfs_open_ns = now_ns().saturating_sub(t0);
    let t0 = now_ns();
    let handle = ekbloom::OpenObject::open(&ekb)?;
    let openobject_open_ns = now_ns().saturating_sub(t0);
    let t0 = now_ns();
    let reopened = ekbloom::reader::open(&ekb)?;
    let ready = ReadyObject::prepare_with(
        reopened,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
        &workload_for(hot_bytes),
    )?;
    let ready_prepare_ns = now_ns().saturating_sub(t0);
    plan.worst_case_steps = ready.worst_case_steps();

    // A byte-exact spot check through the persistent handle.
    let probe = handle.read(ReadRequest {
        offset: 0,
        length: READ_BYTES,
    })?;
    assert_eq!(
        probe,
        &source[..READ_BYTES as usize],
        "handle read not byte-exact"
    );

    // (3) Sustained use. Byte-exactness is asserted inside each measurement.
    let hot = {
        let manifested = common::hot_in_regions(&tiled, HOT_SAMPLES);
        if manifested.is_empty() {
            common::hot_offsets(hot_bytes, HOT_SAMPLES)
        } else {
            manifested
        }
    };
    let rand = common::random_offsets(n, RANDOM_SAMPLES, 0xE2E0_0000_0000_1234);
    let orfs = measure_sustained(source, &hot, &rand, |offset, length| {
        fs.read(logical, ReadRequest { offset, length })
            .expect("orfs read")
    });
    let ready_metrics = measure_sustained(source, &hot, &rand, |offset, length| {
        ready
            .read(ReadRequest { offset, length })
            .expect("ready read")
    });
    let raw_metrics = measure_sustained(source, &hot, &rand, |o, l| raw.read(o, l));
    let lz4_metrics = measure_sustained(source, &hot, &rand, |o, l| lz4.read(o, l));
    let zstd_metrics = measure_sustained(source, &hot, &rand, |o, l| zstd.read(o, l));

    // (5) FUSE end-to-end read latency (skipped when unavailable).
    let fuse = if want_fuse {
        measure_fuse(
            &root, logical, source, &hot, &rand, &mut raw, &ready, scratch,
        )
    } else {
        None
    };

    Ok(CorpusReport {
        name: corpus.name,
        kind: corpus.kind,
        origin: corpus.origin.clone(),
        source_len: n,
        source_blake3: common::blake3_hex(source),
        hot_bytes,
        plan,
        plan_build_ns,
        create_persist_ns,
        orfs_open_ns,
        openobject_open_ns,
        ready_prepare_ns,
        orfs_object_bytes,
        inspect_total_bytes,
        manifests_allocated_bytes,
        raw_bytes,
        lz4_bytes,
        zstd_bytes,
        budget_bytes: budget,
        lz4_pinned_bytes: lz4.pinned_bytes(),
        zstd_pinned_bytes: zstd.pinned_bytes(),
        orfs,
        ready: ready_metrics,
        raw: raw_metrics,
        lz4: lz4_metrics,
        zstd: zstd_metrics,
        fuse,
    })
}

/// A zeroed FUSE row (only the real FUSE path uses it).
#[cfg(all(feature = "fuse", target_os = "linux"))]
fn zero_fuse() -> FuseMetrics {
    FuseMetrics {
        available: false,
        error: String::new(),
        hot_p50: 0,
        hot_p99: 0,
        rand_p50: 0,
        rand_p99: 0,
        full_mbps: 0.0,
        direct_ready_hot_p50: 0,
        raw_hot_p50: 0,
    }
}

/// `num / den`, safe for `den == 0`.
fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

/// The explicit, honest per-corpus verdict: which axis EKBLOOM wins, by how much,
/// and where it loses.
///
/// `holds` is the **strict** tradeoff against the strongest conventional stores:
/// within the equal budget, strictly smaller than **both** pinned-hot stores, the
/// better of EKBLOOM's direct (`ReadyObject`) and filesystem (`OrFs`) hot 4 KiB p50
/// within 3× of the better pinned store, and non-evicting across a scan.
///
/// `beats_raw` is the tradeoff EKBLOOM wins against a **raw file store** at the
/// equal on-disk budget: smaller on disk *and* the direct hot 4 KiB p50 no worse
/// than the raw `pread`, non-evicting across a scan.
fn corpus_verdict(r: &CorpusReport) -> (bool, bool, String) {
    let within = r.orfs_within_budget();
    let smaller_lz4 = r.orfs_object_bytes < r.lz4_bytes;
    let smaller_zstd = r.orfs_object_bytes < r.zstd_bytes;
    let pinned_hot = r.lz4.hot_p50.max(r.zstd.hot_p50);
    let ek_hot = r.orfs.hot_p50.min(r.ready.hot_p50);
    let hot_ok = ek_hot <= pinned_hot.saturating_mul(3).max(1000);
    let non_evict = r.orfs.scan_hot_p50 <= r.orfs.hot_p50.saturating_mul(3).max(2000);
    let holds = within && smaller_lz4 && smaller_zstd && hot_ok && non_evict;
    let beats_raw = within
        && r.orfs_object_bytes < r.raw_bytes
        && ek_hot <= r.raw.hot_p50.max(1000)
        && non_evict;
    let text = format!(
        "within_equal_budget={within}; beats_raw_store={beats_raw}; orfs={} B ({:.2}x raw, {:.2}x pinned-LZ4, {:.2}x pinned-Zstd); \
         hot4k_p50: orfs {} ns / ready {} ns vs raw {} / LZ4 {} / Zstd {}; scan_then_hot4k_p50={} ns; \
         non_evicting={non_evict}; hot_competitive_vs_pinned={hot_ok}",
        r.orfs_object_bytes,
        ratio(r.orfs_object_bytes, r.raw_bytes),
        ratio(r.orfs_object_bytes, r.lz4_bytes),
        ratio(r.orfs_object_bytes, r.zstd_bytes),
        r.orfs.hot_p50,
        r.ready.hot_p50,
        r.raw.hot_p50,
        r.lz4.hot_p50,
        r.zstd.hot_p50,
        r.orfs.scan_hot_p50,
    );
    (holds, beats_raw, text)
}

/// Measure end-to-end FUSE read latency against the direct `ReadyObject` read and
/// the raw-file baseline. Returns `None` when the `fuse` feature is off.
#[cfg(not(all(feature = "fuse", target_os = "linux")))]
#[allow(clippy::too_many_arguments)]
fn measure_fuse(
    _root: &Path,
    _logical: &str,
    _source: &[u8],
    _hot: &[u64],
    _rand: &[u64],
    _raw: &mut Store,
    _ready: &ReadyObject,
    _scratch: &Path,
) -> Option<FuseMetrics> {
    None
}

/// Mount the OrFS object read-only and measure end-to-end filesystem read latency.
#[cfg(all(feature = "fuse", target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
fn measure_fuse(
    root: &Path,
    logical: &str,
    source: &[u8],
    hot: &[u64],
    rand: &[u64],
    raw: &mut Store,
    ready: &ReadyObject,
    scratch: &Path,
) -> Option<FuseMetrics> {
    let mnt = scratch.join("fuse-mnt");
    let _ = std::fs::remove_dir_all(&mnt);
    std::fs::create_dir_all(&mnt).ok()?;
    let fs = OrFs::open(root).ok()?;
    let handle = match mount(fs, &mnt, false) {
        Ok(handle) => handle,
        Err(e) => {
            return Some(FuseMetrics {
                available: false,
                error: e.to_string(),
                ..zero_fuse()
            })
        }
    };
    let path = mnt.join(logical);
    let whole = std::fs::read(&path).ok()?;
    if whole != source {
        return Some(FuseMetrics {
            available: true,
            error: "mounted read is not byte-exact".to_string(),
            ..zero_fuse()
        });
    }
    let n = source.len() as u64;
    let file = std::fs::File::open(&path).ok()?;
    let mut buf = vec![0u8; READ_BYTES as usize];
    file.read_exact_at(&mut buf, hot.first().copied().unwrap_or(0))
        .ok()?;
    let mut hot_samples = Vec::with_capacity(hot.len());
    for &off in hot {
        let t0 = now_ns();
        file.read_exact_at(&mut buf, off).ok()?;
        hot_samples.push(now_ns().saturating_sub(t0));
    }
    let mut rand_samples = Vec::with_capacity(rand.len());
    for &off in rand {
        let t0 = now_ns();
        file.read_exact_at(&mut buf, off).ok()?;
        rand_samples.push(now_ns().saturating_sub(t0));
    }
    let mut full_ns = 0u64;
    for _ in 0..FUSE_FULL_SAMPLES {
        let t0 = now_ns();
        let got = std::fs::read(&path).ok()?;
        full_ns = full_ns.saturating_add(now_ns().saturating_sub(t0));
        if got.len() as u64 != n {
            return None;
        }
    }
    let full_mbps =
        (n as f64 / (1024.0 * 1024.0)) / ((full_ns as f64 / FUSE_FULL_SAMPLES as f64) / 1e9);
    let mut direct = Vec::with_capacity(hot.len());
    for &off in hot {
        let t0 = now_ns();
        let _ = ready.read(ReadRequest {
            offset: off,
            length: READ_BYTES,
        });
        direct.push(now_ns().saturating_sub(t0));
    }
    let mut raw_hot = Vec::with_capacity(hot.len());
    for &off in hot {
        let t0 = now_ns();
        std::hint::black_box(raw.read(off, READ_BYTES));
        raw_hot.push(now_ns().saturating_sub(t0));
    }
    let (hot_p50, hot_p99) = common::summarize(hot_samples);
    let (rand_p50, rand_p99) = common::summarize(rand_samples);
    let (direct_ready_hot_p50, _) = common::summarize(direct);
    let (raw_hot_p50, _) = common::summarize(raw_hot);
    // Close every handle on the mounted filesystem before unmounting: an open fd
    // would make the unmount fail with EBUSY and block the session join.
    drop(file);
    drop(buf);
    drop(handle);
    Some(FuseMetrics {
        available: true,
        error: String::new(),
        hot_p50,
        hot_p99,
        rand_p50,
        rand_p99,
        full_mbps: finite(full_mbps),
        direct_ready_hot_p50,
        raw_hot_p50,
    })
}

/// Render one corpus's JSON object.
fn render_corpus(r: &CorpusReport) -> String {
    let (holds, beats_raw, verdict) = corpus_verdict(r);
    let mut s = String::new();
    s.push_str("    {\n");
    let _ = writeln!(s, "      \"name\": \"{}\",", r.name);
    let _ = writeln!(s, "      \"kind\": \"{}\",", r.kind);
    let _ = writeln!(s, "      \"origin\": \"{}\",", json_escape(&r.origin));
    let _ = writeln!(s, "      \"source_len\": {},", r.source_len);
    let _ = writeln!(s, "      \"source_blake3\": \"{}\",", r.source_blake3);
    let _ = writeln!(s, "      \"hot_bytes\": {},", r.hot_bytes);

    s.push_str("      \"plan\": {\n");
    let _ = writeln!(s, "        \"optimized\": {},", r.plan.optimized);
    let _ = writeln!(s, "        \"machine\": {},", r.plan.machine);
    let _ = writeln!(s, "        \"region_bytes\": {},", r.plan.region_bytes);
    let _ = writeln!(s, "        \"restart\": \"{}\",", r.plan.restart);
    let _ = writeln!(
        s,
        "        \"tile_shift\": {},",
        r.plan
            .tile_shift
            .map(|t| t.to_string())
            .unwrap_or_else(|| "null".to_string())
    );
    let _ = writeln!(s, "        \"hot_regions\": {},", r.plan.hot_regions);
    let _ = writeln!(
        s,
        "        \"committed_bytes\": {},",
        r.plan.committed_bytes
    );
    let _ = writeln!(
        s,
        "        \"expected_p99_ns\": {:.1},",
        finite(r.plan.expected_p99_ns)
    );
    let _ = writeln!(
        s,
        "        \"worst_case_steps\": {},",
        r.plan.worst_case_steps
    );
    let _ = writeln!(s, "        \"over_budget\": {},", r.plan.over_budget);
    let _ = writeln!(
        s,
        "        \"summary\": \"{}\"",
        json_escape(&r.plan.summary)
    );
    s.push_str("      },\n");

    s.push_str("      \"creation\": {\n");
    let _ = writeln!(s, "        \"plan_build_ns\": {},", r.plan_build_ns);
    let _ = writeln!(s, "        \"create_persist_ns\": {}", r.create_persist_ns);
    s.push_str("      },\n");

    s.push_str("      \"reopen\": {\n");
    let _ = writeln!(s, "        \"orfs_open_ns\": {},", r.orfs_open_ns);
    let _ = writeln!(
        s,
        "        \"persistent_handle_open_ns\": {},",
        r.openobject_open_ns
    );
    let _ = writeln!(
        s,
        "        \"ready_object_prepare_ns\": {}",
        r.ready_prepare_ns
    );
    s.push_str("      },\n");

    s.push_str("      \"on_disk\": {\n");
    let _ = writeln!(s, "        \"budget_bytes\": {},", r.budget_bytes);
    let _ = writeln!(s, "        \"orfs_object_bytes\": {},", r.orfs_object_bytes);
    let _ = writeln!(
        s,
        "        \"inspect_physical_total_bytes\": {},",
        r.inspect_total_bytes
    );
    let _ = writeln!(
        s,
        "        \"manifests_allocated_bytes\": {},",
        r.manifests_allocated_bytes
    );
    let _ = writeln!(s, "        \"raw_bytes\": {},", r.raw_bytes);
    let _ = writeln!(s, "        \"pinned_lz4_bytes\": {},", r.lz4_bytes);
    let _ = writeln!(s, "        \"pinned_zstd_bytes\": {},", r.zstd_bytes);
    let _ = writeln!(
        s,
        "        \"lz4_pinned_decoded_bytes\": {},",
        r.lz4_pinned_bytes
    );
    let _ = writeln!(
        s,
        "        \"zstd_pinned_decoded_bytes\": {}",
        r.zstd_pinned_bytes
    );
    s.push_str("      },\n");

    s.push_str("      \"within_budget\": {\n");
    let _ = writeln!(s, "        \"orfs\": {},", r.orfs_within_budget());
    let _ = writeln!(s, "        \"pinned_lz4\": {},", r.lz4_within_budget());
    let _ = writeln!(s, "        \"pinned_zstd\": {}", r.zstd_within_budget());
    s.push_str("      },\n");

    s.push_str("      \"sustained\": {\n");
    s.push_str("        \"orfs\": {\n");
    s.push_str(&r.orfs.json("          "));
    s.push_str("        },\n");
    s.push_str("        \"ready_object\": {\n");
    s.push_str(&r.ready.json("          "));
    s.push_str("        },\n");
    s.push_str("        \"raw\": {\n");
    s.push_str(&r.raw.json("          "));
    s.push_str("        },\n");
    s.push_str("        \"pinned_lz4\": {\n");
    s.push_str(&r.lz4.json("          "));
    s.push_str("        },\n");
    s.push_str("        \"pinned_zstd\": {\n");
    s.push_str(&r.zstd.json("          "));
    s.push_str("        }\n");
    s.push_str("      },\n");

    if let Some(f) = &r.fuse {
        s.push_str("      \"fuse\": {\n");
        let _ = writeln!(s, "        \"available\": {},", f.available);
        let _ = writeln!(s, "        \"error\": \"{}\",", json_escape(&f.error));
        let _ = writeln!(s, "        \"hot4k_p50_ns\": {},", f.hot_p50);
        let _ = writeln!(s, "        \"hot4k_p99_ns\": {},", f.hot_p99);
        let _ = writeln!(s, "        \"random4k_p50_ns\": {},", f.rand_p50);
        let _ = writeln!(s, "        \"random4k_p99_ns\": {},", f.rand_p99);
        let _ = writeln!(s, "        \"full_mbps\": {:.1},", finite(f.full_mbps));
        let _ = writeln!(
            s,
            "        \"direct_ready_hot4k_p50_ns\": {},",
            f.direct_ready_hot_p50
        );
        let _ = writeln!(s, "        \"raw_file_hot4k_p50_ns\": {}", f.raw_hot_p50);
        s.push_str("      },\n");
    } else {
        s.push_str("      \"fuse\": null,\n");
    }

    let _ = writeln!(s, "      \"verdict\": \"{}\",", json_escape(&verdict));
    let _ = writeln!(s, "      \"beats_raw_store\": {beats_raw},");
    let _ = writeln!(s, "      \"holds_vs_pinned\": {holds}");
    s.push_str("    }");
    s
}

/// Render the whole evidence document.
fn render_json(reports: &[CorpusReport], want_fuse: bool) -> String {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"producer\": \"examples/e2e.rs (sh scripts/run-e2e.sh)\","
    );
    let _ = writeln!(s, "  \"kernel\": \"{}\",", json_escape(&kernel));
    let _ = writeln!(
        s,
        "  \"fuse_feature\": {},",
        cfg!(all(feature = "fuse", target_os = "linux"))
    );
    let _ = writeln!(s, "  \"fuse_requested\": {},", want_fuse);
    s.push_str(
        "  \"budget_policy\": \"equal total on-disk budget = the raw file store's real allocated bytes (du)\",\n",
    );
    let _ = writeln!(s, "  \"read_bytes\": {READ_BYTES},");
    let _ = writeln!(s, "  \"hot_bytes\": {HOT_BYTES},");
    let _ = writeln!(s, "  \"hot_samples\": {HOT_SAMPLES},");
    let _ = writeln!(s, "  \"random_samples\": {RANDOM_SAMPLES},");
    let _ = writeln!(s, "  \"seq_chunk_bytes\": {SEQ_CHUNK},");
    s.push_str("  \"corpora\": [\n");
    for (i, r) in reports.iter().enumerate() {
        s.push_str(&render_corpus(r));
        s.push_str(if i + 1 == reports.len() { "\n" } else { ",\n" });
    }
    s.push_str("  ]\n}\n");
    s
}

/// The end-to-end measurement entry point.
fn run(out: &Path) -> ekbloom::Result<()> {
    let caps = match std::env::var("EKB_E2E_MAX_BYTES") {
        Ok(value) => match value.trim().parse::<usize>() {
            Ok(m) if m >= 64 * 1024 => common::Caps {
                text: m.min(4 * 1024 * 1024),
                elf: m,
                mixed: m,
            },
            _ => common::Caps::FULL,
        },
        Err(_) => common::Caps::FULL,
    };
    let corpora = common::load(caps);
    let scratch = PathBuf::from(
        std::env::var("EKB_E2E_SCRATCH").unwrap_or_else(|_| "target/e2e-tmp".to_string()),
    );
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    let want_fuse = std::fs::metadata("/dev/fuse").is_ok();
    if !want_fuse {
        eprintln!("e2e: /dev/fuse unavailable; the FUSE section is skipped");
    }
    let mut reports = Vec::with_capacity(corpora.len());
    for corpus in &corpora {
        eprintln!(
            "e2e: measuring {} ({} bytes, {})",
            corpus.name,
            corpus.source.len(),
            corpus.kind
        );
        reports.push(measure_corpus(corpus, &scratch, want_fuse)?);
    }
    let json = render_json(&reports, want_fuse);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, json)?;
    println!("wrote {}", out.display());
    for report in &reports {
        let (holds, beats_raw, verdict) = corpus_verdict(report);
        println!(
            "[{}] beats_raw={beats_raw} holds_vs_pinned={holds} {verdict}",
            report.name
        );
    }
    Ok(())
}

fn main() {
    let out =
        std::env::var("EKB_E2E_EVIDENCE").unwrap_or_else(|_| "evidence/bench/e2e.json".to_string());
    if let Err(e) = run(Path::new(&out)) {
        eprintln!("e2e: {e}");
        std::process::exit(1);
    }
}
