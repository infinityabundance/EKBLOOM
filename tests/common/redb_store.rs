//! Shared support for the **real embedded KV-engine integration court**
//! (`examples/kv_integration.rs`, `tests/kv_integration.rs`).
//!
//! The standalone `rocksdb_skew` court ([`tests/common/rocksdb.rs`]) measures the
//! codecs as *files* driven by a hand-written read loop. This module measures them
//! the way a reviewer actually asked: as **stored values inside a real embedded
//! KV engine** — [`redb`], a pure-Rust, single-file, copy-on-write B-tree with no C
//! toolchain and no `unsafe` in its public API.
//!
//! Each contender opens its own `redb` database and stores, under increasing `u64`
//! keys, the *encoded* form of the same deterministic 100-byte records:
//!
//! | codec | stored value | decode on read |
//! |---|---|---|
//! | [`Codec::Raw`] | the 100-byte record | copy |
//! | [`Codec::Lz4`] | `lz4_flex` block | `lz4_flex::decompress` |
//! | [`Codec::ZstdDict`] | zstd frame under a trained dictionary | `zstd` decode |
//! | [`Codec::EkbloomDict`] | machine-9 `Dictionary::encode_record` core | `DICT.decode` |
//!
//! The shared trained dictionary (machine-9's 540-byte model, or the zstd
//! dictionary) is stored **once**, under a metadata key in a second table, so it is
//! part of the database footprint — exactly as a real store would keep it.
//!
//! Everything here is deterministic (fixed dataset, fixed key space); only the
//! latency samples vary. `redb` errors are internal to the court and are surfaced
//! as panics (this module is test/example support, never library code).

// Used by one target but not the other (example vs test).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ekbloom::{register_dictionary, Dictionary, OrMachine, DICT};
use redb::{Database, ReadableDatabase, TableDefinition};

use crate::rocksdb::{alloc_bytes, summarize, Lat, VALUE_BYTES};

/// The value table: increasing `u64` ids → encoded 100-byte records.
pub const VALUE_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("values");
/// The metadata table: named blobs (the shared dictionary lives here).
pub const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// Metadata key under which the shared trained dictionary is stored once.
pub const DICT_META_KEY: &str = "shared_dictionary";
/// redb page-cache cap for every database. It is deliberately **smaller than the
/// largest store and larger than the smallest**, so page residency (warm vs cold)
/// is a real, measurable effect: a codec that halves the store also halves the
/// pages a fixed cache must hold, which is a genuine database-level benefit.
pub const CACHE_BYTES: usize = 2 * 1024 * 1024;

/// Which value codec a database stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// The 100-byte record verbatim.
    Raw,
    /// A `lz4_flex` block per record.
    Lz4,
    /// A zstd frame per record under one trained, stored dictionary.
    ZstdDict,
    /// EKBLOOM machine 9 (`OrANS-DICT`): one trained dictionary + a referenced
    /// header-less per-record core.
    EkbloomDict,
}

impl Codec {
    /// Every contender, in report order.
    pub const ALL: [Codec; 4] = [Codec::EkbloomDict, Codec::ZstdDict, Codec::Lz4, Codec::Raw];

    /// The report label.
    pub fn label(self) -> &'static str {
        match self {
            Codec::Raw => "raw",
            Codec::Lz4 => "lz4-block",
            Codec::ZstdDict => "zstd-dict",
            Codec::EkbloomDict => "ekbloom-m9",
        }
    }

    /// Whether this codec stores a shared dictionary under [`DICT_META_KEY`].
    pub fn uses_dict(self) -> bool {
        matches!(self, Codec::ZstdDict | Codec::EkbloomDict)
    }
}

