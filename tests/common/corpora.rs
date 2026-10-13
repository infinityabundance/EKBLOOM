//! Shared support for the **standard-corpora court** (`examples/corpora.rs`,
//! `tests/corpora_bench.rs`) and the **worst-case bound verification**
//! (`examples/worstcase_bound.rs`).
//!
//! Three concerns live here so the example and the integration test cannot
//! diverge:
//!
//! 1. **Standard corpora loading and hashing.** [`load_standard`] reads the
//!    downloaded, extracted standard corpora (Silesia, enwik8, Calgary) from a
//!    directory, sampling a deterministic bounded slice across every file while
//!    hashing the **whole** corpus streaming, so a replay can be checked against
//!    the recorded hashes without keeping the corpus in the repository.
//! 2. **Conventional comparison stores.** [`Store`] is a block-indexed LZ4 /
//!    Zstandard (any level) file store with a real `u64`-offset index and an
//!    optional **pinned-hot** decoded prefix retained permanently — the strongest
//!    conventional non-evicting competitor, at the same retained-hot policy,
//!    with per-read compressed-byte and `pread` counters.
//! 3. **Worst-case-bound verification.** [`verify_bound`] drives an
//!    [`Engine`] over a deterministic request stream and asserts, for **every**
//!    sample, `measured_steps <= worst_case_steps_for(request)` — the core
//!    EKBLOOM promise — while byte-checking the decoded output.
//!
//! Nothing here is EKBLOOM-specific except the counters it borrows; the
//! comparison stores are ordinary compressed files.

// Some helpers are used by one target but not another (example vs test); this
// module is shared, so unused-in-one-target items are not dead code.
#![allow(dead_code)]

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use ekbloom::{
    machine_by_id, pinned_block_count, AccessTrace, BlockIndex, CheckpointSet, DirectoryKind,
    EkbloomBody, Engine, ManifestationBudget, ManifestationProfile, PinnedHotCache, ReadRequest,
    TileSet, Workload, WorkloadKind,
};

/// LZ4 block size (bytes) for the block-indexed comparison store.
pub const LZ4_BLOCK: usize = 4096;
/// Zstandard chunk size (bytes) for the chunked comparison store.
pub const ZSTD_CHUNK: usize = 16 * 1024;
/// Default per-corpus sample cap.
pub const DEFAULT_CAP: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Hashing + statistics.
// ---------------------------------------------------------------------------

/// The lowercase hex form of a 32-byte digest.
pub fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The BLAKE3 hex digest of `bytes`.
pub fn blake3_hex(bytes: &[u8]) -> String {
    hex32(blake3::hash(bytes).as_bytes())
}

/// Stream a file through BLAKE3, returning `(hex, bytes)` — so a large corpus can
/// be hashed without ever being fully resident.
pub fn blake3_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex32(hasher.finalize().as_bytes()), total))
}

/// A nearest-rank latency summary over an ascending sample.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lat {
    pub n: usize,
    pub min: u64,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
    pub mean: f64,
}

