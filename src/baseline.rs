//! Conventional direct-access baselines — the ordinary readers EKBLOOM must be
//! evaluated against, under identical workloads, on the whole relationship
//! (manifested bytes, construction cost, read latency/throughput, retained
//! capabilities), not only against authoritative machine replay.
//!
//! The reviewer's request: EKBLOOM is only meaningful if it beats (or honestly
//! fails to beat) *conventional* structures. The kinds implemented here are:
//!
//! * [`BaselineKind::RawVec`] — a `Vec<u8>` of the source; a read is a direct
//!   positional slice (a `memcpy`).
//! * [`BaselineKind::RawFile`] — the source on disk, read via `pread`
//!   (`File::read_exact_at`): the file-backed positional baseline.
//! * [`BaselineKind::Predecoded`] — a materialised output cache. For this codec
//!   `V = N` (visible representation length equals logical length), so a fully
//!   predecoded cache **is byte-for-byte the raw source**. It is retained to
//!   make that equivalence explicit and honest; it is not a different structure.
//! * [`BaselineKind::IndexedArray`] — raw bytes plus a plain block-offset index
//!   (a representative conventional structure).
//! * [`BaselineKind::LruCache`] — an ordinary **byte-budgeted LRU cache** over the
//!   raw bytes, backed by a `pread`-served raw file. It holds at most
//!   `capacity_bytes` of block data in RAM; a miss loads the covering block from
//!   the backing file and may evict the least-recently-used block. This is the
//!   conventional caching structure EKBLOOM's manifested regions must be compared
//!   against under the **same** byte budget.
//!
//! These intentionally mirror EKBLOOM's certified path-bloom `hot` array, which
//! **is** a raw `N`-byte array. The comparison is therefore fair: no read-speed
//! win over `RawVec` is expected, and EKBLOOM's cost is its extra structures.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::error::{EkbloomError, Result};
use crate::fastpath;
use crate::manifest::EkbloomBody;
use crate::metrics::{now_ns, percentiles};
use crate::{CpuFeatures, ReadRequest, SweepConfig};

/// Default block size (bytes) of the [`BaselineKind::IndexedArray`] offset index.
pub const DEFAULT_INDEX_BLOCK: usize = 4096;

/// Default block size (bytes) of the [`BaselineKind::LruCache`] resident cache.
pub const DEFAULT_LRU_BLOCK: usize = 4096;

/// One conventional direct-access baseline construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaselineKind {
    /// In-memory `Vec<u8>` of the source; positional slice reads.
    RawVec,
    /// The source on disk; positional `pread` reads.
    RawFile,
    /// A materialised output cache (for `V = N` this equals the raw bytes).
    Predecoded,
    /// Raw bytes plus a plain block-offset index.
    IndexedArray,
    /// An ordinary byte-budgeted LRU cache over the raw bytes (file-backed).
    LruCache,
}

impl BaselineKind {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            BaselineKind::RawVec => "raw-vec",
            BaselineKind::RawFile => "raw-file",
            BaselineKind::Predecoded => "predecoded",
            BaselineKind::IndexedArray => "indexed-array",
            BaselineKind::LruCache => "lru-cache",
        }
    }

    /// Human-readable description including the residency of the retained bytes.
    pub fn description(self) -> &'static str {
        match self {
            BaselineKind::RawVec => "in-memory Vec<u8> source, positional slice",
            BaselineKind::RawFile => "source on disk, pread (File::read_exact_at)",
            BaselineKind::Predecoded => "materialised output cache (== raw bytes, V=N)",
            BaselineKind::IndexedArray => "raw bytes + plain block-offset index",
            BaselineKind::LruCache => "byte-budgeted LRU cache over the raw bytes (file-backed)",
        }
    }

    /// Every baseline kind, in report order.
    pub fn all() -> [BaselineKind; 5] {
        [
            BaselineKind::RawVec,
            BaselineKind::RawFile,
            BaselineKind::Predecoded,
            BaselineKind::IndexedArray,
            BaselineKind::LruCache,
        ]
    }
}

/// Bounds/overflow error identical to [`crate::reader::read`].
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
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

/// An internal invariant failure for the LRU cache (a block vanished after a
/// successful load); surfaced as a section error rather than a panic.
fn corrupt_lru(block: u64) -> EkbloomError {
    EkbloomError::BadSection {
        reason: format!("lru-cache block {block} missing after load"),
    }
}

