//! **Real embedded KV-engine integration court.**
//!
//! The reviewer's P0: evaluate EKBLOOM *inside a real embedded KV store*, not as
//! standalone codec calls. This example builds a small database workload on top of
//! [`redb`] — a pure-Rust, single-file, copy-on-write B-tree with no C toolchain
//! and no `unsafe` in its public API — where the **stored values are codec
//! outputs**:
//!
//! * `raw` — the 100-byte record verbatim;
//! * `lz4-block` — an `lz4_flex` block per record;
//! * `zstd-dict` — a zstd frame per record under one trained dictionary;
//! * `ekbloom-m9` — machine 9 (`OrANS-DICT`, [`Dictionary::train`] +
//!   [`Dictionary::encode_record`]): one shared trained model + a referenced,
//!   header-less per-record core.
//!
//! Keys are increasing `u64` ids and every value is decoded on read and asserted
//! byte-exact. The shared trained dictionary (machine-9's 540-byte model, or the
//! zstd dictionary) is stored **once** under a metadata key, so it is *inside* the
//! database footprint.
//!
//! It reports the **database-level** metrics the reviewer asked for: point-get
//! p50/p99/p99.9 under an 80/20 skew; range-scan throughput; reopen cost; cache
//! residency (warm vs cold, with a redb page cache between the smallest and largest
//! store, so compression shrinks the resident set); **write amplification** (bytes
//! written to the DB per logical byte, from
//! the process `wchar` counter); total DB file footprint; and the **bytes fetched**
//! per get (stored/encoded bytes, plus the process `rchar` the engine actually
//! read). A separate `ledger_reference` row measures the same machine-9 records
//! through the standing record-granular store whose counting `pread` wrapper is
//! EKBLOOM's central-ledger analogue.
//!
//! Writes `evidence/bench/kv-integration.json`. Run `sh scripts/run-kv-integration.sh`.

#[path = "../tests/common/redb_store.rs"]
mod redb_store;
#[path = "../tests/common/rocksdb.rs"]
mod rocksdb;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use ekbloom::{train_dictionary, Dictionary};
use redb_store::Codec;
use rocksdb::{dataset, measure_seq, read_plan, DictStore, Lat, VALUE_BYTES};

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

fn scratch_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-kv-integration-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    dir
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

/// One contender's complete measured row.
struct Row {
    name: &'static str,
    footprint_bytes: u64,
    footprint_as_loaded: u64,
    values_bytes: u64,
    dict_shared_bytes: u64,
    write_bytes: u64,
    logical_bytes: u64,
    load_ms: f64,
    reopen_us: f64,
    gets: usize,
    cold: Lat,
    warm: Lat,
    warm_encoded_bytes: u64,
    cold_storage_bytes: u64,
    warm_storage_bytes: u64,
    scan_best_mib_s: f64,
    scan_encoded_bytes: u64,
    scan_records: u64,
}

impl Row {
    fn write_amp(&self) -> f64 {
        ratio(self.write_bytes, self.logical_bytes)
    }
    fn footprint_ratio(&self, raw: u64) -> f64 {
        ratio(self.footprint_bytes, raw)
    }
    fn cold_b_per_get(&self) -> f64 {
        if self.gets == 0 {
            0.0
        } else {
            self.cold_storage_bytes as f64 / self.gets as f64
        }
    }
    fn warm_b_per_get(&self) -> f64 {
        if self.gets == 0 {
            0.0
        } else {
            self.warm_storage_bytes as f64 / self.gets as f64
        }
    }
    fn enc_b_per_get(&self) -> f64 {
        if self.gets == 0 {
            0.0
        } else {
            self.warm_encoded_bytes as f64 / self.gets as f64
        }
    }
}