/// Encodes records into the stored value bytes for one codec.
pub enum Encoder {
    /// Identity.
    Raw,
    /// LZ4 block.
    Lz4,
    /// Zstandard under a trained dictionary.
    ZstdDict(zstd::bulk::Compressor<'static>),
    /// EKBLOOM machine 9, referencing the shared dictionary (boxed to keep the
    /// enum small).
    EkbloomDict(Box<Dictionary>),
}

impl Encoder {
    /// Build the encoder for `codec`. `zdict` is the trained zstd dictionary
    /// (required for [`Codec::ZstdDict`]); `dict` is the trained machine-9
    /// dictionary (required for [`Codec::EkbloomDict`]).
    pub fn new(codec: Codec, zdict: Option<&[u8]>, dict: Option<&Dictionary>) -> Encoder {
        match codec {
            Codec::Raw => Encoder::Raw,
            Codec::Lz4 => Encoder::Lz4,
            Codec::ZstdDict => Encoder::ZstdDict(
                zstd::bulk::Compressor::with_dictionary(
                    crate::rocksdb::ZSTD_LEVEL,
                    zdict.expect("zstd dictionary"),
                )
                .expect("zstd compressor"),
            ),
            Codec::EkbloomDict => {
                let d = dict.expect("machine-9 dictionary").clone();
                // `encode_record` registers the dictionary; register once up front
                // so the store is decodable even before the first record is written.
                register_dictionary(&d).expect("register dictionary");
                Encoder::EkbloomDict(Box::new(d))
            }
        }
    }

    /// Encode one record.
    pub fn encode(&mut self, record: &[u8]) -> Vec<u8> {
        match self {
            Encoder::Raw => record.to_vec(),
            Encoder::Lz4 => lz4_flex::compress(record),
            Encoder::ZstdDict(c) => c.compress(record).expect("zstd compress"),
            Encoder::EkbloomDict(d) => d.encode_record(record).expect("machine-9 encode_record"),
        }
    }
}

/// Decodes stored value bytes back to a record.
pub enum Decoder {
    /// Identity.
    Raw,
    /// LZ4 block.
    Lz4,
    /// Zstandard under the stored dictionary.
    ZstdDict(zstd::bulk::Decompressor<'static>),
    /// EKBLOOM machine 9 against the registered shared dictionary (boxed).
    EkbloomDict(Box<Dictionary>),
}

impl Decoder {
    /// Build the decoder for `codec`. `zdict` is the dictionary bytes as loaded
    /// from the database's metadata key; `dict` is the resolved machine-9
    /// dictionary (also already registered).
    pub fn new(codec: Codec, zdict: Option<&[u8]>, dict: Option<Dictionary>) -> Decoder {
        match codec {
            Codec::Raw => Decoder::Raw,
            Codec::Lz4 => Decoder::Lz4,
            Codec::ZstdDict => Decoder::ZstdDict(
                zstd::bulk::Decompressor::with_dictionary(zdict.expect("zstd dictionary"))
                    .expect("zstd decompressor"),
            ),
            Codec::EkbloomDict => {
                let d = dict.expect("machine-9 dictionary");
                register_dictionary(&d).expect("register dictionary");
                Decoder::EkbloomDict(Box::new(d))
            }
        }
    }

