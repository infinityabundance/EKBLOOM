//! Shared support for the **RocksDB-style skewed KV access court**
//! (`examples/rocksdb_skew.rs`, `tests/rocksdb_skew.rs`).
//!
//! Three concerns live here so the example and the integration test cannot
//! diverge:
//!
//! 1. **A deterministic 80/20-skewed KV dataset.** [`dataset`] builds
//!    `records` × `VALUE_BYTES`-byte values (RocksDB-style key + a skewed token
//!    value), and [`Skew`] draws record indices so that exactly
//!    [`HOT_READ_PCT`]% of reads land in the hot [`HOT_SET_PCT`]% of records
//!    (Zipf-ish within each region) from a fixed seed.
//! 2. **Conventional KV stores.** [`RawStore`] is a positional byte array;
//!    [`BlockStore`] is a block-indexed LZ4 / Zstandard / Zstandard-with-trained-
//!    dictionary store (4 KiB blocks, a real `u64`/`u32` offset index) whose
//!    per-read compressed bytes and `pread` syscalls are counted by a
//!    [`BlockStore::pread_block`] wrapper; [`DictStore`] is EKBLOOM machine 9
//!    (`OrANS-DICT`) — one shared trained dictionary plus header-less per-record
//!    cores.
//! 3. **Skew-aware measurement.** [`measure_seq`] times a read sequence, asserts
//!    **every** read byte-exact against the source, and splits the get latency
//!    into hot/cold tails.
//!
//! Nothing here is EKBLOOM-specific except the machine-9 store; the comparison
//! stores are ordinary compressed files.

// Some helpers are used by one target but not another (example vs test); this
// module is shared, so unused-in-one-target items are not dead code.
#![allow(dead_code)]

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use ekbloom::{prepare, Dictionary, Prepared, Snapshot, DICT};

/// Value size in bytes (a RocksDB value).
pub const VALUE_BYTES: usize = 100;
/// Default record count (4 MiB of values).
pub const DEFAULT_RECORDS: usize = 40_000;
/// I/O block for the block-indexed codecs and the `read_random` workload.
pub const BLOCK_BYTES: u64 = 4096;
/// Zstandard dictionary capacity (bytes), matching `examples/dictionary.rs`.
pub const ZSTD_DICT_BYTES: usize = 110 * 1024;
/// Zstandard level used for every zstd row.
pub const ZSTD_LEVEL: i32 = 3;
/// Fraction of reads (percent) drawn from the hot set.
pub const HOT_READ_PCT: u32 = 80;
/// Fraction of records (percent) that form the hot set.
pub const HOT_SET_PCT: usize = 20;
/// Cursor of a referenced machine-9 record core:
/// `DICT_FRAME_PREFIX_BYTES (20) + DICT_RECORD_HEADER_BYTES (8)`.
const DICT_CURSOR: u64 = 28;

// ---------------------------------------------------------------------------
// Dataset + skew.
// ---------------------------------------------------------------------------

/// A deterministic KV dataset.
pub struct Dataset {
    /// The values, each exactly `VALUE_BYTES` long.
    pub records: Vec<Vec<u8>>,
    /// The concatenation of every value (the logical object).
    pub source: Vec<u8>,
    /// Number of leading records that form the hot set.
    pub hot_records: usize,
}