fn nearest_rank(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil();
    let idx = (rank as usize).saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Summarise `samples` (consumed; sorted in place) into a [`Lat`].
pub fn summarize(mut samples: Vec<u64>) -> Lat {
    if samples.is_empty() {
        return Lat::default();
    }
    samples.sort_unstable();
    let sum: u128 = samples.iter().map(|&x| u128::from(x)).sum();
    Lat {
        n: samples.len(),
        min: samples[0],
        p50: nearest_rank(&samples, 0.50),
        p99: nearest_rank(&samples, 0.99),
        p999: nearest_rank(&samples, 0.999),
        max: samples[samples.len() - 1],
        mean: sum as f64 / samples.len() as f64,
    }
}

// ---------------------------------------------------------------------------
// Standard corpora (downloaded + extracted outside the repo).
// ---------------------------------------------------------------------------

/// One loaded corpus: the sampled bytes used for benchmarking, plus the full
/// corpus's size and BLAKE3 digest (so the sample is verifiable against the
/// standard artifact without committing it).
#[derive(Clone, Debug)]
pub struct Corpus {
    /// Stable short name (`silesia`, `enwik8`, `calgary`, `text`, `elf`).
    pub name: &'static str,
    /// Category label (`standard`, `text-like`, `elf-executable`).
    pub kind: &'static str,
    /// A short human-readable provenance string.
    pub origin: String,
    /// The sampled corpus bytes used for benchmarking (a bounded prefix).
    pub source: Vec<u8>,
    /// Total bytes in the whole corpus (all files).
    pub full_bytes: u64,
    /// BLAKE3 of the whole corpus, in sorted-file order, concatenated.
    pub full_blake3: String,
    /// BLAKE3 of the sampled prefix actually used.
    pub used_blake3: String,
}

/// Recursively collect regular files under `dir`, sorted.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_files(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

/// Read a deterministic bounded slice spanning every file: at most
/// `cap / n_files` bytes from each file, concatenated in sorted order.
fn sample_prefix(files: &[PathBuf], cap: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(cap.min(1 << 22));
    if files.is_empty() {
        return out;
    }
    let per = (cap / files.len()).max(1);
    for path in files {
        if out.len() >= cap {
            break;
        }
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let take = per.min(cap - out.len());
        let mut chunk = vec![0u8; take];
        match file.read(&mut chunk) {
            Ok(n) => {
                chunk.truncate(n);
                out.extend_from_slice(&chunk);
            }
            Err(_) => continue,
        }
    }
    out
}

/// Load the standard corpora present under `dir`.
///
/// A corpus directory is any immediate subdirectory (`silesia`, `enwik8`,
/// `calgary`); a missing one is skipped. `enwik8` is a single file inside its
/// directory. The whole corpus is hashed streaming; only a bounded slice
/// ([`sample_prefix`]) is retained.
pub fn load_standard(dir: &Path, cap: usize) -> Vec<Corpus> {
    let specs: [(&'static str, &'static str); 3] = [
        ("silesia", "standard/silesia"),
        ("enwik8", "standard/enwik8"),
        ("calgary", "standard/calgary"),
    ];
    let mut out = Vec::new();
    for (name, kind) in specs {
        let root = dir.join(name);
        if !root.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_files(&root, &mut files);
        files.sort();
        if files.is_empty() {
            continue;
        }
        // Hash the whole corpus streaming (sorted files, concatenated).
        let mut hasher = blake3::Hasher::new();
        let mut full_bytes = 0u64;
        for path in &files {
            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(n) = file.read(&mut buf) {
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                full_bytes += n as u64;
            }
        }
        let source = sample_prefix(&files, cap);
        out.push(Corpus {
            name,
            kind,
            origin: format!("{} standard file(s) under {}", files.len(), root.display()),
            used_blake3: blake3_hex(&source),
            source,
            full_bytes,
            full_blake3: hex32(hasher.finalize().as_bytes()),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Body / engine builders for the bound court.
// ---------------------------------------------------------------------------

/// A generous budget (the bound court is about the step guarantee, not the
/// footprint).
pub fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

/// Plan the bounded engine over `body` for a 4 KiB range workload.
pub fn engine_for(body: &EkbloomBody) -> ekbloom::Result<Engine> {
    let workload = Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, 4096)],
    };
    Engine::plan(body, &generous(), &workload)
}

/// A body carrying a **large block-restart index** (`block_bytes` consecutive
/// logical positions per independent restart), so the per-region bound is
/// `block_bytes - 1`.
pub fn body_with_block(
    source: &[u8],
    machine_id: u32,
    block_bytes: u32,
) -> ekbloom::Result<EkbloomBody> {
    let mut body = ekbloom::create_machine(source, ManifestationProfile::Root, machine_id)?;
    let index =
        BlockIndex::build_machine(&body.core, source.len() as u64, block_bytes, machine_id)?;
    body.blocks.push(index);
    body.generation += 1;
    Ok(body)
}

/// A body carrying a **large checkpoint interval** (sparse complete restarts),
/// so the per-region bound is `interval - 1`.
pub fn body_with_checkpoint(
    source: &[u8],
    machine_id: u32,
    interval: u32,
) -> ekbloom::Result<EkbloomBody> {
    let mut body = ekbloom::create_machine(source, ManifestationProfile::Root, machine_id)?;
    let machine = machine_by_id(machine_id)
        .ok_or(ekbloom::EkbloomError::UnknownMachine { id: machine_id })?;
    let cp = CheckpointSet::build_with(machine, &body.core, source.len() as u64, interval)?;
    body.checkpoints.push(cp);
    body.generation += 1;
    Ok(body)
}

/// A body carrying a large block-restart index **and** hot byte tiles over the
/// middle `band` of regions — so a request spanning cold → hot → cold pays one
/// bounded restart per dispatched segment (the multi-region case).
pub fn body_with_block_and_tiles(
    source: &[u8],
    machine_id: u32,
    block_bytes: u32,
    band: (u64, u64),
    tile_shift: u8,
) -> ekbloom::Result<EkbloomBody> {
    let mut body = body_with_block(source, machine_id, block_bytes)?;
    let trace = AccessTrace {
        regions: vec![band],
        counts: vec![1000],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        tile_shift,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )?;
    body.generation += 1;
    Ok(body)
}

// ---------------------------------------------------------------------------
// Conventional comparison stores.
// ---------------------------------------------------------------------------

/// Which codec a [`Store`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Block-indexed LZ4.
    Lz4,
    /// Chunked/seekable Zstandard at an explicit level.
    Zstd,
}

/// A block-indexed file store with a real offset index and an optional
/// permanently pinned decoded hot prefix.
///
/// A cold read decompresses **only** the covering block(s) — never the whole
/// object — by `pread`ing the compressed bytes; a pinned block is served from the
/// permanent decoded set. Per-read compressed-byte and `pread` counters are
/// tracked so the encoded-window cost is reportable.
pub struct Store {
    kind: Codec,
    level: i32,
    n: u64,
    block_bytes: usize,
    offsets: Vec<u64>,
    lens: Vec<u32>,
    file: Option<File>,
    roots: Vec<PathBuf>,
    pinned: PinnedHotCache,
    scratch: Vec<u8>,
    last_fetched: u64,
    last_syscalls: u64,
}

impl Store {
    /// Build a block-indexed LZ4 store.
    pub fn lz4(dir: &Path, source: &[u8], pin_hot_bytes: u64) -> std::io::Result<Store> {
        let units: Vec<Vec<u8>> = source.chunks(LZ4_BLOCK).map(lz4_flex::compress).collect();
        Self::assemble(
            dir,
            source,
            pin_hot_bytes,
            Codec::Lz4,
            0,
            LZ4_BLOCK,
            "lz4",
            &units,
        )
    }

    /// Build a chunked Zstandard store at `level`.
    pub fn zstd(
        dir: &Path,
        source: &[u8],
        level: i32,
        pin_hot_bytes: u64,
    ) -> std::io::Result<Store> {
        let units: Vec<Vec<u8>> = source
            .chunks(ZSTD_CHUNK)
            .map(|c| zstd::stream::encode_all(c, level))
            .collect::<std::io::Result<_>>()?;
        Self::assemble(
            dir,
            source,
            pin_hot_bytes,
            Codec::Zstd,
            level,
            ZSTD_CHUNK,
            "zstd",
            &units,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        dir: &Path,
        source: &[u8],
        pin_hot_bytes: u64,
        kind: Codec,
        level: i32,
        block_bytes: usize,
        prefix: &str,
        units: &[Vec<u8>],
    ) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let mut concatenated = Vec::new();
        let mut index = Vec::with_capacity(units.len() * 12);
        let mut offsets = Vec::with_capacity(units.len());
        let mut lens = Vec::with_capacity(units.len());
        for unit in units {
            offsets.push(concatenated.len() as u64);
            lens.push(unit.len() as u32);
            index.extend_from_slice(&(concatenated.len() as u64).to_le_bytes());
            index.extend_from_slice(&(unit.len() as u32).to_le_bytes());
            concatenated.extend_from_slice(unit);
        }
        let blocks_path = dir.join(format!("{prefix}.blocks"));
        let index_path = dir.join(format!("{prefix}.index"));
        std::fs::write(&blocks_path, &concatenated)?;
        std::fs::write(&index_path, &index)?;

        let mut roots = vec![blocks_path.clone(), index_path];
        let mut pinned = PinnedHotCache::new(block_bytes as u64, units.len());
        if pin_hot_bytes > 0 {
            let pinned_path = dir.join(format!("{prefix}.pinned"));
            let hot = (pin_hot_bytes as usize).min(source.len());
            std::fs::write(&pinned_path, &source[..hot])?;
            let count = pinned_block_count(hot as u64, block_bytes as u64, units.len() as u64);
            for (b, unit) in units.iter().enumerate().take(count as usize) {
                let start = b * block_bytes;
                let full = (source.len() - start).min(block_bytes);
                let decoded = match kind {
                    Codec::Lz4 => lz4_flex::decompress(unit, full).expect("lz4 block"),
                    Codec::Zstd => zstd::stream::decode_all(&unit[..])?,
                };
                assert!(pinned.pin(b as u64, decoded), "pin must admit block {b}");
            }
            roots.push(pinned_path);
        }

        Ok(Store {
            kind,
            level,
            n: source.len() as u64,
            block_bytes,
            offsets,
            lens,
            file: Some(File::open(&blocks_path)?),
            roots,
            pinned,
            scratch: Vec::new(),
            last_fetched: 0,
            last_syscalls: 0,
        })
    }

    /// The logical length served.
    pub fn logical_len(&self) -> u64 {
        self.n
    }

    /// Real on-disk bytes: the sum of the `du`-style allocation of every file.
    pub fn on_disk_bytes(&self) -> u64 {
        self.roots.iter().map(|p| du_bytes(p)).sum()
    }

    /// The retained footprint (on-disk allocation).
    pub fn physical_bytes(&self) -> u64 {
        self.on_disk_bytes()
    }

    /// The pinned decoded bytes (retained permanently).
    pub fn pinned_bytes(&self) -> u64 {
        self.pinned.pinned_bytes()
    }

    /// The codec label, including the Zstandard level.
    pub fn label(&self) -> String {
        match self.kind {
            Codec::Lz4 => "lz4".to_string(),
            Codec::Zstd => format!("zstd-{}", self.level),
        }
    }

    /// Compressed bytes fetched by the last [`Store::read`].
    pub fn last_fetched(&self) -> u64 {
        self.last_fetched
    }

    /// `pread` syscalls performed by the last [`Store::read`].
    pub fn last_syscalls(&self) -> u64 {
        self.last_syscalls
    }

    /// The expected decoded length of block `b`.
    fn full_len(&self, b: usize) -> usize {
        let start = b * self.block_bytes;
        (self.n as usize - start).min(self.block_bytes)
    }

    /// Byte-exact read of `[offset, offset + len)`; a pinned block is served from
    /// the permanent decoded set, a cold block is `pread`+decompressed on demand
    /// and **not** retained. Updates the per-read counters.
    pub fn read(&mut self, offset: u64, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        let block = self.block_bytes as u64;
        let end = offset + len;
        let mut pos = offset;
        let mut fetched = 0u64;
        let mut syscalls = 0u64;
        while pos < end {
            let b = (pos / block) as usize;
            let start = pos - pos % block;
            let seg_end = end.min(start + block);
            let lo = (pos - start) as usize;
            let hi = (seg_end - start) as usize;
            if let Some(data) = self.pinned.get(b as u64) {
                let hi = hi.min(data.len());
                out.extend_from_slice(&data[lo..hi]);
            } else {
                self.pinned.note_miss();
                let co = self.offsets[b];
                let clen = self.lens[b] as usize;
                fetched += clen as u64;
                syscalls += 1;
                self.scratch.resize(clen, 0);
                self.file
                    .as_ref()
                    .expect("compressed file")
                    .read_exact_at(&mut self.scratch, co)
                    .expect("compressed pread");
                let decoded = match self.kind {
                    Codec::Lz4 => {
                        lz4_flex::decompress(&self.scratch, self.full_len(b)).expect("lz4 block")
                    }
                    Codec::Zstd => zstd::stream::decode_all(&self.scratch[..]).expect("zstd frame"),
                };
                let hi = hi.min(decoded.len());
                out.extend_from_slice(&decoded[lo..hi]);
            }
            pos = seg_end;
        }
        self.last_fetched = fetched;
        self.last_syscalls = syscalls;
        out
    }

    /// Single-byte positional read (the codec's `byte_at`).
    pub fn byte_at(&mut self, offset: u64) -> u8 {
        self.read(offset, 1)[0]
    }
}

/// Real on-disk bytes of `path`: `st_blocks * 512` per inode, each inode once,
/// directories included (the same `du`-style accounting `OrFs` reports).
pub fn du_bytes(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = metadata.blocks().saturating_mul(512).max(metadata.len());
    if metadata.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total = total.saturating_add(du_bytes(&entry.path()));
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Worst-case bound verification.
// ---------------------------------------------------------------------------

/// The outcome of a [`verify_bound`] run.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BoundOutcome {
    /// Samples checked.
    pub samples: u64,
    /// The largest **measured** positioning step count.
    pub max_measured_steps: u64,
    /// The largest **claimed** request-level bound.
    pub max_bound_steps: u64,
    /// Samples where `measured > claimed` (must be `0`).
    pub violations: u64,
    /// Samples whose decoded bytes differed from the source (must be `0`).
    pub mismatches: u64,
    /// The worst (measured / bound) ratio observed (`0` when the bound is `0`).
    pub worst_ratio: f64,
}

/// Drive `engine` over `count` deterministic requests and assert, for **every**
/// sample, `positioning <= worst_case_steps_for(request)`.
///
/// The request length cycles through `lens`; offsets are drawn uniformly (not
/// page-aligned, a stronger test). Every decoded range is compared byte-for-byte
/// against `source`, so a bound that "holds" by returning wrong bytes is caught.
pub fn verify_bound(
    engine: &Engine,
    body: &EkbloomBody,
    source: &[u8],
    count: usize,
    lens: &[u64],
    seed: u64,
) -> ekbloom::Result<BoundOutcome> {
    let n = source.len() as u64;
    assert!(!lens.is_empty(), "verify_bound needs at least one length");
    let mut out = BoundOutcome::default();
    let mut s = seed | 1;
    for i in 0..count {
        let len = lens[i % lens.len()].min(n);
        let span = n.saturating_sub(len).max(1);
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let offset = (s >> 17) % span;
        let req = ReadRequest {
            offset,
            length: len,
        };
        let (bytes, steps) = engine.read_counted_steps(body, req)?;
        let measured = steps.positioning;
        let bound = engine.worst_case_steps_for(req);
        out.samples += 1;
        out.max_measured_steps = out.max_measured_steps.max(measured);
        out.max_bound_steps = out.max_bound_steps.max(bound);
        if measured > bound {
            out.violations += 1;
        }
        if bound > 0 {
            let ratio = measured as f64 / bound as f64;
            if ratio > out.worst_ratio {
                out.worst_ratio = ratio;
            }
        }
        if bytes != source[offset as usize..(offset + len) as usize] {
            out.mismatches += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_matches_definition() {
        let s: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&s, 0.50), 50);
        assert_eq!(nearest_rank(&s, 0.99), 99);
        assert_eq!(nearest_rank(&s, 0.999), 100);
        let l = summarize(s);
        assert_eq!(l.p50, 50);
        assert_eq!(l.max, 100);
        assert_eq!(l.n, 100);
    }

    #[test]
    fn empty_summary_is_zero() {
        let l = summarize(Vec::new());
        assert_eq!((l.n, l.p50, l.p99, l.p999), (0, 0, 0, 0));
    }

    #[test]
    fn hex_and_hash_are_stable() {
        assert_eq!(blake3_hex(b"").len(), 64);
        assert_eq!(blake3_hex(b"abc"), blake3_hex(b"abc"));
        assert_ne!(blake3_hex(b"abc"), blake3_hex(b"abd"));
    }
}
