//! Shared support for the end-to-end EKBLOOM/OrFS proof (`examples/e2e.rs`,
//! `tests/e2e.rs`).
//!
//! Three concerns live here, so the example and the integration test cannot
//! diverge:
//!
//! 1. **Real corpora.** [`load`] reads *real* files that exist inside the pinned
//!    container — an ELF executable, text/licence corpora, and a mixed/binary
//!    aggregate — concatenating each category up to a byte cap. Candidate paths
//!    are collected and **sorted**, so the concatenation is deterministic; every
//!    corpus records a BLAKE3 digest and size.
//! 2. **Real on-disk accounting.** [`du_bytes`] is the same `st_blocks * 512`
//!    walk `OrFs::inspect_physical` uses (each inode once, directories included),
//!    so the measured footprint is the real allocation, not an estimate.
//! 3. **Conventional pinned-hot stores.** [`Store`] builds a raw file store, a
//!    block-indexed **LZ4** file store and a chunked **Zstandard** file store,
//!    each on real files with a real `u32`-offset index, and each retaining its
//!    decoded hot prefix **permanently** ([`PinnedHotCache`]) — the strongest
//!    conventional non-evicting competitor, at the same retained-hot policy.
//!
//! Nothing here is EKBLOOM-specific except the pinned-cache type it borrows; the
//! comparison stores are ordinary compressed files.

// Some helpers are used by the example but not the integration test (or vice
// versa); this module is shared, so unused-in-one-target items are not dead code.
#![allow(dead_code)]

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use ekbloom::{pinned_block_count, PinnedHotCache};

/// LZ4 block size (bytes) for the block-indexed comparison store.
pub const LZ4_BLOCK: usize = 4096;
/// Zstandard chunk size (bytes) for the chunked comparison store.
pub const ZSTD_CHUNK: usize = 16 * 1024;
/// Maximum recursion depth when walking a real corpus directory tree.
const MAX_WALK_DEPTH: usize = 12;

/// Per-category byte caps for [`load`].
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Maximum text-corpus bytes.
    pub text: usize,
    /// Maximum ELF-corpus bytes.
    pub elf: usize,
    /// Maximum mixed/binary-corpus bytes.
    pub mixed: usize,
}

impl Caps {
    /// The full end-to-end caps (bounded well inside the 8 GiB container cap).
    pub const FULL: Caps = Caps {
        text: 4 * 1024 * 1024,
        elf: 2 * 1024 * 1024,
        mixed: 16 * 1024 * 1024,
    };

    /// Small caps for the bounded integration test.
    pub const SMALL: Caps = Caps {
        text: 256 * 1024,
        elf: 128 * 1024,
        mixed: 256 * 1024,
    };
}

/// One loaded corpus: its bytes, its category, and where the bytes came from.
#[derive(Clone, Debug)]
pub struct Corpus {
    /// Stable short name (`text`, `elf`, `mixed`).
    pub name: &'static str,
    /// Category label (`text-like`, `elf-executable`, `mixed-binary`, ...).
    pub kind: &'static str,
    /// A short human-readable provenance string.
    pub origin: String,
    /// The concatenated corpus bytes.
    pub source: Vec<u8>,
}

/// Load the three real corpora under `caps`.
pub fn load(caps: Caps) -> Vec<Corpus> {
    vec![
        corpus_text(caps.text),
        corpus_elf(caps.elf),
        corpus_mixed(caps.mixed),
    ]
}

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

/// Real on-disk bytes of `path`: `st_blocks * 512` per inode (allocated units),
/// each inode once, directories included — the same `du`-style accounting
/// `OrFs::inspect_physical` reports.
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

/// The `q`-quantile (nearest rank) of an **ascending** sample.
pub fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// `(p50, p99)` of a sample, sorting a copy.
pub fn summarize(mut samples: Vec<u64>) -> (u64, u64) {
    samples.sort_unstable();
    (percentile(&samples, 0.50), percentile(&samples, 0.99))
}