/// A text-like KV value of exactly [`VALUE_BYTES`] bytes for record `index`.
///
/// A short, effectively unique numeric key prefix (`k=NNNNNNNN;`) is followed by
/// tokens drawn from a small, heavily skewed set, so the value distribution is
/// order-0 compressible while the keys stay near-incompressible — the realistic
/// mixed alphabet of a real key/value store.
pub fn make_value(index: usize) -> Vec<u8> {
    const PARTS: [&[u8]; 14] = [
        b"region=us-east-1",
        b"region=eu-west-1",
        b"region=ap-south-1",
        b"status=active",
        b"status=idle",
        b"status=draining",
        b"kind=alpha",
        b"kind=beta",
        b"kind=gamma",
        b"|score=000",
        b"|score=111",
        b"|score=222",
        b"user=anon",
        b"|tail",
    ];
    let mut out = Vec::with_capacity(VALUE_BYTES + 32);
    out.extend_from_slice(format!("k={index:08};").as_bytes());
    let mut k = (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    while out.len() < VALUE_BYTES {
        k = k
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (k >> 33) % 100;
        let idx = if r < 45 {
            0
        } else if r < 62 {
            1
        } else if r < 74 {
            2
        } else if r < 84 {
            3
        } else if r < 90 {
            4
        } else if r < 94 {
            5
        } else {
            6 + ((k >> 41) as usize % (PARTS.len() - 6))
        };
        out.extend_from_slice(PARTS[idx]);
    }
    out.truncate(VALUE_BYTES);
    out
}

/// Build a deterministic dataset of `records` values.
pub fn dataset(records: usize) -> Dataset {
    let records = records.max(1);
    let mut values = Vec::with_capacity(records);
    let mut source = Vec::with_capacity(records * VALUE_BYTES);
    for i in 0..records {
        let v = make_value(i);
        source.extend_from_slice(&v);
        values.push(v);
    }
    let hot = (records * HOT_SET_PCT / 100).max(1);
    Dataset {
        records: values,
        source,
        hot_records: hot,
    }
}

/// A deterministic 80/20 skew sampler over `[0, total)`.
///
/// Exactly [`HOT_READ_PCT`]% of draws come from the first `hot` indices
/// (Zipf-ish within the hot and cold regions independently), so the access
/// pattern is both head-heavy and reproducible from one seed.
pub struct Skew {
    state: u64,
    total: usize,
    hot: usize,
}

impl Skew {
    /// A sampler whose hot set is [`HOT_SET_PCT`]% of `total`.
    pub fn new(seed: u64, total: usize) -> Skew {
        let total = total.max(1);
        Self::with_hot(seed, total, (total * HOT_SET_PCT / 100).max(1))
    }

    /// A sampler with an explicit hot-set size.
    pub fn with_hot(seed: u64, total: usize, hot: usize) -> Skew {
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

    /// The next record index: [`HOT_READ_PCT`]% from the head-heavy (Zipf-ish)
    /// hot set, the rest uniform over the cold set.
    pub fn next_index(&mut self) -> usize {
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

    /// Whether `index` is in the hot set.
    pub fn is_hot(&self, index: usize) -> bool {
        index < self.hot
    }

    /// The hot-set size.
    pub fn hot(&self) -> usize {
        self.hot
    }
}

/// A deterministic read plan: `get` byte offsets (100-byte values) and
/// block-aligned `read_random` offsets (4 KiB), with a hot/cold label each.
pub struct ReadPlan {
    /// `get` offsets (one 100-byte value each).
    pub gets: Vec<u64>,
    /// Whether each get targets the hot set.
    pub get_hot: Vec<bool>,
    /// `read_random` offsets (4 KiB, block-aligned).
    pub reads4k: Vec<u64>,
    /// Whether each 4 KiB read targets the hot set.
    pub r4k_hot: Vec<bool>,
}

/// Build the skewed read plan for `dataset`.
pub fn read_plan(dataset: &Dataset, gets: usize, reads4k: usize, seed: u64) -> ReadPlan {
    let records = dataset.records.len();
    let mut get_skew = Skew::new(seed, records);
    let mut gets_v = Vec::with_capacity(gets);
    let mut get_hot = Vec::with_capacity(gets);
    for _ in 0..gets {
        let i = get_skew.next_index();
        gets_v.push((i as u64) * VALUE_BYTES as u64);
        get_hot.push(get_skew.is_hot(i));
    }

    let blocks = (dataset.source.len() as u64 / BLOCK_BYTES) as usize;
    let hot_bytes = dataset.hot_records as u64 * VALUE_BYTES as u64;
    let hot_blocks = (hot_bytes / BLOCK_BYTES) as usize;
    let mut block_skew = Skew::with_hot(seed ^ 0xABCD_EF01_2345_6789, blocks, hot_blocks);
    let mut reads_v = Vec::with_capacity(reads4k);
    let mut r4k_hot = Vec::with_capacity(reads4k);
    for _ in 0..reads4k {
        let i = block_skew.next_index();
        reads_v.push((i as u64) * BLOCK_BYTES);
        r4k_hot.push(block_skew.is_hot(i));
    }

    ReadPlan {
        gets: gets_v,
        get_hot,
        reads4k: reads_v,
        r4k_hot,
    }
}

// ---------------------------------------------------------------------------
// Latency summaries.
// ---------------------------------------------------------------------------

/// A nearest-rank latency summary in nanoseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lat {
    /// Sample count.
    pub n: u64,
    /// 50th percentile.
    pub p50: u64,
    /// 99th percentile.
    pub p99: u64,
    /// 99.9th percentile.
    pub p999: u64,
    /// Arithmetic mean.
    pub mean: f64,
}

fn nearest_rank(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Summarise a nanosecond sample (sorts in place).
pub fn summarize(mut samples: Vec<u64>) -> Lat {
    if samples.is_empty() {
        return Lat::default();
    }
    samples.sort_unstable();
    let n = samples.len() as u64;
    let mean = samples.iter().map(|&x| x as f64).sum::<f64>() / n as f64;
    Lat {
        n,
        p50: nearest_rank(&samples, 0.50),
        p99: nearest_rank(&samples, 0.99),
        p999: nearest_rank(&samples, 0.999),
        mean,
    }
}

/// The measured result of one read sequence.
pub struct SeqResult {
    /// Overall latency distribution.
    pub lat: Lat,
    /// Hot-set sub-distribution, when the sequence carries hot/cold labels.
    pub hot: Option<Lat>,
    /// Cold-set sub-distribution, when labelled.
    pub cold: Option<Lat>,
    /// Mean bytes fetched per hot read.
    pub hot_bytes_mean: Option<f64>,
    /// Mean bytes fetched per cold read.
    pub cold_bytes_mean: Option<f64>,
    /// Mean bytes fetched per read (compressed / encoded bytes for codecs).
    pub bytes_mean: f64,
    /// Maximum bytes fetched by a single read.
    pub bytes_max: u64,
    /// Mean `pread` syscalls per read.
    pub syscalls_mean: f64,
    /// Logical bytes served (`offs.len() * len`).
    pub logical_bytes: u64,
    /// Sum of per-read elapsed wall time.
    pub elapsed_ns: u64,
}

impl SeqResult {
    /// Logical decode/serve throughput (MiB/s over the summed per-read time).
    pub fn mib_per_s(&self) -> f64 {
        if self.elapsed_ns == 0 {
            0.0
        } else {
            self.logical_bytes as f64 * 1e9 / (self.elapsed_ns as f64) / (1024.0 * 1024.0)
        }
    }
}

/// Time `read` over `offs`, assert every result byte-exact, and split the
/// latency by an optional hot/cold label.
///
/// `read(offset, len, out)` must fill `out` with exactly `len` logical bytes and
/// return `(bytes_fetched, syscalls)` from storage.
pub fn measure_seq<F: FnMut(u64, u64, &mut Vec<u8>) -> (u64, u64)>(
    source: &[u8],
    offs: &[u64],
    hot: Option<&[bool]>,
    len: u64,
    mut read: F,
) -> SeqResult {
    let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
    let mut samples = Vec::with_capacity(offs.len());
    let mut hot_samples: Vec<u64> = Vec::new();
    let mut cold_samples: Vec<u64> = Vec::new();
    let mut hot_bytes_sum: u128 = 0;
    let mut hot_n: u64 = 0;
    let mut cold_bytes_sum: u128 = 0;
    let mut cold_n: u64 = 0;
    let mut bytes_sum: u128 = 0;
    let mut sys_sum: u128 = 0;
    let mut bytes_max = 0u64;
    let mut elapsed_ns = 0u64;
    for (i, &o) in offs.iter().enumerate() {
        buf.clear();
        let t0 = Instant::now();
        let (bytes, syscalls) = read(o, len, &mut buf);
        let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        elapsed_ns = elapsed_ns.saturating_add(dt);
        assert_eq!(buf.len(), len as usize, "read length at {o}");
        assert_eq!(
            &buf[..],
            &source[o as usize..(o + len) as usize],
            "byte-exact mismatch at {o}"
        );
        samples.push(dt);
        bytes_sum += u128::from(bytes);
        sys_sum += u128::from(syscalls);
        bytes_max = bytes_max.max(bytes);
        if let Some(h) = hot {
            if h.get(i).copied().unwrap_or(false) {
                hot_samples.push(dt);
                hot_bytes_sum += u128::from(bytes);
                hot_n += 1;
            } else {
                cold_samples.push(dt);
                cold_bytes_sum += u128::from(bytes);
                cold_n += 1;
            }
        }
    }
    let n = offs.len() as f64;
    SeqResult {
        lat: summarize(samples),
        hot: hot.map(|_| summarize(hot_samples)),
        cold: hot.map(|_| summarize(cold_samples)),
        hot_bytes_mean: hot.map(|_| {
            if hot_n > 0 {
                hot_bytes_sum as f64 / hot_n as f64
            } else {
                0.0
            }
        }),
        cold_bytes_mean: hot.map(|_| {
            if cold_n > 0 {
                cold_bytes_sum as f64 / cold_n as f64
            } else {
                0.0
            }
        }),
        bytes_mean: if n > 0.0 { bytes_sum as f64 / n } else { 0.0 },
        bytes_max,
        syscalls_mean: if n > 0.0 { sys_sum as f64 / n } else { 0.0 },
        logical_bytes: offs.len() as u64 * len,
        elapsed_ns,
    }
}

// ---------------------------------------------------------------------------
// Filesystem accounting.
// ---------------------------------------------------------------------------

/// `du`-style on-disk bytes of `path` (`st_blocks * 512`, at least `len`).
pub fn alloc_bytes(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    metadata.blocks().saturating_mul(512).max(metadata.len())
}

// ---------------------------------------------------------------------------
// Raw store.
// ---------------------------------------------------------------------------

/// A positional raw byte array (the identity store).
pub struct RawStore {
    data: Vec<u8>,
    last: u64,
    footprint: u64,
}

impl RawStore {
    /// Wrap `source`.
    pub fn new(source: &[u8]) -> RawStore {
        RawStore {
            data: source.to_vec(),
            last: 0,
            footprint: source.len() as u64,
        }
    }

    /// Read `[offset, offset+len)` into `out` (replacing its contents).
    pub fn read_into(&mut self, offset: u64, len: u64, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.data[offset as usize..(offset + len) as usize]);
        self.last = len;
    }

    /// Compressed bytes fetched by the last read.
    pub fn last_fetched(&self) -> u64 {
        self.last
    }

    /// Retained footprint.
    pub fn footprint(&self) -> u64 {
        self.footprint
    }

    /// The independent expected fetch count for `[offset, offset+len)`.
    pub fn expected_fetched(&self, _offset: u64, len: u64) -> u64 {
        len
    }
}

// ---------------------------------------------------------------------------
// Block-indexed codec store.
// ---------------------------------------------------------------------------

/// Which codec a [`BlockStore`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Block-indexed LZ4.
    Lz4,
    /// Block-indexed Zstandard.
    Zstd,
    /// Block-indexed Zstandard with a trained dictionary.
    ZstdDict,
}

enum Decoder {
    Lz4,
    Zstd(zstd::bulk::Decompressor<'static>),
}

/// A block-indexed file store with a real offset index and counted I/O.
///
/// Every read `pread`s only the covering compressed block(s) — never the whole
/// object — and the per-read compressed bytes and `pread` syscalls are counted
/// by [`BlockStore::pread_block`], the counting I/O wrapper.
pub struct BlockStore {
    kind: Codec,
    block: usize,
    n: u64,
    offs: Vec<u64>,
    lens: Vec<u32>,
    file: File,
    scratch: Vec<u8>,
    decoder: Decoder,
    last_fetched: u64,
    last_syscalls: u64,
    footprint: u64,
    dict_shared: u64,
    _dir: PathBuf,
}

impl BlockStore {
    /// Build a block-indexed store for `codec` over `source`.
    ///
    /// `dict` must be the leaked, `'static` trained dictionary for
    /// [`Codec::ZstdDict`] and is `None` otherwise.
    pub fn build(
        dir: &Path,
        codec: Codec,
        source: &[u8],
        dict: Option<&'static [u8]>,
    ) -> io::Result<BlockStore> {
        let block = BLOCK_BYTES as usize;
        std::fs::create_dir_all(dir)?;
        let units: Vec<Vec<u8>> = match codec {
            Codec::Lz4 => source.chunks(block).map(lz4_flex::compress).collect(),
            Codec::Zstd => source
                .chunks(block)
                .map(|c| zstd::stream::encode_all(c, ZSTD_LEVEL))
                .collect::<io::Result<_>>()?,
            Codec::ZstdDict => {
                let dict = dict.expect("zstd-dict requires a dictionary");
                let mut compressor = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, dict)?;
                source
                    .chunks(block)
                    .map(|c| compressor.compress(c))
                    .collect::<io::Result<_>>()?
            }
        };

        let mut blocks = Vec::new();
        let mut index = Vec::with_capacity(units.len() * 12);
        let mut offs = Vec::with_capacity(units.len());
        let mut lens = Vec::with_capacity(units.len());
        for unit in &units {
            offs.push(blocks.len() as u64);
            lens.push(unit.len() as u32);
            index.extend_from_slice(&(blocks.len() as u64).to_le_bytes());
            index.extend_from_slice(&(unit.len() as u32).to_le_bytes());
            blocks.extend_from_slice(unit);
        }
        let prefix = match codec {
            Codec::Lz4 => "lz4",
            Codec::Zstd => "zstd",
            Codec::ZstdDict => "zstd-dict",
        };
        let blocks_path = dir.join(format!("{prefix}.blocks"));
        let index_path = dir.join(format!("{prefix}.index"));
        std::fs::write(&blocks_path, &blocks)?;
        std::fs::write(&index_path, &index)?;

        let decoder = match codec {
            Codec::Lz4 => Decoder::Lz4,
            Codec::Zstd => Decoder::Zstd(zstd::bulk::Decompressor::new()?),
            Codec::ZstdDict => Decoder::Zstd(zstd::bulk::Decompressor::with_dictionary(
                dict.expect("zstd-dict requires a dictionary"),
            )?),
        };
        let dict_shared = dict.map_or(0, |d| d.len() as u64);
        let footprint = alloc_bytes(&blocks_path) + alloc_bytes(&index_path) + dict_shared;

        Ok(BlockStore {
            kind: codec,
            block,
            n: source.len() as u64,
            offs,
            lens,
            file: File::open(&blocks_path)?,
            scratch: Vec::new(),
            decoder,
            last_fetched: 0,
            last_syscalls: 0,
            footprint,
            dict_shared,
            _dir: dir.to_path_buf(),
        })
    }

    /// The codec.
    pub fn kind(&self) -> Codec {
        self.kind
    }

    /// Retained footprint (blocks + index + shared dictionary).
    pub fn footprint(&self) -> u64 {
        self.footprint
    }

    /// Bytes attributable to the shared trained dictionary.
    pub fn dict_shared(&self) -> u64 {
        self.dict_shared
    }

    /// Compressed bytes fetched by the last read.
    pub fn last_fetched(&self) -> u64 {
        self.last_fetched
    }

    /// `pread` syscalls performed by the last read.
    pub fn last_syscalls(&self) -> u64 {
        self.last_syscalls
    }

    /// The expected decoded length of block `b`.
    fn full_len(&self, b: usize) -> usize {
        let start = b * self.block;
        (self.n as usize - start).min(self.block)
    }

    /// The block indices covering `[offset, offset+len)`.
    pub fn covering_blocks(&self, offset: u64, len: u64) -> Vec<usize> {
        let block = self.block as u64;
        let end = offset + len;
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let b = (pos / block) as usize;
            out.push(b);
            pos = (pos - pos % block) + block;
        }
        out
    }

    /// The independent expected compressed bytes fetched for `[offset,
    /// offset+len)` — a recount from the index, used to assert the counting I/O
    /// wrapper is real.
    pub fn expected_fetched(&self, offset: u64, len: u64) -> u64 {
        self.covering_blocks(offset, len)
            .into_iter()
            .map(|b| u64::from(self.lens[b]))
            .sum()
    }

    /// The counting I/O wrapper: one `pread` of a whole compressed block, adding
    /// its length and one syscall to the per-read counters.
    fn pread_block(&mut self, b: usize) -> io::Result<()> {
        let co = self.offs[b];
        let clen = self.lens[b] as usize;
        self.scratch.resize(clen, 0);
        self.file.read_exact_at(&mut self.scratch, co)?;
        self.last_fetched += clen as u64;
        self.last_syscalls += 1;
        Ok(())
    }

    fn decode_block(&mut self, full: usize) -> Vec<u8> {
        match &mut self.decoder {
            Decoder::Lz4 => lz4_flex::decompress(&self.scratch, full).expect("lz4 block"),
            Decoder::Zstd(d) => d.decompress(&self.scratch, full).expect("zstd frame"),
        }
    }

    /// Read `[offset, offset+len)` into `out`, decompressing only the covering
    /// block(s) and updating the per-read counters.
    pub fn read_into(&mut self, offset: u64, len: u64, out: &mut Vec<u8>) {
        out.clear();
        self.last_fetched = 0;
        self.last_syscalls = 0;
        let block = self.block as u64;
        let end = offset + len;
        let mut pos = offset;
        while pos < end {
            let b = (pos / block) as usize;
            let start = pos - pos % block;
            let seg_end = end.min(start + block);
            let lo = (pos - start) as usize;
            let hi = (seg_end - start) as usize;
            self.pread_block(b).expect("compressed pread");
            let decoded = self.decode_block(self.full_len(b));
            let hi = hi.min(decoded.len());
            out.extend_from_slice(&decoded[lo..hi]);
            pos = seg_end;
        }
    }

    /// Single-byte positional read.
    pub fn byte_at(&mut self, offset: u64) -> u8 {
        let mut out = Vec::with_capacity(1);
        self.read_into(offset, 1, &mut out);
        out[0]
    }
}