/// Mutable LRU-cache state (interior mutability so reads keep the `&self` API).
#[derive(Debug, Default)]
struct LruState {
    /// Resident block data, keyed by block ordinal.
    resident: BTreeMap<u64, Vec<u8>>,
    /// Recency order of resident blocks, least-recently-used first.
    recency: Vec<u64>,
    /// Total resident block bytes.
    resident_bytes: u64,
    /// Cache hits (resident block touched).
    hits: u64,
    /// Cache misses (block loaded from the backing file).
    misses: u64,
}

/// A conventional direct-access baseline over a single source.
///
/// `bytes` (in-memory kinds) and/or `file` (the file-backed kinds) hold the only
/// bytes retained to serve reads; `physical_bytes` reports exactly those. For
/// [`BaselineKind::LruCache`] `physical_bytes` reports the declared byte budget
/// and `resident_bytes` reports the block bytes currently held in RAM.
#[derive(Debug)]
pub struct Baseline {
    kind: BaselineKind,
    logical_len: u64,
    physical: u64,
    construction_ns: u64,
    bytes: Vec<u8>,
    index: Vec<u64>,
    file: Option<File>,
    path: Option<PathBuf>,
    capacity: u64,
    block: u64,
    cache: RefCell<LruState>,
}

impl Baseline {
    /// Build a baseline of `kind` over `source`.
    ///
    /// `dir` is used by [`BaselineKind::RawFile`] (which materialises the source
    /// at `dir/baseline-rawfile.bin`) and by [`BaselineKind::LruCache`] (which
    /// materialises a backing file at `dir/baseline-lru-backing.bin`). The
    /// construction wall-clock time is recorded.
    ///
    /// The [`BaselineKind::LruCache`] byte budget defaults to the whole source;
    /// use [`Baseline::build_lru`] or [`Baseline::build_with_capacity`] to set an
    /// explicit budget.
    pub fn build(kind: BaselineKind, source: &[u8], dir: &Path) -> Result<Baseline> {
        let default_capacity = source.len() as u64;
        Baseline::build_with_capacity(kind, source, dir, default_capacity)
    }

    /// Build a baseline with an explicit byte budget (only
    /// [`BaselineKind::LruCache`] uses `capacity_bytes`; every other kind ignores
    /// it).
    pub fn build_with_capacity(
        kind: BaselineKind,
        source: &[u8],
        dir: &Path,
        capacity_bytes: u64,
    ) -> Result<Baseline> {
        let start = now_ns();
        let logical_len = source.len() as u64;
        let mut bytes: Vec<u8> = Vec::new();
        let mut index: Vec<u64> = Vec::new();
        let mut file: Option<File> = None;
        let mut path: Option<PathBuf> = None;
        let mut capacity = capacity_bytes;
        let block = DEFAULT_LRU_BLOCK as u64;
        let physical: u64 = match kind {
            BaselineKind::RawVec => {
                bytes = source.to_vec();
                bytes.len() as u64
            }
            BaselineKind::Predecoded => {
                // V = N for this codec: a fully predecoded cache is the source.
                bytes = source.to_vec();
                bytes.len() as u64
            }
            BaselineKind::RawFile => {
                std::fs::create_dir_all(dir)?;
                let p = dir.join("baseline-rawfile.bin");
                std::fs::write(&p, source)?;
                let f = File::open(&p)?;
                let len = f.metadata()?.len();
                file = Some(f);
                path = Some(p);
                len
            }
            BaselineKind::IndexedArray => {
                bytes = source.to_vec();
                let blocks = source.len().div_ceil(DEFAULT_INDEX_BLOCK);
                index = Vec::with_capacity(blocks);
                for b in 0..blocks {
                    index.push(b as u64 * DEFAULT_INDEX_BLOCK as u64);
                }
                (bytes.len() as u64).saturating_add((index.len() as u64).saturating_mul(8))
            }
            BaselineKind::LruCache => {
                std::fs::create_dir_all(dir)?;
                let p = dir.join("baseline-lru-backing.bin");
                std::fs::write(&p, source)?;
                let f = File::open(&p)?;
                file = Some(f);
                path = Some(p);
                // The retained RAM is the byte budget; resident bytes never
                // exceed it (one block is always retained).
                capacity = capacity_bytes;
                capacity
            }
        };
        let construction_ns = now_ns().saturating_sub(start);
        Ok(Baseline {
            kind,
            logical_len,
            physical,
            construction_ns,
            bytes,
            index,
            file,
            path,
            capacity,
            block,
            cache: RefCell::new(LruState::default()),
        })
    }