/// Deterministic 4 KiB-aligned offsets inside `[0, hot)`.
pub fn hot_offsets(hot: u64, samples: usize) -> Vec<u64> {
    if hot < 4096 {
        return vec![0; samples];
    }
    let span = (hot - 4096) | 1;
    (0..samples)
        .map(|i| ((i as u64 * 2654435761) % span) & !4095)
        .collect()
}

/// Deterministic 4 KiB-aligned offsets drawn from inside `regions` (the engine
/// regions a plan actually manifests as hot tiles), round-robin. Returns an empty
/// vector when no region is at least one page.
pub fn hot_in_regions(regions: &[(u64, u64)], samples: usize) -> Vec<u64> {
    let usable: Vec<(u64, u64)> = regions
        .iter()
        .copied()
        .filter(|(_, len)| *len >= 4096)
        .collect();
    if usable.is_empty() {
        return Vec::new();
    }
    (0..samples)
        .map(|i| {
            let (start, len) = usable[i % usable.len()];
            let span = (len - 4096) | 1;
            (start + ((i as u64).wrapping_mul(2654435761) % span)) & !4095
        })
        .collect()
}

/// Deterministic 4 KiB-aligned offsets uniformly spread over `[0, n)`.
pub fn random_offsets(n: u64, samples: usize, seed: u64) -> Vec<u64> {
    let span = n.saturating_sub(4096).max(1) | 1;
    let mut s = seed | 1;
    (0..samples)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 17) % span) & !4095
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Corpus construction (real files, deterministic ordering).
// ---------------------------------------------------------------------------

/// Recursively collect regular files under `dir` (following symlinks), bounded by
/// [`MAX_WALK_DEPTH`].
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `metadata` follows symlinks, so a symlinked regular file is included and
        // a symlinked directory is descended (a depth cap bounds any cycle).
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(&path, depth + 1, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

/// Collect, filter, de-duplicate and sort candidate files under `roots`.
fn collect(roots: &[&str], keep: impl Fn(&Path, u64) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        walk(Path::new(root), 0, &mut out);
    }
    out.retain(|p| {
        std::fs::metadata(p)
            .map(|m| keep(p, m.len()))
            .unwrap_or(false)
    });
    out.sort();
    out.dedup();
    out
}

/// Concatenate files (in order) up to `cap` bytes, returning the bytes and the
/// paths actually used.
fn join_files(paths: &[PathBuf], cap: usize) -> (Vec<u8>, Vec<PathBuf>) {
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(1 << 22));
    let mut used = Vec::new();
    for path in paths {
        if buf.len() >= cap {
            break;
        }
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let take = (cap - buf.len()).min(meta.len() as usize);
        let mut chunk = vec![0u8; take];
        if file.read_exact(&mut chunk).is_ok() {
            buf.extend_from_slice(&chunk);
            used.push(path.clone());
        }
    }
    (buf, used)
}

/// A deterministic LCG fallback used only when a category finds no real files.
fn synthetic(name: &'static str, kind: &'static str, cap: usize) -> Corpus {
    let len = cap.clamp(4096, 1 << 20);
    let mut s = 0xE2E0_0000_0000_0001u64;
    let mut source = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        source.push((s >> 33) as u8);
    }
    Corpus {
        name,
        kind,
        origin: "(no real data found; deterministic LCG fallback)".to_string(),
        source,
    }
}