// ---------------------------------------------------------------------------
// Machine-9 dictionary store.
// ---------------------------------------------------------------------------

/// One shared trained dictionary plus header-less per-record cores (machine 9,
/// `OrANS-DICT`) — a record-granular KV store.
pub struct DictStore {
    dict_shared: u64,
    cores: File,
    offs: Vec<u64>,
    scratch: Vec<u8>,
    prepared: Prepared<'static>,
    last_fetched: u64,
    last_syscalls: u64,
    footprint: u64,
    _dir: PathBuf,
}

impl DictStore {
    /// Train `dict` over `records`, encode every record as a referenced machine-9
    /// core, and write the concatenated cores plus a `u64` offset index.
    pub fn build(
        dir: &Path,
        dict: &Dictionary,
        records: &[Vec<u8>],
        source: &[u8],
    ) -> io::Result<DictStore> {
        std::fs::create_dir_all(dir)?;
        let mut cores = Vec::with_capacity(source.len());
        let mut index = Vec::with_capacity((records.len() + 1) * 8);
        let mut offs = Vec::with_capacity(records.len() + 1);
        let mut first_framed: Vec<u8> = Vec::new();
        for (i, rec) in records.iter().enumerate() {
            let core = dict
                .encode(rec)
                .map_err(|e| io::Error::other(e.to_string()))?;
            let framed = ekbloom::frame_dictionary_reference(dict, &core);
            if i == 0 {
                first_framed = framed.clone();
            }
            offs.push(cores.len() as u64);
            cores.extend_from_slice(&framed);
        }
        offs.push(cores.len() as u64);

        for o in &offs {
            index.extend_from_slice(&o.to_le_bytes());
        }
        let cores_path = dir.join("dict.cores");
        let index_path = dir.join("dict.index");
        std::fs::write(&cores_path, &cores)?;
        std::fs::write(&index_path, &index)?;

        let prepared: Prepared<'static> =
            prepare(&DICT, &first_framed).map_err(|e| io::Error::other(e.to_string()))?;
        let dict_shared = dict.physical_bytes();
        let footprint = dict_shared + alloc_bytes(&cores_path) + alloc_bytes(&index_path);

        Ok(DictStore {
            dict_shared,
            cores: File::open(&cores_path)?,
            offs,
            scratch: Vec::new(),
            prepared,
            last_fetched: 0,
            last_syscalls: 0,
            footprint,
            _dir: dir.to_path_buf(),
        })
    }