    /// Decode `value` into `out` (clearing it first).
    pub fn decode(&mut self, value: &[u8], out: &mut Vec<u8>) {
        out.clear();
        match self {
            Decoder::Raw => out.extend_from_slice(value),
            Decoder::Lz4 => {
                out.extend_from_slice(&lz4_flex::decompress(value, VALUE_BYTES).expect("lz4 block"))
            }
            Decoder::ZstdDict(d) => {
                out.extend_from_slice(&d.decompress(value, VALUE_BYTES).expect("zstd frame"));
            }
            Decoder::EkbloomDict(_) => {
                let rec = DICT
                    .decode(value, VALUE_BYTES as u64)
                    .expect("machine-9 decode");
                out.extend_from_slice(&rec);
            }
        }
    }
}

/// The result of loading a database.
pub struct Built {
    /// The open database.
    pub db: Database,
    /// Its single file path.
    pub path: PathBuf,
    /// Sum of stored (encoded) value lengths, excluding keys and B-tree pages.
    pub values_bytes: u64,
    /// Bytes of the shared dictionary stored once under [`DICT_META_KEY`].
    pub dict_shared_bytes: u64,
    /// Bytes written to storage (process `wchar` delta) while loading.
    pub write_bytes: u64,
    /// Logical bytes loaded (`records * VALUE_BYTES`).
    pub logical_bytes: u64,
    /// Wall time of the whole load (create + insert `commit`).
    pub load_ns: u64,
}

/// Build (or overwrite) a `redb` database at `dir/kv.redb` holding `records`
/// encoded by `codec`, plus the shared dictionary under the metadata key.
pub fn build(
    dir: &Path,
    codec: Codec,
    records: &[Vec<u8>],
    zdict: Option<&[u8]>,
    dict: Option<&Dictionary>,
) -> Built {
    fs::create_dir_all(dir).expect("scratch dir");
    let path = dir.join("kv.redb");
    let _ = fs::remove_file(&path);

    let w0 = proc_wchar();
    let t0 = Instant::now();
    let db = Database::builder()
        .set_cache_size(CACHE_BYTES)
        .create(&path)
        .expect("redb create");

    let mut encoder = Encoder::new(codec, zdict, dict);
    let mut values_bytes = 0u64;
    let wtx = db.begin_write().expect("begin_write");
    {
        let mut values = wtx.open_table(VALUE_TABLE).expect("open values");
        for (i, rec) in records.iter().enumerate() {
            let v = encoder.encode(rec);
            values_bytes += v.len() as u64;
            values.insert(i as u64, v.as_slice()).expect("insert value");
        }
    }
    let mut dict_shared_bytes = 0u64;
    if codec.uses_dict() {
        let bytes = match codec {
            Codec::ZstdDict => zdict.expect("zstd dictionary").to_vec(),
            Codec::EkbloomDict => dict.expect("machine-9 dictionary").to_bytes(),
            _ => unreachable!("uses_dict is only zstd/ekbloom"),
        };
        dict_shared_bytes = bytes.len() as u64;
        let mut meta = wtx.open_table(META_TABLE).expect("open meta");
        meta.insert(DICT_META_KEY, bytes.as_slice())
            .expect("insert dictionary");
    }
    wtx.commit().expect("commit");
    let write_bytes = proc_wchar().saturating_sub(w0);
    let load_ns = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;

    Built {
        db,
        path,
        values_bytes,
        dict_shared_bytes,
        write_bytes,
        logical_bytes: (records.len() as u64) * VALUE_BYTES as u64,
        load_ns,
    }
}

/// Reopen an existing database (proving the on-disk file is a valid store).
pub fn reopen(path: &Path) -> Database {
    Database::builder()
        .set_cache_size(CACHE_BYTES)
        .open(path)
        .expect("redb reopen")
}

/// Read the shared dictionary blob back from the metadata table, if present.
pub fn load_stored_dictionary(db: &Database) -> Option<Vec<u8>> {
    let rtx = db.begin_read().ok()?;
    let meta = rtx.open_table(META_TABLE).ok()?;
    let guard = meta.get(DICT_META_KEY).ok()??;
    Some(guard.value().to_vec())
}

/// Build the decoder for `codec`, resolving the shared dictionary from the
/// database's metadata table — exactly as a reopened process must, so the stored
/// dictionary is genuinely on the decode path.
pub fn decoder(codec: Codec, db: &Database) -> Decoder {
    let stored = load_stored_dictionary(db);
    match codec {
        Codec::EkbloomDict => {
            let bytes = stored.expect("machine-9 dictionary must be stored in the db");
            let dict = Dictionary::from_bytes(&bytes).expect("parse stored dictionary");
            Decoder::new(codec, None, Some(dict))
        }
        Codec::ZstdDict => Decoder::new(codec, stored.as_deref(), None),
        Codec::Raw | Codec::Lz4 => Decoder::new(codec, None, None),
    }
}

/// One timed point-get pass.
pub struct GetPass {
    /// Latency distribution over the pass.
    pub lat: Lat,
    /// Encoded (stored) bytes returned.
    pub encoded_bytes: u64,
    /// Logical bytes served.
    pub decoded_bytes: u64,
}

/// Time a skewed point-get pass: `table.get(id)` + decode, asserting each value is
/// byte-exact against `records`.
pub fn point_gets(
    db: &Database,
    dec: &mut Decoder,
    records: &[Vec<u8>],
    indices: &[usize],
) -> GetPass {
    let rtx = db.begin_read().expect("begin_read");
    let table = rtx.open_table(VALUE_TABLE).expect("open values");
    let mut samples = Vec::with_capacity(indices.len());
    let mut encoded_bytes = 0u64;
    let mut decoded_bytes = 0u64;
    let mut out = Vec::with_capacity(VALUE_BYTES);
    for &i in indices {
        let t0 = Instant::now();
        let guard = table
            .get(i as u64)
            .expect("get")
            .expect("value present for every id");
        let value = guard.value();
        encoded_bytes += value.len() as u64;
        dec.decode(value, &mut out);
        let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        assert_eq!(out, records[i], "byte-exact get at {i}");
        decoded_bytes += out.len() as u64;
        samples.push(dt);
    }
    GetPass {
        lat: summarize(samples),
        encoded_bytes,
        decoded_bytes,
    }
}

/// One timed range-scan pass over `[0, end)`.
pub struct ScanPass {
    /// Wall time of the whole scan.
    pub elapsed_ns: u64,
    /// Logical bytes served (decoded).
    pub decoded_bytes: u64,
    /// Encoded (stored) bytes read.
    pub encoded_bytes: u64,
    /// Records scanned.
    pub records: u64,
}

impl ScanPass {
    /// Logical scan throughput in MiB/s.
    pub fn mib_per_s(&self) -> f64 {
        if self.elapsed_ns == 0 {
            0.0
        } else {
            self.decoded_bytes as f64 * 1e9 / (self.elapsed_ns as f64) / (1024.0 * 1024.0)
        }
    }
}

/// Time one ascending range scan over keys `[0, end)`, decoding every value and
/// asserting it is byte-exact against `records`.
pub fn scan_once(db: &Database, dec: &mut Decoder, records: &[Vec<u8>], end: u64) -> ScanPass {
    let rtx = db.begin_read().expect("begin_read");
    let table = rtx.open_table(VALUE_TABLE).expect("open values");
    let range = table.range(0u64..end).expect("range");
    let mut out = Vec::with_capacity(VALUE_BYTES);
    let mut decoded_bytes = 0u64;
    let mut encoded_bytes = 0u64;
    let mut scanned = 0u64;
    let t0 = Instant::now();
    for item in range {
        let (_key, value) = item.expect("range item");
        let v = value.value();
        encoded_bytes += v.len() as u64;
        dec.decode(v, &mut out);
        assert_eq!(
            out, records[scanned as usize],
            "byte-exact scan at {scanned}"
        );
        decoded_bytes += out.len() as u64;
        scanned += 1;
    }
    let elapsed_ns = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    ScanPass {
        elapsed_ns,
        decoded_bytes,
        encoded_bytes,
        records: scanned,
    }
}

/// On-disk footprint of a database file (`du`-style allocated bytes).
pub fn footprint(path: &Path) -> u64 {
    alloc_bytes(path)
}

// ---------------------------------------------------------------------------
// /proc/self/io — process-wide byte counters (page-cache independent).
// ---------------------------------------------------------------------------

fn proc_io_field(field: &str) -> u64 {
    fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix(field)
                    .map(|rest| rest.trim_start_matches(':').trim().parse().unwrap_or(0))
            })
        })
        .unwrap_or(0)
}

/// Bytes read by the process via `read`/`pread` (`rchar`).
pub fn proc_rchar() -> u64 {
    proc_io_field("rchar")
}

/// Bytes written by the process via `write`/`pwrite` (`wchar`).
pub fn proc_wchar() -> u64 {
    proc_io_field("wchar")
}
