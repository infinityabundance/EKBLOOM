//! **RocksDB-style skewed KV access court.**
//!
//! A dataset of 100-byte values (`records` × 100 B, default 4 MiB) is read with
//! an **80/20 key skew** (80 % of reads hit 20 % of records, Zipf-ish from a fixed
//! seed) under the two access patterns a storage engine actually shows:
//!
//! * `get` — a single 100-byte value read, and
//! * `read_random` — a 4 KiB, block-aligned block read.
//!
//! At an equal on-disk budget (the raw size `N`) it compares:
//!
//! * `raw` — the positional byte array;
//! * `lz4-block` — block-indexed LZ4 (4 KiB blocks + a real offset index);
//! * `zstd-seek` — block-indexed Zstandard (seekable 4 KiB frames);
//! * `zstd-dict` — the same, with a **trained dictionary** (the same training set
//!   the machine-9 dictionary is trained on);
//! * `ekbloom-dict` — machine 9 (`OrANS-DICT`): one shared trained dictionary plus
//!   header-less per-record cores (record-granular I/O); and
//! * `ekbloom-opt` — the best plan the joint optimiser admits under the budget
//!   (cold restart + hot-region tiles), read through the bounded engine.
//!
//! Per contender it reports the retained footprint (the shared dictionary
//! amortised), the **bytes fetched per logical read** (I/O amplification, from the
//! real `FileBody` window counters for EKBLOOM and a counting `pread` wrapper for
//! the codecs), the `get`/4 KiB p50/p99/p99.9 and decode throughput, and — because
//! skew is the point — the **hot-set** and **cold** get latencies. Every read is
//! asserted byte-exact before it is timed.
//!
//! Writes `evidence/bench/rocksdb-skew.json`. Run `sh scripts/run-rocksdb-skew.sh`.

#[path = "../tests/common/rocksdb.rs"]
mod rocksdb;

use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use ekbloom::{
    register_dictionary, train_dictionary, write_body, AccessTrace, CourtConfig,
    ManifestationBudget, OptimizedPlan,
};

use rocksdb::{BlockStore, Codec, DictStore, FileMeter, RawStore, ReadPlan, SeqResult};

/// A store the court can drive.
trait Kv {
    /// Read `[off, off+len)` into `out` (replacing its contents).
    fn read_out(&mut self, off: u64, len: u64, out: &mut Vec<u8>);
    /// Bytes fetched by the last read.
    fn fetched(&self) -> u64;
    /// `pread` syscalls performed by the last read.
    fn syscalls(&self) -> u64;
}

impl Kv for RawStore {
    fn read_out(&mut self, off: u64, len: u64, out: &mut Vec<u8>) {
        self.read_into(off, len, out);
    }
    fn fetched(&self) -> u64 {
        self.last_fetched()
    }
    fn syscalls(&self) -> u64 {
        0
    }
}

impl Kv for BlockStore {
    fn read_out(&mut self, off: u64, len: u64, out: &mut Vec<u8>) {
        self.read_into(off, len, out);
    }
    fn fetched(&self) -> u64 {
        self.last_fetched()
    }
    fn syscalls(&self) -> u64 {
        self.last_syscalls()
    }
}

impl Kv for DictStore {
    fn read_out(&mut self, off: u64, len: u64, out: &mut Vec<u8>) {
        self.read_into(off, len, out);
    }
    fn fetched(&self) -> u64 {
        self.last_fetched()
    }
    fn syscalls(&self) -> u64 {
        self.last_syscalls()
    }
}

/// One contender's row.
struct Contender {
    name: &'static str,
    class: &'static str,
    footprint: u64,
    dict_shared: u64,
    note: String,
    get: SeqResult,
    read4k: SeqResult,
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

fn scratch_dir() -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-rocksdb-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    dir
}

/// Measure one codec/raw/dict store over both access patterns.
fn measure_io<S: Kv>(store: &mut S, source: &[u8], plan: &ReadPlan) -> (SeqResult, SeqResult) {
    let get = rocksdb::measure_seq(
        source,
        &plan.gets,
        Some(&plan.get_hot),
        rocksdb::VALUE_BYTES as u64,
        |o, l, out| {
            store.read_out(o, l, out);
            (store.fetched(), store.syscalls())
        },
    );
    let read4k = rocksdb::measure_seq(
        source,
        &plan.reads4k,
        None,
        rocksdb::BLOCK_BYTES,
        |o, l, out| {
            store.read_out(o, l, out);
            (store.fetched(), store.syscalls())
        },
    );
    (get, read4k)
}