    /// Retained footprint (cores + index + shared dictionary).
    pub fn footprint(&self) -> u64 {
        self.footprint
    }

    /// Bytes attributable to the shared trained dictionary (`540`).
    pub fn dict_shared(&self) -> u64 {
        self.dict_shared
    }

    /// Encoded bytes fetched by the last read.
    pub fn last_fetched(&self) -> u64 {
        self.last_fetched
    }

    /// `pread` syscalls performed by the last read.
    pub fn last_syscalls(&self) -> u64 {
        self.last_syscalls
    }

    /// The first record index covering `offset`.
    pub fn first_record(&self, offset: u64) -> usize {
        (offset / VALUE_BYTES as u64) as usize
    }

    /// The last record index covering `[offset, offset+len)`.
    pub fn last_record(&self, offset: u64, len: u64) -> usize {
        ((offset + len - 1) / VALUE_BYTES as u64) as usize
    }

    /// The independent expected encoded bytes fetched for `[offset,
    /// offset+len)` — the contiguous core span, recounted from the index.
    pub fn expected_fetched(&self, offset: u64, len: u64) -> u64 {
        let first = self.first_record(offset);
        let last = self.last_record(offset, len);
        self.offs[last + 1] - self.offs[first]
    }

    /// Read `[offset, offset+len)` into `out`: one bounded `pread` of the
    /// covering cores (counted), then machine-9 decode against the shared
    /// dictionary.
    pub fn read_into(&mut self, offset: u64, len: u64, out: &mut Vec<u8>) {
        out.clear();
        let first = self.first_record(offset);
        let last = self.last_record(offset, len);
        let start = self.offs[first];
        let end = self.offs[last + 1];
        let span = (end - start) as usize;
        self.scratch.resize(span, 0);
        self.cores
            .read_exact_at(&mut self.scratch, start)
            .expect("core pread");
        self.last_fetched = span as u64;
        self.last_syscalls = 1;

        let mut decoded = Vec::with_capacity((last - first + 1) * VALUE_BYTES);
        for i in first..=last {
            let s = (self.offs[i] - start) as usize;
            let e = (self.offs[i + 1] - start) as usize;
            let framed = &self.scratch[s..e];
            let state = u32::from_le_bytes([framed[24], framed[25], framed[26], framed[27]]);
            let mut omega = Snapshot::new(state, DICT_CURSOR);
            self.prepared
                .decode_into(framed, &mut omega, VALUE_BYTES as u64, &mut decoded)
                .expect("machine-9 decode");
        }
        let base = (first as u64) * VALUE_BYTES as u64;
        let lo = (offset - base) as usize;
        out.extend_from_slice(&decoded[lo..lo + len as usize]);
    }
}