/// Measure one codec end-to-end: load → reopen → cold gets → warm gets → scan.
fn measure(
    scratch: &Path,
    codec: Codec,
    records: &[Vec<u8>],
    indices: &[usize],
    zdict: Option<&[u8]>,
    dict: &Dictionary,
    scan_reps: usize,
) -> Row {
    let mut built = redb_store::build(
        &scratch.join(codec.label()),
        codec,
        records,
        zdict,
        Some(dict),
    );
    // Footprint at steady state: a real store reclaims copy-on-write fragmentation
    // with a compaction, so report the compacted file size (and the as-loaded one).
    let footprint_as_loaded = redb_store::footprint(&built.path);
    built.db.compact().expect("redb compact");
    let footprint_bytes = redb_store::footprint(&built.path);
    let logical_bytes = built.logical_bytes;

    // Drop the writer, then reopen from disk — the file must be a valid store.
    drop(built.db);
    let t0 = Instant::now();
    let db = redb_store::reopen(&built.path);
    let reopen_us = t0.elapsed().as_nanos() as f64 / 1_000.0;

    let mut dec = redb_store::decoder(codec, &db);

    // Cold: first touch after reopen (redb's page cache starts empty).
    let r0 = redb_store::proc_rchar();
    let cold = redb_store::point_gets(&db, &mut dec, records, indices);
    let cold_storage_bytes = redb_store::proc_rchar().saturating_sub(r0);

    // Warm: after a warm-up pass, the resident pages are hot.
    let _ = redb_store::point_gets(&db, &mut dec, records, indices);
    let r0 = redb_store::proc_rchar();
    let warm = redb_store::point_gets(&db, &mut dec, records, indices);
    let warm_storage_bytes = redb_store::proc_rchar().saturating_sub(r0);

    // Range scan: ascending over the whole key space, best of `scan_reps`.
    let end = records.len() as u64;
    let mut scan_best_mib_s = 0.0f64;
    let mut scan_encoded_bytes = 0u64;
    let mut scan_records = 0u64;
    for _ in 0..scan_reps.max(1) {
        let pass = redb_store::scan_once(&db, &mut dec, records, end);
        scan_best_mib_s = scan_best_mib_s.max(pass.mib_per_s());
        scan_encoded_bytes = pass.encoded_bytes;
        scan_records = pass.records;
    }

    Row {
        name: codec.label(),
        footprint_bytes,
        footprint_as_loaded,
        values_bytes: built.values_bytes,
        dict_shared_bytes: built.dict_shared_bytes,
        write_bytes: built.write_bytes,
        logical_bytes,
        load_ms: built.load_ns as f64 / 1e6,
        reopen_us,
        gets: indices.len(),
        cold: cold.lat,
        warm: warm.lat,
        warm_encoded_bytes: warm.encoded_bytes,
        cold_storage_bytes,
        warm_storage_bytes,
        scan_best_mib_s,
        scan_encoded_bytes,
        scan_records,
    }
}

fn emit_row(js: &mut String, r: &Row, raw: u64, comma: &str) {
    let _ = writeln!(js, "    {{");
    let _ = writeln!(js, "      \"name\": \"{}\",", r.name);
    let _ = writeln!(js, "      \"class\": \"kv-store\",");
    let _ = writeln!(js, "      \"footprint_bytes\": {},", r.footprint_bytes);
    let _ = writeln!(
        js,
        "      \"footprint_bytes_as_loaded\": {},",
        r.footprint_as_loaded
    );
    let _ = writeln!(
        js,
        "      \"footprint_ratio\": {:.4},",
        r.footprint_ratio(raw)
    );
    let _ = writeln!(js, "      \"logical_bytes\": {},", r.logical_bytes);
    let _ = writeln!(js, "      \"stored_value_bytes\": {},", r.values_bytes);
    let _ = writeln!(
        js,
        "      \"stored_dictionary_bytes\": {},",
        r.dict_shared_bytes
    );
    let _ = writeln!(js, "      \"write_bytes\": {},", r.write_bytes);
    let _ = writeln!(js, "      \"write_amp\": {:.4},", r.write_amp());
    let _ = writeln!(js, "      \"load_ms\": {:.3},", r.load_ms);
    let _ = writeln!(js, "      \"reopen_us\": {:.3},", r.reopen_us);
    let _ = writeln!(js, "      \"gets\": {},", r.gets);
    let _ = writeln!(
        js,
        "      \"get_cold\": {{\"p50_ns\": {}, \"p99_ns\": {}, \"p999_ns\": {}}},",
        r.cold.p50, r.cold.p99, r.cold.p999
    );
    let _ = writeln!(
        js,
        "      \"get_warm\": {{\"p50_ns\": {}, \"p99_ns\": {}, \"p999_ns\": {}}},",
        r.warm.p50, r.warm.p99, r.warm.p999
    );
    let _ = writeln!(
        js,
        "      \"encoded_bytes_per_get\": {:.2},",
        r.enc_b_per_get()
    );
    let _ = writeln!(
        js,
        "      \"storage_bytes_per_get_cold\": {:.2},",
        r.cold_b_per_get()
    );
    let _ = writeln!(
        js,
        "      \"storage_bytes_per_get_warm\": {:.2},",
        r.warm_b_per_get()
    );
    let _ = writeln!(
        js,
        "      \"scan_best_mib_per_s\": {:.2},",
        r.scan_best_mib_s
    );
    let _ = writeln!(
        js,
        "      \"scan_encoded_bytes\": {},",
        r.scan_encoded_bytes
    );
    let _ = writeln!(js, "      \"scan_records\": {}", r.scan_records);
    let _ = writeln!(js, "    }}{comma}");
}