fn seq_json(label: &str, s: &SeqResult) -> String {
    let hot = match &s.hot {
        Some(h) => format!(
            "{{\"p50_ns\":{},\"p99_ns\":{},\"bytes_mean\":{:.1}}}",
            h.p50,
            h.p99,
            s.hot_bytes_mean.unwrap_or(0.0)
        ),
        None => "null".to_string(),
    };
    let cold = match &s.cold {
        Some(c) => format!(
            "{{\"p50_ns\":{},\"p99_ns\":{},\"bytes_mean\":{:.1}}}",
            c.p50,
            c.p99,
            s.cold_bytes_mean.unwrap_or(0.0)
        ),
        None => "null".to_string(),
    };
    let mut out = String::new();
    let _ = write!(
        out,
        "\"{label}\": {{\"bytes_fetched_mean\": {:.1}, \"bytes_fetched_max\": {}, \
         \"syscalls_mean\": {:.2}, \"p50_ns\": {}, \"p99_ns\": {}, \"p999_ns\": {}, \
         \"mean_ns\": {:.1}, \"mib_per_s\": {:.1}, \"hot\": {hot}, \"cold\": {cold}}}",
        s.bytes_mean,
        s.bytes_max,
        s.syscalls_mean,
        s.lat.p50,
        s.lat.p99,
        s.lat.p999,
        s.lat.mean,
        s.mib_per_s(),
    );
    out
}

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

fn emit_row(js: &mut String, r: &Contender, raw: u64, comma: &str) {
    let _ = writeln!(js, "    {{");
    let _ = writeln!(js, "      \"name\": \"{}\",", r.name);
    let _ = writeln!(js, "      \"class\": \"{}\",", r.class);
    let _ = writeln!(js, "      \"footprint_bytes\": {},", r.footprint);
    let _ = writeln!(
        js,
        "      \"footprint_ratio\": {:.4},",
        ratio(r.footprint, raw)
    );
    let _ = writeln!(js, "      \"dict_shared_bytes\": {},", r.dict_shared);
    let _ = writeln!(js, "      \"note\": \"{}\",", json_escape(&r.note));
    let _ = writeln!(js, "      {},", seq_json("get", &r.get));
    let _ = writeln!(js, "      {}", seq_json("read4k", &r.read4k));
    let _ = writeln!(js, "    }}{comma}");
}

fn find<'a>(rows: &'a [Contender], name: &str) -> &'a Contender {
    rows.iter()
        .find(|r| r.name == name)
        .expect("contender present")
}