// ---------------------------------------------------------------------------
// EKBLOOM file-body meter.
// ---------------------------------------------------------------------------

/// Wraps a [`ekbloom::FileBody`] and reports the bytes actually fetched per read
/// from its window counters.
///
/// `FileBody::fetched_bytes` counts only the encoded core windows and restart
/// reads; a read served from a **persisted tile set** is a plain `pread` the
/// counter does not yet charge (a library gap reported in `docs/ROCKSDB.md`), so
/// this meter adds those tile bytes back explicitly from the tile geometry.
pub struct FileMeter {
    fb: ekbloom::FileBody,
    tile_shift: Option<u8>,
    logical_len: u64,
}

impl FileMeter {
    /// Open `path` and prepare the tile-aware byte meter.
    pub fn open(path: &Path) -> ekbloom::Result<FileMeter> {
        let mut fb = ekbloom::FileBody::open(path)?;
        fb.set_bounded_windows(true);
        let tile_shift = fb.tile_summaries().iter().map(|t| t.tile_shift).min();
        let logical_len = fb.logical_len();
        Ok(FileMeter {
            fb,
            tile_shift,
            logical_len,
        })
    }

    /// Read `[offset, offset+len)` and return `(bytes_fetched, syscalls)`.
    pub fn read_counted(&self, offset: u64, len: u64) -> ekbloom::Result<(Vec<u8>, u64, u64)> {
        let before_b = self.fb.fetched_bytes();
        let before_s = self.fb.fetched_reads();
        let out = self.fb.read(ekbloom::ReadRequest {
            offset,
            length: len,
        })?;
        let mut bytes = self.fb.fetched_bytes() - before_b;
        let mut syscalls = self.fb.fetched_reads() - before_s;
        if bytes == 0 && syscalls == 0 && self.fb.tiles_cover(offset, len) {
            if let Some(shift) = self.tile_shift {
                // `read_via_tiles` preads exactly the requested range, split at
                // tile boundaries: total bytes = `len`, one `pread` per tile.
                let first = offset >> shift;
                let last = (offset + len - 1) >> shift;
                bytes = len;
                syscalls = last - first + 1;
            }
        }
        Ok((out, bytes, syscalls))
    }
}