    /// Build an ordinary byte-budgeted LRU cache over `source` with an explicit
    /// resident-byte budget.
    pub fn build_lru(source: &[u8], capacity_bytes: u64, dir: &Path) -> Result<Baseline> {
        Baseline::build_with_capacity(BaselineKind::LruCache, source, dir, capacity_bytes)
    }

    /// The baseline construction kind.
    #[inline]
    pub fn kind(&self) -> BaselineKind {
        self.kind
    }

    /// The number of logical bytes the baseline serves.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The real bytes retained to serve reads.
    ///
    /// In-memory kinds report the source length (plus the `8`-byte-per-block
    /// index for [`BaselineKind::IndexedArray`]); [`BaselineKind::RawFile`]
    /// reports the on-disk file length (its RAM residency is zero). This is the
    /// same "physical bytes retained" quantity EKBLOOM reports.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.physical
    }

    /// Wall-clock nanoseconds spent building the baseline.
    #[inline]
    pub fn construction_ns(&self) -> u64 {
        self.construction_ns
    }

    /// The backing file path for [`BaselineKind::RawFile`].
    #[inline]
    pub fn backing_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The retained block-offset index for [`BaselineKind::IndexedArray`]
    /// (empty for every other kind).
    ///
    /// This index is the representative conventional structure: it is real
    /// retained bytes, counted in [`Baseline::physical_bytes`].
    #[inline]
    pub fn index(&self) -> &[u64] {
        &self.index
    }

    /// The declared byte budget of an [`BaselineKind::LruCache`] (its retained RAM
    /// ceiling); `0` for other kinds.
    #[inline]
    pub fn capacity_bytes(&self) -> u64 {
        if self.kind == BaselineKind::LruCache {
            self.capacity
        } else {
            0
        }
    }

    /// The block bytes currently resident in an [`BaselineKind::LruCache`]'s
    /// cache; `0` for other kinds.
    pub fn resident_bytes(&self) -> u64 {
        if self.kind == BaselineKind::LruCache {
            self.cache.borrow().resident_bytes
        } else {
            0
        }
    }

    /// Cache hits recorded so far (LRU only; `0` otherwise).
    pub fn hits(&self) -> u64 {
        self.cache.borrow().hits
    }

    /// Cache misses recorded so far (LRU only; `0` otherwise).
    pub fn misses(&self) -> u64 {
        self.cache.borrow().misses
    }

    /// Drop every resident block (models a close-and-reopen of the cache; the
    /// backing file is untouched). Cumulative hit/miss counters are reset too.
    pub fn clear_cache(&self) {
        let mut st = self.cache.borrow_mut();
        st.resident.clear();
        st.recency.clear();
        st.resident_bytes = 0;
        st.hits = 0;
        st.misses = 0;
    }

    /// The capabilities this baseline retains (none of EKBLOOM's).
    pub fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    /// Read `req` into `out` (appends; `out` is not cleared).
    ///
    /// Bounds and overflow semantics match [`crate::reader::read`]: an overflowing
    /// range yields [`EkbloomError::Overflow`], a range past `logical_len` yields
    /// [`EkbloomError::OutOfBounds`], and a zero-length read always succeeds.
    pub fn read_into(&self, req: ReadRequest, out: &mut Vec<u8>) -> Result<()> {
        let end = req
            .offset
            .checked_add(req.length)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > self.logical_len {
            return Err(oob(req.offset, req.length, self.logical_len));
        }
        if req.length == 0 {
            return Ok(());
        }
        if self.kind == BaselineKind::LruCache {
            return self.lru_read_into(req.offset, end, out);
        }
        let len = usize::try_from(req.length).map_err(|_| EkbloomError::Overflow {
            what: "read length",
        })?;
        match &self.file {
            Some(f) => {
                let start = out.len();
                let stop = start.checked_add(len).ok_or(EkbloomError::Overflow {
                    what: "read buffer length",
                })?;
                out.resize(stop, 0);
                f.read_exact_at(&mut out[start..stop], req.offset)?;
            }
            None => {
                let start = usize::try_from(req.offset)
                    .map_err(|_| oob(req.offset, req.length, self.logical_len))?;
                let stop = usize::try_from(end)
                    .map_err(|_| oob(req.offset, req.length, self.logical_len))?;
                out.extend_from_slice(&self.bytes[start..stop]);
            }
        }
        Ok(())
    }

    /// LRU-served read: touch every block covering `[offset, end)` and copy the
    /// requested slice of each out of the resident cache.
    fn lru_read_into(&self, offset: u64, end: u64, out: &mut Vec<u8>) -> Result<()> {
        let block = self.block.max(1);
        let first = offset / block;
        let last = (end - 1) / block;
        for b in first..=last {
            self.lru_ensure(b)?;
            let bstart = b.saturating_mul(block);
            let st = self.cache.borrow();
            let data = st.resident.get(&b).ok_or_else(|| corrupt_lru(b))?;
            let lo = usize::try_from(offset.max(bstart) - bstart)
                .map_err(|_| EkbloomError::Overflow { what: "lru slice" })?;
            let hi = usize::try_from(end.min(bstart.saturating_add(data.len() as u64)) - bstart)
                .map_err(|_| EkbloomError::Overflow { what: "lru slice" })?;
            out.extend_from_slice(&data[lo..hi]);
        }
        Ok(())
    }

    /// Ensure block `b` is resident, loading it from the backing file on a miss
    /// and evicting least-recently-used blocks while over budget.
    fn lru_ensure(&self, b: u64) -> Result<()> {
        {
            let mut st = self.cache.borrow_mut();
            if st.resident.contains_key(&b) {
                st.hits = st.hits.saturating_add(1);
                st.recency.retain(|&x| x != b);
                st.recency.push(b);
                return Ok(());
            }
            st.misses = st.misses.saturating_add(1);
        }
        let block = self.block.max(1);
        let bstart = b.saturating_mul(block);
        if bstart >= self.logical_len {
            return Err(oob(bstart, block, self.logical_len));
        }
        let bstop = bstart.saturating_add(block).min(self.logical_len);
        let nbytes = usize::try_from(bstop - bstart)
            .map_err(|_| EkbloomError::Overflow { what: "lru block" })?;
        let f = self
            .file
            .as_ref()
            .ok_or_else(|| EkbloomError::InvalidArgument {
                what: "lru-cache has no backing file".to_string(),
            })?;
        let mut buf = vec![0u8; nbytes];
        f.read_exact_at(&mut buf, bstart)?;
        let mut st = self.cache.borrow_mut();
        st.resident_bytes = st.resident_bytes.saturating_add(buf.len() as u64);
        st.resident.insert(b, buf);
        st.recency.retain(|&x| x != b);
        st.recency.push(b);
        while st.resident_bytes > self.capacity && st.resident.len() > 1 {
            let victim = st.recency.remove(0);
            if let Some(bytes) = st.resident.remove(&victim) {
                st.resident_bytes = st.resident_bytes.saturating_sub(bytes.len() as u64);
            }
        }
        Ok(())
    }

    /// Byte-exact read of `req` as a fresh `Vec`.
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_into(req, &mut out)?;
        Ok(out)
    }

    /// Read one byte per offset, in order (appends; `out` is not cleared).
    ///
    /// Bounds and overflow behaviour match [`crate::reader::read`] for a
    /// length-1 request.
    pub fn read_random_1b(&self, offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
        out.reserve(offsets.len());
        for &offset in offsets {
            let end = offset
                .checked_add(1)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            if end > self.logical_len {
                return Err(oob(offset, 1, self.logical_len));
            }
            if self.kind == BaselineKind::LruCache {
                let block = self.block.max(1);
                let b = offset / block;
                self.lru_ensure(b)?;
                let st = self.cache.borrow();
                let data = st.resident.get(&b).ok_or_else(|| corrupt_lru(b))?;
                let idx = usize::try_from(offset - b.saturating_mul(block))
                    .map_err(|_| EkbloomError::Overflow { what: "lru byte" })?;
                out.push(*data.get(idx).ok_or_else(|| corrupt_lru(b))?);
                continue;
            }
            match &self.file {
                Some(f) => {
                    let mut b = [0u8; 1];
                    f.read_exact_at(&mut b, offset)?;
                    out.push(b[0]);
                }
                None => {
                    let idx =
                        usize::try_from(offset).map_err(|_| oob(offset, 1, self.logical_len))?;
                    match self.bytes.get(idx) {
                        Some(&byte) => out.push(byte),
                        None => return Err(oob(offset, 1, self.logical_len)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Byte-exact read of the whole source.
    pub fn read_sequential(&self) -> Result<Vec<u8>> {
        self.read(ReadRequest {
            offset: 0,
            length: self.logical_len,
        })
    }
}

/// The capabilities a structure retains, for the whole-relationship comparison.
///
/// Every baseline retains none of these; EKBLOOM profiles retain a subset
/// determined by the structures they actually manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// A hash-certifiable encoded representation of the whole object.
    pub verifiable_encoded_history: bool,
    /// Complete resumable states permitting bounded checkpoint restart.
    pub exact_checkpoint_restart: bool,
    /// A retained per-position state projection.
    pub per_position_state: bool,
    /// Selectable shells (path/checkpoint/history/machine) on one body.
    pub adaptive_shells: bool,
    /// Physical bytes exceeding the visible representation (concealed expansion).
    pub concealment: bool,
}

impl Capabilities {
    /// The empty capability set retained by every conventional baseline.
    pub const fn none() -> Capabilities {
        Capabilities {
            verifiable_encoded_history: false,
            exact_checkpoint_restart: false,
            per_position_state: false,
            adaptive_shells: false,
            concealment: false,
        }
    }

    /// The capabilities an EKBLOOM body actually manifests.
    pub fn of_body(body: &EkbloomBody) -> Capabilities {
        Capabilities {
            // Every profile carries a BLAKE3-hashed encoded core.
            verifiable_encoded_history: true,
            exact_checkpoint_restart: !body.checkpoints.is_empty(),
            per_position_state: body.history.is_some(),
            adaptive_shells: !body.blooms.is_empty()
                || !body.checkpoints.is_empty()
                || body.history.is_some(),
            concealment: body.concealed_bytes() > 0,
        }
    }

    /// `true` when no capability is retained.
    pub fn is_empty(&self) -> bool {
        *self == Capabilities::none()
    }
}

/// One measured row: a baseline or an EKBLOOM profile under one workload.
#[derive(Clone, Debug, PartialEq)]
pub struct BaselinePoint {
    /// Row label (e.g. `"raw-vec"`, `"ekbloom path(D=4)"`).
    pub name: String,
    /// `"baseline"` or `"ekbloom"`.
    pub class: &'static str,
    /// Real bytes retained.
    pub physical_bytes: u64,
    /// Build/construction wall-clock nanoseconds.
    pub construction_ns: u64,
    /// Sequential throughput (bytes/s) over the whole object.
    pub seq_bytes_per_s: f64,
    /// Median single-byte random read latency (ns).
    pub random_1b_ns_p50: f64,
    /// 99th-percentile single-byte random read latency (ns).
    pub random_1b_ns_p99: f64,
    /// Median 4 KiB random range read latency (ns).
    pub random_4k_ns_p50: f64,
    /// Number of single-byte random samples collected.
    pub samples: u64,
    /// Retained capabilities.
    pub capabilities: Capabilities,
}

/// The full baseline-vs-EKBLOOM report.
#[derive(Clone, Debug, PartialEq)]
pub struct BaselineReport {
    /// Detected CPU features.
    pub cpu: CpuFeatures,
    /// Source length in bytes.
    pub source_len: u64,
    /// Corpus seed.
    pub source_seed: u64,
    /// Repetition/sample budget.
    pub iterations: u64,
    /// Random workload seed.
    pub sweep_seed: u64,
    /// One row per baseline and per measured EKBLOOM profile.
    pub points: Vec<BaselinePoint>,
}

/// Row metadata for one measurement (bundled so the argument list stays small).
struct RowMeta {
    name: String,
    class: &'static str,
    physical: u64,
    construction_ns: u64,
    n: u64,
    capabilities: Capabilities,
}

/// One measured point: identical per-op timings over a `(read-full, read-1b,
/// read-4k)` workload, for a closure-based reader.
fn measure_with<F>(meta: RowMeta, cfg: &SweepConfig, read: F) -> Result<BaselinePoint>
where
    F: Fn(ReadRequest, &mut Vec<u8>) -> Result<()>,
{
    let RowMeta {
        name,
        class,
        physical,
        construction_ns,
        n,
        capabilities,
    } = meta;
    let cap = usize::try_from(n).unwrap_or(0);

    // --- sequential throughput -------------------------------------------------
    let seq_reps = cfg.iterations.max(1);
    let mut seq_out: Vec<u8> = Vec::with_capacity(cap);
    let mut seq_bytes = 0u64;
    let seq_start = now_ns();
    for _ in 0..seq_reps {
        seq_out.clear();
        read(
            ReadRequest {
                offset: 0,
                length: n,
            },
            &mut seq_out,
        )?;
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
        read(ReadRequest { offset, length: 1 }, &mut one_out)?;
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
        read(
            ReadRequest {
                offset,
                length: win4k,
            },
            &mut out4k,
        )?;
        let b = now_ns();
        durs4k.push(b.saturating_sub(a));
    }
    durs4k.sort_unstable();
    let p4k = percentiles(&durs4k);

    Ok(BaselinePoint {
        name,
        class,
        physical_bytes: physical,
        construction_ns,
        seq_bytes_per_s,
        random_1b_ns_p50: p1.p50,
        random_1b_ns_p99: p1.p99,
        random_4k_ns_p50: p4k.p50,
        samples: durs.len() as u64,
        capabilities,
    })
}

/// Measure a built baseline under `cfg`.
pub fn measure_baseline(b: &Baseline, cfg: &SweepConfig) -> Result<BaselinePoint> {
    measure_with(
        RowMeta {
            name: b.kind().label().to_string(),
            class: "baseline",
            physical: b.physical_bytes(),
            construction_ns: b.construction_ns(),
            n: b.logical_len(),
            capabilities: b.capabilities(),
        },
        cfg,
        |req, out| b.read_into(req, out),
    )
}

/// Measure an EKBLOOM body under `cfg`, using the same optimised read paths the
/// sweep uses.
pub fn measure_body(
    body: &EkbloomBody,
    label: &str,
    construction_ns: u64,
    cfg: &SweepConfig,
) -> Result<BaselinePoint> {
    measure_with(
        RowMeta {
            name: format!("ekbloom {label}"),
            class: "ekbloom",
            physical: body.physical_bytes(),
            construction_ns,
            n: body.logical_len,
            capabilities: Capabilities::of_body(body),
        },
        cfg,
        |req, out| fastpath::read_ranges(body, &[(req.offset, req.length)], out),
    )
}

/// A tiny deterministic LCG (Numerical Recipes constants), matching `sweep`.
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

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
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

/// Serialise `report` as JSON to `path` (created, including parents, if absent).
pub fn write_baselines_json(path: &Path, report: &BaselineReport) -> Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"cpu\": {{ \"sse2\": {}, \"avx2\": {}, \"avx512f\": {} }},",
        report.cpu.sse2, report.cpu.avx2, report.cpu.avx512f
    );
    let _ = writeln!(s, "  \"source_len\": {},", report.source_len);
    let _ = writeln!(s, "  \"source_seed\": {},", report.source_seed);
    let _ = writeln!(s, "  \"iterations\": {},", report.iterations);
    let _ = writeln!(s, "  \"sweep_seed\": {},", report.sweep_seed);
    s.push_str("  \"points\": [\n");
    for (i, p) in report.points.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&p.name));
        let _ = writeln!(s, "      \"class\": \"{}\",", p.class);
        let _ = writeln!(s, "      \"physical_bytes\": {},", p.physical_bytes);
        let _ = writeln!(s, "      \"construction_ns\": {},", p.construction_ns);
        let _ = writeln!(
            s,
            "      \"seq_bytes_per_s\": {:.1},",
            finite(p.seq_bytes_per_s)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p50\": {:.1},",
            finite(p.random_1b_ns_p50)
        );
        let _ = writeln!(
            s,
            "      \"random_1b_ns_p99\": {:.1},",
            finite(p.random_1b_ns_p99)
        );
        let _ = writeln!(
            s,
            "      \"random_4k_ns_p50\": {:.1},",
            finite(p.random_4k_ns_p50)
        );
        let _ = writeln!(s, "      \"samples\": {},", p.samples);
        let c = p.capabilities;
        s.push_str("      \"retained_capabilities\": {\n");
        let _ = writeln!(
            s,
            "        \"verifiable_encoded_history\": {},",
            c.verifiable_encoded_history
        );
        let _ = writeln!(
            s,
            "        \"exact_checkpoint_restart\": {},",
            c.exact_checkpoint_restart
        );
        let _ = writeln!(
            s,
            "        \"per_position_state\": {},",
            c.per_position_state
        );
        let _ = writeln!(s, "        \"adaptive_shells\": {},", c.adaptive_shells);
        let _ = writeln!(s, "        \"concealment\": {}", c.concealment);
        s.push_str("      }\n");
        let _ = write!(s, "    }}");
        if i + 1 < report.points.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, s)?;
    Ok(())
}

/// Minimal JSON string escaping for labels.
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