fn find<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter().find(|r| r.name == name).expect("row present")
}

fn main() -> ekbloom::Result<()> {
    let records = env_usize("EKB_KV_RECORDS", rocksdb::DEFAULT_RECORDS);
    let gets = env_usize("EKB_KV_GETS", 16_384);
    let scan_reps = env_usize("EKB_KV_SCAN_REPS", 3);
    let seed = 0x5D3B_100E_0000_0001u64;

    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let scratch = scratch_dir();
    std::fs::create_dir_all(&scratch)?;

    let ds = dataset(records);
    let source = &ds.source;
    let n = source.len() as u64;
    let plan = read_plan(&ds, gets, 0, seed);
    let indices: Vec<usize> = plan
        .gets
        .iter()
        .map(|&o| (o / VALUE_BYTES as u64) as usize)
        .collect();
    let measured_hot_fraction =
        plan.get_hot.iter().filter(|&&h| h).count() as f64 / plan.get_hot.len().max(1) as f64;

    // The shared training set (identical for machine-9 and zstd-dict).
    let refs: Vec<&[u8]> = ds.records.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs)?;
    let zdict_bytes = zstd::dict::from_samples(&refs, rocksdb::ZSTD_DICT_BYTES)?;

    let mut rows: Vec<Row> = Vec::new();
    for codec in Codec::ALL {
        let zdict = if codec == Codec::ZstdDict {
            Some(zdict_bytes.as_slice())
        } else {
            None
        };
        rows.push(measure(
            &scratch,
            codec,
            &ds.records,
            &indices,
            zdict,
            &dict,
            scan_reps,
        ));
    }

    // Ledger reference: the same machine-9 records through the standing
    // record-granular store, whose counting `pread` wrapper is the analogue of the
    // `.ekb` central ledger (a real `pread` per read, bytes counted).
    let ledger = {
        let mut store = DictStore::build(&scratch.join("ledger"), &dict, &ds.records, source)
            .map_err(|e| ekbloom::EkbloomError::InvalidArgument {
                what: e.to_string(),
            })?;
        let res = measure_seq(
            source,
            &plan.gets,
            Some(&plan.get_hot),
            VALUE_BYTES as u64,
            |o, l, out| {
                store.read_into(o, l, out);
                (store.last_fetched(), store.last_syscalls())
            },
        );
        (store.footprint(), res)
    };

    // ---- report -------------------------------------------------------------
    println!(
        "kv-integration: {records} records x {VALUE_BYTES} B = {n} B on redb; \
         {gets} skewed gets (hot fraction {measured_hot_fraction:.3}); cache {} B",
        redb_store::CACHE_BYTES
    );
    println!(
        "{:<11} {:>10} {:>7} {:>7} {:>7} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "codec",
        "footprint",
        "F/N",
        "wamp",
        "wrMB/s",
        "cold p50",
        "warm p50",
        "warm p999",
        "cold B/get",
        "scan MiB/s"
    );
    for r in &rows {
        println!(
            "{:<11} {:>10} {:>7.3} {:>7.2} {:>7.1} {:>9} {:>9} {:>9} {:>9.1} {:>9.1}",
            r.name,
            r.footprint_bytes,
            r.footprint_ratio(n),
            r.write_amp(),
            if r.load_ms > 0.0 {
                (r.logical_bytes as f64 / 1e6) / (r.load_ms / 1e3)
            } else {
                0.0
            },
            r.cold.p50,
            r.warm.p50,
            r.warm.p999,
            r.cold_b_per_get(),
            r.scan_best_mib_s
        );
    }

    let raw = find(&rows, "raw");
    let lz4 = find(&rows, "lz4-block");
    let zdict = find(&rows, "zstd-dict");
    let m9 = find(&rows, "ekbloom-m9");

    let verdict = format!(
        "Inside a real embedded KV engine (redb), on skewed 100-byte point gets: raw is fastest \
         ({raw_warm} ns warm p50) and smallest-unit; lz4-block stores {lz4_fp:.3}x raw; zstd-dict \
         stores {zd_fp:.3}x raw and its tail is {zd_p999} ns p99.9; EKBLOOM machine 9 stores \
         {m9_fp:.3}x raw, decodes a get in {m9_warm} ns warm p50 with a {m9_p999} ns p99.9 tail and \
         returns {m9_bpg:.0} B/get from storage (cold). Write amplification: raw {raw_wa:.2}x, lz4 \
         {lz4_wa:.2}x, zstd-dict {zd_wa:.2}x, ekbloom-m9 {m9_wa:.2}x. Range scan: raw {raw_scan:.0} \
         MiB/s, lz4 {lz4_scan:.0}, zstd-dict {zd_scan:.0}, ekbloom-m9 {m9_scan:.0}. Verdict: machine 9 \
         is a legitimate record-granular store (small footprint premium, decode-on-read) but it does \
         NOT beat zstd-dict or LZ4 inside a real B-tree store on footprint, scan throughput, or write \
         amplification; its only edge is the small, self-contained per-value tail, and even that \
         depends on the engine's read path, not on the codec alone.",
        raw_warm = raw.warm.p50,
        lz4_fp = lz4.footprint_ratio(n),
        zd_fp = zdict.footprint_ratio(n),
        zd_p999 = zdict.warm.p999,
        m9_fp = m9.footprint_ratio(n),
        m9_warm = m9.warm.p50,
        m9_p999 = m9.warm.p999,
        m9_bpg = m9.cold_b_per_get(),
        raw_wa = raw.write_amp(),
        lz4_wa = lz4.write_amp(),
        zd_wa = zdict.write_amp(),
        m9_wa = m9.write_amp(),
        raw_scan = raw.scan_best_mib_s,
        lz4_scan = lz4.scan_best_mib_s,
        zd_scan = zdict.scan_best_mib_s,
        m9_scan = m9.scan_best_mib_s,
    );

    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/kv_integration.rs (sh scripts/run-kv-integration.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"engine\": \"redb 4.4.0 (pure Rust, single-file copy-on-write B-tree; no C toolchain; version pinned in Cargo.lock)\","
    );
    let _ = writeln!(
        js,
        "  \"method\": \"real embedded KV store: values are codec outputs stored under increasing u64 ids and decoded byte-exact on read; the shared trained dictionary is stored once under a metadata key and included in the footprint; footprint_bytes is measured after Database::compact() (steady state) and footprint_bytes_as_loaded is the copy-on-write size before compaction; write_amp = process wchar (write-syscall bytes) / logical bytes over the load; storage_bytes_per_get = process rchar delta per get; cold = first pass after reopen, warm = after a warm-up pass; cache_bytes is between the smallest and largest store so residency is a real effect\",",
    );
    let _ = writeln!(js, "  \"records\": {records},");
    let _ = writeln!(js, "  \"value_bytes\": {VALUE_BYTES},");
    let _ = writeln!(js, "  \"raw_bytes\": {n},");
    let _ = writeln!(js, "  \"hot_records\": {},", ds.hot_records);
    let _ = writeln!(js, "  \"hot_read_pct\": {},", rocksdb::HOT_READ_PCT);
    let _ = writeln!(
        js,
        "  \"hot_read_fraction_measured\": {measured_hot_fraction:.4},"
    );
    let _ = writeln!(js, "  \"gets\": {gets},");
    let _ = writeln!(js, "  \"scan_reps\": {scan_reps},");
    let _ = writeln!(js, "  \"seed\": {seed},");
    let _ = writeln!(js, "  \"cache_bytes\": {},", redb_store::CACHE_BYTES);
    let _ = writeln!(js, "  \"zstd_dict_bytes\": {},", zdict_bytes.len());
    let _ = writeln!(js, "  \"machine9_dict_bytes\": {},", dict.physical_bytes());
    let _ = writeln!(js, "  \"contenders\": [");
    for (i, r) in rows.iter().enumerate() {
        let comma = if i + 1 == rows.len() { "" } else { "," };
        emit_row(&mut js, r, n, comma);
    }
    let _ = writeln!(js, "  ],");
    let _ = writeln!(js, "  \"ledger_reference\": {{");
    let _ = writeln!(
        js,
        "    \"name\": \"ekbloom-m9-ledger (record-granular store, counting pread = central-ledger analogue)\","
    );
    let _ = writeln!(js, "    \"footprint_bytes\": {},", ledger.0);
    let _ = writeln!(
        js,
        "    \"bytes_fetched_per_get\": {:.2},",
        ledger.1.bytes_mean
    );
    let _ = writeln!(
        js,
        "    \"syscalls_per_get\": {:.2},",
        ledger.1.syscalls_mean
    );
    let _ = writeln!(js, "    \"get_p50_ns\": {},", ledger.1.lat.p50);
    let _ = writeln!(js, "    \"get_p99_ns\": {},", ledger.1.lat.p99);
    let _ = writeln!(js, "    \"get_p999_ns\": {}", ledger.1.lat.p999);
    let _ = writeln!(js, "  }},");
    let _ = writeln!(js, "  \"verdict\": \"{}\"", json_escape(&verdict));
    js.push_str("}\n");

    std::fs::write(dir.join("kv-integration.json"), js)?;
    let _ = std::fs::remove_dir_all(&scratch);
    println!("wrote evidence/bench/kv-integration.json");
    Ok(())
}