fn main() -> ekbloom::Result<()> {
    let records = env_usize("EKB_ROCKSDB_RECORDS", rocksdb::DEFAULT_RECORDS);
    let gets = env_usize("EKB_ROCKSDB_GETS", 16_384);
    let reads4k = env_usize("EKB_ROCKSDB_READS", 2_048);
    let seed = 0x5D3B_100E_0000_0001u64;

    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let scratch = scratch_dir();
    std::fs::create_dir_all(&scratch)?;

    let dataset = rocksdb::dataset(records);
    let source = &dataset.source;
    let n = source.len() as u64;
    let hot_bytes = dataset.hot_records as u64 * rocksdb::VALUE_BYTES as u64;
    let plan = rocksdb::read_plan(&dataset, gets, reads4k, seed);
    let measured_hot_fraction =
        plan.get_hot.iter().filter(|&&h| h).count() as f64 / plan.get_hot.len().max(1) as f64;

    // ---- the shared training set (identical for machine-9 and zstd-dict) ------
    let refs: Vec<&[u8]> = dataset.records.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs)?;
    register_dictionary(&dict)?;
    let zdict_bytes = zstd::dict::from_samples(&refs, rocksdb::ZSTD_DICT_BYTES)?;
    let zdict: &'static [u8] = Box::leak(zdict_bytes.into_boxed_slice());

    let mut rows: Vec<Contender> = Vec::new();

    // ---- raw ----------------------------------------------------------------
    let mut raw = RawStore::new(source);
    let (get, read4k) = measure_io(&mut raw, source, &plan);
    rows.push(Contender {
        name: "raw",
        class: "baseline",
        footprint: raw.footprint(),
        dict_shared: 0,
        note: "positional byte array".to_string(),
        get,
        read4k,
    });

    // ---- block-indexed codecs ----------------------------------------------
    for (name, codec, d) in [
        ("lz4-block", Codec::Lz4, None),
        ("zstd-seek", Codec::Zstd, None),
        ("zstd-dict", Codec::ZstdDict, Some(zdict)),
    ] {
        let mut store = BlockStore::build(&scratch.join(name), codec, source, d).map_err(|e| {
            ekbloom::EkbloomError::InvalidArgument {
                what: e.to_string(),
            }
        })?;
        let (get, read4k) = measure_io(&mut store, source, &plan);
        rows.push(Contender {
            name,
            class: "baseline",
            footprint: store.footprint(),
            dict_shared: store.dict_shared(),
            note: format!("4 KiB blocks, {} index", name),
            get,
            read4k,
        });
    }

    // ---- machine 9 (shared dictionary) -------------------------------------
    let mut dict_store = DictStore::build(&scratch.join("dict"), &dict, &dataset.records, source)
        .map_err(|e| ekbloom::EkbloomError::InvalidArgument {
        what: e.to_string(),
    })?;
    let (get, read4k) = measure_io(&mut dict_store, source, &plan);
    rows.push(Contender {
        name: "ekbloom-dict",
        class: "ekbloom",
        footprint: dict_store.footprint(),
        dict_shared: dict_store.dict_shared(),
        note: format!(
            "OrANS-DICT machine 9, {} B dictionary",
            dict.physical_bytes()
        ),
        get,
        read4k,
    });

    // ---- EKBLOOM best plan under budget = raw ------------------------------
    let trace = AccessTrace {
        regions: vec![(0, hot_bytes), (0, n)],
        counts: vec![800, 200],
    };
    let budget = ManifestationBudget {
        maximum_physical_bytes: n,
    };
    let cfg = CourtConfig {
        reps: 1,
        interleave: true,
        seed: 0x0E0B_10E0_5DB5_0001,
    };
    let (optimized, _receipts) = OptimizedPlan::optimize(source, &trace, &budget, &cfg)?;
    let body = optimized.build(source)?;
    let ekb_physical = body.physical_bytes();

    // Read through the file-backed path (I/O + decode), so EKBLOOM is measured the
    // same way as the codec stores; the real window counters give the bytes fetched.
    let ekb_path = scratch.join("kv.ekb");
    write_body(&ekb_path, &body)?;
    let meter = FileMeter::open(&ekb_path)?;
    let get = rocksdb::measure_seq(
        source,
        &plan.gets,
        Some(&plan.get_hot),
        rocksdb::VALUE_BYTES as u64,
        |o, l, out| {
            let (data, bytes, syscalls) = meter.read_counted(o, l).expect("meter get");
            out.clear();
            out.extend_from_slice(&data);
            (bytes, syscalls)
        },
    );
    let read4k = rocksdb::measure_seq(
        source,
        &plan.reads4k,
        None,
        rocksdb::BLOCK_BYTES,
        |o, l, out| {
            let (data, bytes, syscalls) = meter.read_counted(o, l).expect("meter 4k");
            out.clear();
            out.extend_from_slice(&data);
            (bytes, syscalls)
        },
    );

    rows.push(Contender {
        name: "ekbloom-opt",
        class: "ekbloom",
        footprint: ekb_physical,
        dict_shared: 0,
        note: optimized.summary(),
        get,
        read4k,
    });

    // ---- report -------------------------------------------------------------
    println!(
        "rocksdb-skew: {records} records x {} B = {n} B, hot {hot_bytes} B, \
         {gets} gets + {reads4k} 4k reads (hot fraction {measured_hot_fraction:.3})",
        rocksdb::VALUE_BYTES
    );
    println!(
        "{:<14} {:>10} {:>7} {:>10} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "contender",
        "footprint",
        "F/N",
        "get B/read",
        "get p50",
        "get p99.9",
        "hot p50",
        "cold p50",
        "4k B/read"
    );
    for r in &rows {
        println!(
            "{:<14} {:>10} {:>7.3} {:>10.1} {:>9} {:>9} {:>9} {:>9} {:>9.1}",
            r.name,
            r.footprint,
            ratio(r.footprint, n),
            r.get.bytes_mean,
            r.get.lat.p50,
            r.get.lat.p999,
            r.get.hot.as_ref().map(|h| h.p50).unwrap_or(0),
            r.get.cold.as_ref().map(|c| c.p50).unwrap_or(0),
            r.read4k.bytes_mean,
        );
    }

    let raw_row = find(&rows, "raw");
    let lz4_row = find(&rows, "lz4-block");
    let zdict_row = find(&rows, "zstd-dict");
    let m9_row = find(&rows, "ekbloom-dict");
    let opt_row = find(&rows, "ekbloom-opt");

    let verdict = format!(
        "On skewed 100-byte KV reads the block-indexed codecs pay a whole 4 KiB block per \
         value: raw fetches {raw_get:.0} B/get, lz4 {lz4_get:.0} B/get and zstd-dict \
         {zd_get:.0} B/get, against {m9_get:.0} B/get for EKBLOOM machine 9 \
         ({amp:.1}x less I/O than zstd-dict) and {opt_get:.0} B/get for EKBLOOM's best plan \
         (hot tiles). zstd-dict is the smallest store ({zd_fp:.3}x raw) and the fastest \
         small-record decoder ({zd_p50} ns vs {m9_p50} ns get p50); EKBLOOM machine 9 is \
         {m9_fp:.3}x raw, costs {m9_get:.0} B/get (about the value) and has the tightest get \
         tail ({m9_p999} ns p99.9 vs {zd_p999} ns). EKBLOOM-opt's hot tiles serve hot gets at \
         {opt_hot} ns p50 / {opt_hot_bytes:.0} B/get and cold gets at {opt_cold} ns p50 / \
         {opt_cold_bytes:.0} B/get. Net: on \
         skewed small-value KV, EKBLOOM machine 9 buys a real I/O-amplification and tail \
         tradeoff against the block codecs, but zstd-dict stays smaller and decodes faster - \
         stated plainly.",
        raw_get = raw_row.get.bytes_mean,
        lz4_get = lz4_row.get.bytes_mean,
        zd_get = zdict_row.get.bytes_mean,
        m9_get = m9_row.get.bytes_mean,
        amp = if m9_row.get.bytes_mean > 0.0 {
            zdict_row.get.bytes_mean / m9_row.get.bytes_mean
        } else {
            0.0
        },
        opt_get = opt_row.get.bytes_mean,
        zd_fp = ratio(zdict_row.footprint, n),
        m9_fp = ratio(m9_row.footprint, n),
        zd_p50 = zdict_row.get.lat.p50,
        m9_p50 = m9_row.get.lat.p50,
        m9_p999 = m9_row.get.lat.p999,
        zd_p999 = zdict_row.get.lat.p999,
        opt_hot = opt_row.get.hot.as_ref().map(|h| h.p50).unwrap_or(0),
        opt_cold = opt_row.get.cold.as_ref().map(|c| c.p50).unwrap_or(0),
        opt_hot_bytes = opt_row.get.hot_bytes_mean.unwrap_or(0.0),
        opt_cold_bytes = opt_row.get.cold_bytes_mean.unwrap_or(0.0),
    );

    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/rocksdb_skew.rs (sh scripts/run-rocksdb-skew.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"method\": \"RocksDB-style skewed KV court: 100-byte values, 80/20 key skew (Zipf-ish, fixed seed), get + block-aligned read_random 4 KiB, at budget = raw size N; bytes_fetched are real (FileBody window counters for EKBLOOM, a counting pread wrapper for the codecs); every read asserted byte-exact\","
    );
    let _ = writeln!(js, "  \"records\": {records},");
    let _ = writeln!(js, "  \"value_bytes\": {},", rocksdb::VALUE_BYTES);
    let _ = writeln!(js, "  \"raw_bytes\": {n},");
    let _ = writeln!(js, "  \"hot_records\": {},", dataset.hot_records);
    let _ = writeln!(js, "  \"hot_read_pct\": {},", rocksdb::HOT_READ_PCT);
    let _ = writeln!(
        js,
        "  \"hot_read_fraction_measured\": {measured_hot_fraction:.4},"
    );
    let _ = writeln!(js, "  \"gets\": {gets},");
    let _ = writeln!(js, "  \"reads_4k\": {reads4k},");
    let _ = writeln!(js, "  \"seed\": {seed},");
    let _ = writeln!(js, "  \"budget_bytes\": {n},");
    let _ = writeln!(js, "  \"zstd_dict_bytes\": {},", zdict.len());
    let _ = writeln!(
        js,
        "  \"ekbloom_plan\": \"{}\",",
        json_escape(&optimized.summary())
    );
    let _ = writeln!(js, "  \"contenders\": [");
    for (i, r) in rows.iter().enumerate() {
        let comma = if i + 1 == rows.len() { "" } else { "," };
        emit_row(&mut js, r, n, comma);
    }
    let _ = writeln!(js, "  ],");
    let _ = writeln!(js, "  \"verdict\": \"{}\"", json_escape(&verdict));
    js.push_str("}\n");

    std::fs::write(dir.join("rocksdb-skew.json"), js)?;
    let _ = std::fs::remove_dir_all(&scratch);
    println!("wrote evidence/bench/rocksdb-skew.json");
    Ok(())
}