/// A real text-like corpus: licences, copyright/README docs, dpkg listings.
fn corpus_text(cap: usize) -> Corpus {
    let mut candidates = collect(
        &["/usr/share/common-licenses", "/usr/share/doc"],
        |p, len| {
            len > 0
                && !matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("gz" | "xz" | "bz2" | "zst")
                )
        },
    );
    candidates.extend(collect(&["/var/lib/dpkg/info"], |p, len| {
        len > 0 && p.extension().is_none_or(|e| e == "list")
    }));
    for extra in ["/var/lib/dpkg/status", "/usr/share/zoneinfo/tzdata.zi"] {
        if let Ok(meta) = std::fs::metadata(extra) {
            if meta.is_file() {
                candidates.push(PathBuf::from(extra));
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    let (source, used) = join_files(&candidates, cap);
    if source.is_empty() {
        return synthetic("text", "text-like", cap);
    }
    Corpus {
        name: "text",
        kind: "text-like",
        origin: format!(
            "{} real file(s); first {}",
            used.len(),
            used.first()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        source,
    }
}

/// A real ELF executable corpus.
fn corpus_elf(cap: usize) -> Corpus {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("EKB_E2E_ELF") {
        candidates.push(PathBuf::from(path));
    }
    for path in [
        "target/release/ekbloom",
        "/usr/bin/bash",
        "/usr/sbin/e2fsck",
        "/usr/bin/perl",
    ] {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(exe);
    }
    for path in candidates {
        let Ok(mut source) = std::fs::read(&path) else {
            continue;
        };
        if source.len() < 4096 || source.get(..4) != Some(b"\x7fELF".as_slice()) {
            continue;
        }
        source.truncate(cap);
        return Corpus {
            name: "elf",
            kind: "elf-executable",
            origin: path.display().to_string(),
            source,
        };
    }
    synthetic("elf", "elf-executable", cap)
}

/// A real mixed/binary aggregate: shared objects, executables and some text.
fn corpus_mixed(cap: usize) -> Corpus {
    let mut candidates = collect(
        &["/usr/lib/x86_64-linux-gnu", "/usr/bin", "/usr/sbin"],
        |_, len| len > 4096,
    );
    candidates.extend(collect(&["/usr/share/common-licenses"], |_, len| len > 0));
    candidates.sort();
    candidates.dedup();
    let (source, used) = join_files(&candidates, cap);
    if source.is_empty() {
        return synthetic("mixed", "mixed-binary", cap);
    }
    Corpus {
        name: "mixed",
        kind: "mixed-binary",
        origin: format!(
            "{} real file(s); first {}",
            used.len(),
            used.first()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        source,
    }
}

/// Find the single concealed committed `.ekb` generation for the one object under
/// `root`.
pub fn concealed_ekb(root: &Path) -> PathBuf {
    let concealed = root.join(".ekbloom");
    std::fs::read_dir(&concealed)
        .expect(".ekbloom exists")
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .expect("one concealed object directory")
        .path()
        .join("manifests")
}

// ---------------------------------------------------------------------------
// Conventional comparison stores (raw file / pinned-hot LZ4 / pinned-hot Zstd).
// ---------------------------------------------------------------------------

/// Which conventional store a [`Store`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// The source bytes stored plainly, served by `pread`.
    Raw,
    /// Block-indexed LZ4 with a permanently pinned decoded hot prefix.
    Lz4,
    /// Chunked/seekable Zstandard with a permanently pinned decoded hot prefix.
    Zstd,
}

/// A conventional comparison store over real files, with a real offset index and a
/// permanently pinned decoded hot prefix.
///
/// `Raw` keeps the source in one file and serves reads with `pread`. `Lz4`/`Zstd`
/// keep the compressed blocks/frames in one file plus a `u64`-offset/`u32`-length
/// index file; the decoded prefix covering `hot_bytes` is written to a third
/// (pinned) file and retained permanently in a [`PinnedHotCache`], so a cold read
/// can never evict it. Reads decompress **only** the covering block(s) by
/// `pread`ing the compressed bytes.
pub struct Store {
    kind: Codec,
    n: u64,
    block_bytes: usize,
    offsets: Vec<u64>,
    lens: Vec<u32>,
    file: Option<File>,
    roots: Vec<PathBuf>,
    pinned: PinnedHotCache,
    scratch: Vec<u8>,
}

impl Store {
    /// Build the raw file store for `source` under `dir`.
    pub fn raw(dir: &Path, source: &[u8]) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("raw.bin");
        std::fs::write(&path, source)?;
        Ok(Store {
            kind: Codec::Raw,
            n: source.len() as u64,
            block_bytes: 0,
            offsets: Vec::new(),
            lens: Vec::new(),
            file: Some(File::open(&path)?),
            roots: vec![path],
            pinned: PinnedHotCache::new(0, 0),
            scratch: Vec::new(),
        })
    }

    /// Build the pinned-hot LZ4 store for `source` under `dir`, pinning the decoded
    /// prefix covering `hot_bytes`.
    pub fn lz4(dir: &Path, source: &[u8], hot_bytes: u64) -> std::io::Result<Store> {
        let blocks: Vec<Vec<u8>> = source.chunks(LZ4_BLOCK).map(lz4_flex::compress).collect();
        Self::assemble(
            dir,
            source,
            hot_bytes,
            Codec::Lz4,
            LZ4_BLOCK,
            "lz4",
            &blocks,
        )
    }

    /// Build the pinned-hot Zstandard store for `source` under `dir`.
    pub fn zstd(dir: &Path, source: &[u8], hot_bytes: u64) -> std::io::Result<Store> {
        let frames: Vec<Vec<u8>> = source
            .chunks(ZSTD_CHUNK)
            .map(|c| zstd::stream::encode_all(c, 0))
            .collect::<std::io::Result<_>>()?;
        Self::assemble(
            dir,
            source,
            hot_bytes,
            Codec::Zstd,
            ZSTD_CHUNK,
            "zstd",
            &frames,
        )
    }

    fn assemble(
        dir: &Path,
        source: &[u8],
        hot_bytes: u64,
        kind: Codec,
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
        let pinned_path = dir.join(format!("{prefix}.pinned"));
        std::fs::write(&blocks_path, &concatenated)?;
        std::fs::write(&index_path, &index)?;

        // Retain the decoded hot prefix permanently: write it to a real file (its
        // real on-disk bytes are part of the footprint) and pin its decoded blocks.
        let hot = (hot_bytes as usize).min(source.len());
        std::fs::write(&pinned_path, &source[..hot])?;
        let count = pinned_block_count(hot as u64, block_bytes as u64, units.len() as u64);
        let mut pinned = PinnedHotCache::new(block_bytes as u64, units.len());
        for (b, unit) in units.iter().enumerate().take(count as usize) {
            let start = b * block_bytes;
            let full = (source.len() - start).min(block_bytes);
            let decoded = match kind {
                Codec::Lz4 => lz4_flex::decompress(unit, full).expect("lz4 block"),
                Codec::Zstd => zstd::stream::decode_all(&unit[..])?,
                Codec::Raw => unreachable!("raw has no compressed units"),
            };
            assert!(pinned.pin(b as u64, decoded), "pin must admit block {b}");
        }

        Ok(Store {
            kind,
            n: source.len() as u64,
            block_bytes,
            offsets,
            lens,
            file: Some(File::open(&blocks_path)?),
            roots: vec![blocks_path, index_path, pinned_path],
            pinned,
            scratch: Vec::new(),
        })
    }

    /// The codec this store uses.
    pub fn kind(&self) -> Codec {
        self.kind
    }

    /// The logical length served.
    pub fn logical_len(&self) -> u64 {
        self.n
    }

    /// Real on-disk bytes: the sum of the `du`-style allocation of every file.
    pub fn on_disk_bytes(&self) -> u64 {
        self.roots.iter().map(|p| du_bytes(p)).sum()
    }

    /// The number of permanently pinned decoded blocks.
    pub fn pinned_blocks(&self) -> usize {
        self.pinned.pinned_blocks()
    }

    /// The pinned decoded bytes.
    pub fn pinned_bytes(&self) -> u64 {
        self.pinned.pinned_bytes()
    }

    /// The expected decoded length of block `b`.
    fn full_len(&self, b: usize) -> usize {
        let start = b * self.block_bytes;
        (self.n as usize - start).min(self.block_bytes)
    }

    /// Byte-exact read of `[offset, offset + len)`; a pinned block is served from
    /// the permanent decoded set, a cold block is `pread`+decompressed on demand and
    /// **not** retained.
    pub fn read(&mut self, offset: u64, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        if self.kind == Codec::Raw {
            out.resize(len as usize, 0);
            self.file
                .as_ref()
                .expect("raw file")
                .read_exact_at(&mut out, offset)
                .expect("raw pread");
            return out;
        }
        let block = self.block_bytes as u64;
        let end = offset + len;
        let mut pos = offset;
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
                    Codec::Raw => unreachable!(),
                };
                let hi = hi.min(decoded.len());
                out.extend_from_slice(&decoded[lo..hi]);
            }
            pos = seg_end;
        }
        out
    }
}
