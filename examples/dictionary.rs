//! Shared-dictionary footprint and decode-latency study for small records.
//!
//! On a corpus of many small (≈100-byte) text-like key/value records it compares
//! the **total** physical footprint, including the shared dictionary amortised,
//! for:
//!
//! * `(a)` per-record machine 2 (`OrANS-RANS0`) — each core carries its own
//!   516-byte model header;
//! * `(b)` `OrANS-DICT` (machine 9) — one 540-byte shared dictionary plus a
//!   compact, header-less per-record core (20-byte framing + entropy bytes);
//! * `(c)` raw records;
//! * `(d)` Zstandard with a trained dictionary (`zstd::dict::from_samples`);
//! * `(e)` plain per-record Zstandard.
//!
//! It reports bytes/record and totals, the number of records at which the shared
//! dictionary breaks even against per-record machine 2, a record-**size** sweep
//! (shared structure wins for small records, loses once the per-record header
//! amortises), and per-record decode latencies. Every row is asserted byte-exact
//! before timing.
//!
//! Writes `evidence/bench/dictionary.json`. Run with `sh scripts/run-dictionary.sh`.

use ekbloom::metrics::now_ns;
use ekbloom::{prepare, train_dictionary, Dictionary, OrMachine, MACHINE_DICT, RANS0};
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// Records in the primary small-record corpus.
const RECORDS: usize = 4096;
/// Target record size (bytes) for the primary corpus.
const RECORD_BYTES: usize = 100;
/// Record sizes swept to locate the size break-even.
const SIZE_SWEEP: [usize; 7] = [64, 128, 256, 512, 1024, 2048, 4096];
/// Records per swept size.
const SWEEP_RECORDS: usize = 1024;
/// Sizes swept on the *heterogeneous* corpus (each record has its own local
/// alphabet), where a per-record model can overtake the shared one.
const HETERO_SWEEP: [usize; 5] = [256, 1024, 4096, 16384, 65536];
/// Records per heterogeneous swept size.
const HETERO_RECORDS: usize = 256;
/// Zstandard dictionary capacity.
const ZSTD_DICT_BYTES: usize = 110 * 1024;
/// Zstandard level used for every row.
const ZSTD_LEVEL: i32 = 3;

/// A text-like record of exactly `target` bytes drawn from a small token set (a
/// skewed, repetitive alphabet), deterministically seeded by `seed`.
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

fn corpus(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| make_record(i, size)).collect()
}

/// A record whose 4-symbol alphabet is **local** to that record (a different,
/// narrower skew per record), so a shared model is a poor fit and a per-record
/// machine-2 model can win once its header amortises.
fn make_hetero_record(seed: usize, target: usize) -> Vec<u8> {
    let base = ((seed.wrapping_mul(37)) % 200) as u8;
    let mut out = Vec::with_capacity(target);
    let mut k = seed as u64;
    for _ in 0..target {
        k = k
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (k >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(base.wrapping_add(idx));
    }
    out
}

fn hetero_corpus(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| make_hetero_record(i, size)).collect()
}

fn refs(records: &[Vec<u8>]) -> Vec<&[u8]> {
    records.iter().map(|r| r.as_slice()).collect()
}

fn p50(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// The total footprint of one method over the corpus.
struct Footprint {
    total: u64,
    per_record: f64,
}

/// Per-record sizes (bytes) of each method, plus the shared-dictionary size.
struct Sized {
    dict_shared: u64,
    dict_core: Vec<u64>,
    m2_core: Vec<u64>,
    zdict: u64,
    zdict_core: Vec<u64>,
    zstd_core: Vec<u64>,
    raw: Vec<u64>,
}

/// Compute every method's per-record sizes. Asserts byte-exact reconstruction.
fn size_all(records: &[Vec<u8>], dict: &Dictionary) -> ekbloom::Result<Sized> {
    // Machine 9 resolves a *referenced* dictionary id from the registry.
    ekbloom::register_dictionary(dict)?;
    let mut dict_core = Vec::with_capacity(records.len());
    let mut m2_core = Vec::with_capacity(records.len());
    let mut zdict_core = Vec::with_capacity(records.len());
    let mut zstd_core = Vec::with_capacity(records.len());
    let mut raw = Vec::with_capacity(records.len());

    let samples = refs(records);
    let zdict = zstd::dict::from_samples(&samples, ZSTD_DICT_BYTES)?;
    let mut zc = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &zdict)?;
    let mut zd = zstd::bulk::Decompressor::with_dictionary(&zdict)?;

    for rec in records {
        // (a) per-record machine 2.
        let c2 = RANS0.encode(rec)?;
        assert_eq!(
            RANS0.decode(&c2, rec.len() as u64)?,
            *rec,
            "machine-2 exact"
        );
        m2_core.push(c2.len() as u64);

        // (b) dictionary machine 9 (referenced framing + header-less core).
        let core = dict.encode(rec)?;
        assert_eq!(dict.decode(&core)?, *rec, "dictionary exact");
        let framed = ekbloom::frame_dictionary_reference(dict, &core);
        assert_eq!(
            ekbloom::DICT.decode(&framed, rec.len() as u64)?,
            *rec,
            "machine-9 exact"
        );
        dict_core.push(framed.len() as u64);

        // (d) zstd with the trained dictionary.
        let zc_bytes = zc.compress(rec)?;
        assert_eq!(
            zd.decompress(&zc_bytes, rec.len())?,
            *rec,
            "zstd-dict exact"
        );
        zdict_core.push(zc_bytes.len() as u64);

        // (e) plain per-record zstd.
        let zs = zstd::bulk::compress(rec, ZSTD_LEVEL)?;
        assert_eq!(zstd::bulk::decompress(&zs, rec.len())?, *rec, "zstd exact");
        zstd_core.push(zs.len() as u64);

        raw.push(rec.len() as u64);
    }

    Ok(Sized {
        dict_shared: dict.physical_bytes(),
        dict_core,
        m2_core,
        zdict: zdict.len() as u64,
        zdict_core,
        zstd_core,
        raw,
    })
}

impl Sized {
    fn footprint(&self, shared: u64, per: &[u64]) -> Footprint {
        let total = shared + per.iter().sum::<u64>();
        let n = per.len().max(1) as f64;
        Footprint {
            total,
            per_record: total as f64 / n,
        }
    }
}

/// First record count at which `a` (with shared preamble) beats `b` (no preamble),
/// or `None` if it never does within the corpus.
fn break_even(a_shared: u64, a: &[u64], b: &[u64]) -> Option<usize> {
    let mut sa = a_shared;
    let mut sb = 0u64;
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        sa = sa.saturating_add(*x);
        sb = sb.saturating_add(*y);
        if sa < sb {
            return Some(i + 1);
        }
    }
    None
}

fn json_opt(v: Option<usize>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    // ---- primary small-record corpus -----------------------------------------
    let records = corpus(RECORDS, RECORD_BYTES);
    let dict = train_dictionary(&refs(&records))?;

    let sized = size_all(&records, &dict)?;
    let dict_fp = sized.footprint(sized.dict_shared, &sized.dict_core);
    let m2_fp = sized.footprint(0, &sized.m2_core);
    let raw_fp = sized.footprint(0, &sized.raw);
    let zdict_fp = sized.footprint(sized.zdict, &sized.zdict_core);
    let zstd_fp = sized.footprint(0, &sized.zstd_core);

    let avg_record = ratio(sized.raw.iter().sum::<u64>(), RECORDS as u64);
    let be_m2 = break_even(sized.dict_shared, &sized.dict_core, &sized.m2_core);
    let be_zdict = break_even(sized.dict_shared, &sized.dict_core, &sized.zdict_core);

    println!(
        "small records (n={RECORDS}, ~{avg_record:.1} B): dict {:.1} B/rec vs m2 {:.1} B/rec; \
         break-even vs m2 = {be_m2:?} records",
        dict_fp.per_record, m2_fp.per_record
    );

    // ---- decode latency -------------------------------------------------------
    // One prepared machine-9 context is reused across records (the shared-model
    // path); machine 2 rebuilds its per-record model, as its cores each carry one.
    let first_core = dict.encode(&records[0])?;
    let first_framed = ekbloom::frame_dictionary_reference(&dict, &first_core);
    let prepared = prepare(&ekbloom::DICT, &first_framed)?;

    let mut dict_lat = Vec::with_capacity(RECORDS);
    let mut m2_lat = Vec::with_capacity(RECORDS);
    let mut zdict_lat = Vec::with_capacity(RECORDS);
    let mut zstd_lat = Vec::with_capacity(RECORDS);

    let zdict_bytes = zstd::dict::from_samples(&refs(&records), ZSTD_DICT_BYTES)?;
    let mut zd_reuse = zstd::bulk::Decompressor::with_dictionary(&zdict_bytes)?;
    let mut zs_reuse = zstd::bulk::Decompressor::new()?;
    for rec in records.iter() {
        // dictionary machine 9, prepared context reused.
        let core = dict.encode(rec)?;
        let framed = ekbloom::frame_dictionary_reference(&dict, &core);
        // A referenced core is `[dict_id 16][dict_len 0 u32][logical_len u32]
        // [initial_state u32][renorm...]`, so the shared cursor is 28 and only the
        // record's own genesis state changes.
        let mut omega = ekbloom::Snapshot::new(
            u32::from_le_bytes([framed[24], framed[25], framed[26], framed[27]]),
            28,
        );
        let t0 = now_ns();
        let mut out = Vec::with_capacity(rec.len());
        prepared.decode_into(&framed, &mut omega, rec.len() as u64, &mut out)?;
        black_box(&out);
        dict_lat.push(now_ns().saturating_sub(t0));
        assert_eq!(out, *rec, "machine-9 prepared exact");

        // machine 2 per-record decode.
        let c2 = RANS0.encode(rec)?;
        let t0 = now_ns();
        let out = RANS0.decode(&c2, rec.len() as u64)?;
        black_box(&out);
        m2_lat.push(now_ns().saturating_sub(t0));

        // zstd with dictionary (decompressor context reused across records).
        let mut zc = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &zdict_bytes)?;
        let zc_bytes = zc.compress(rec)?;
        let t0 = now_ns();
        let out = zd_reuse.decompress(&zc_bytes, rec.len())?;
        black_box(&out);
        zdict_lat.push(now_ns().saturating_sub(t0));

        // plain zstd (decompressor context reused across records).
        let zs = zstd::bulk::compress(rec, ZSTD_LEVEL)?;
        let t0 = now_ns();
        let out = zs_reuse.decompress(&zs, rec.len())?;
        black_box(&out);
        zstd_lat.push(now_ns().saturating_sub(t0));
    }

    let dict_p50 = p50(dict_lat);
    let m2_p50 = p50(m2_lat);
    let zdict_p50 = p50(zdict_lat);
    let zstd_p50 = p50(zstd_lat);

    // ---- size sweep (where does shared structure stop winning?) --------------
    let mut sweep = String::new();
    for &size in &SIZE_SWEEP {
        let recs = corpus(SWEEP_RECORDS, size);
        let d = train_dictionary(&refs(&recs))?;
        let s = size_all(&recs, &d)?;
        let dfp = s.footprint(s.dict_shared, &s.dict_core);
        let mfp = s.footprint(0, &s.m2_core);
        let zfp = s.footprint(s.zdict, &s.zdict_core);
        let shared_wins = dfp.per_record < mfp.per_record;
        println!(
            "  size={size:>4}: dict {:.1} B/rec  m2 {:.1} B/rec  zstd-dict {:.1} B/rec  \
             dictionary_wins={shared_wins}",
            dfp.per_record, mfp.per_record, zfp.per_record
        );
        let _ = writeln!(
            sweep,
            "    {{ \"record_bytes\": {size}, \"dictionary_bytes_per_record\": {:.3}, \
             \"machine2_bytes_per_record\": {:.3}, \"zstd_dict_bytes_per_record\": {:.3}, \
             \"raw_bytes_per_record\": {:.3}, \"dictionary_wins\": {shared_wins} }},",
            dfp.per_record,
            mfp.per_record,
            zfp.per_record,
            s.raw.iter().sum::<u64>() as f64 / SWEEP_RECORDS as f64,
        );
    }

    let peak_rss = ekbloom::peak_rss_bytes();

    // Heterogeneous sweep: where a per-record model stops losing to the shared one.
    let mut hetero = String::new();
    for &size in &HETERO_SWEEP {
        let recs = hetero_corpus(HETERO_RECORDS, size);
        let d = train_dictionary(&refs(&recs))?;
        let s = size_all(&recs, &d)?;
        let dfp = s.footprint(s.dict_shared, &s.dict_core);
        let mfp = s.footprint(0, &s.m2_core);
        let shared_wins = dfp.per_record < mfp.per_record;
        println!(
            "  hetero size={size:>6}: dict {:.1} B/rec  m2 {:.1} B/rec  dictionary_wins={shared_wins}",
            dfp.per_record, mfp.per_record
        );
        let _ = writeln!(
            hetero,
            "    {{ \"record_bytes\": {size}, \"dictionary_bytes_per_record\": {:.3}, \
             \"machine2_bytes_per_record\": {:.3}, \"dictionary_wins\": {shared_wins} }},",
            dfp.per_record, mfp.per_record
        );
    }

    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/dictionary.rs (sh scripts/run-dictionary.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"machine\": \"OrANS-DICT (machine_id={MACHINE_DICT})\","
    );
    let _ = writeln!(js, "  \"records\": {RECORDS},");
    let _ = writeln!(js, "  \"avg_record_bytes\": {avg_record:.3},");
    let _ = writeln!(js, "  \"dictionary_bytes\": {},", sized.dict_shared);
    let _ = writeln!(js, "  \"dictionary_id\": \"{}\",", hex(&dict.id()));
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(js, "  \"footprint\": [");
    let rows = [
        ("dictionary_machine9", &dict_fp),
        ("machine2_per_record", &m2_fp),
        ("raw_records", &raw_fp),
        ("zstd_dictionary", &zdict_fp),
        ("zstd_per_record", &zstd_fp),
    ];
    for (i, (name, fp)) in rows.iter().enumerate() {
        let comma = if i + 1 == rows.len() { "" } else { "," };
        let _ = writeln!(
            js,
            "    {{ \"method\": \"{name}\", \"total_bytes\": {}, \"bytes_per_record\": {:.3} }}{comma}",
            fp.total, fp.per_record
        );
    }
    let _ = writeln!(js, "  ],");
    let _ = writeln!(js, "  \"break_even\": {{");
    let _ = writeln!(js, "    \"vs_machine2_records\": {},", json_opt(be_m2));
    let _ = writeln!(
        js,
        "    \"vs_zstd_dictionary_records\": {}",
        json_opt(be_zdict)
    );
    let _ = writeln!(js, "  }},");
    let _ = writeln!(js, "  \"latency\": [");
    let latency_rows = [
        ("dictionary_machine9_prepared", dict_p50),
        ("machine2_per_record", m2_p50),
        ("zstd_dictionary", zdict_p50),
        ("zstd_per_record", zstd_p50),
    ];
    for (i, (name, ns)) in latency_rows.iter().enumerate() {
        let comma = if i + 1 == latency_rows.len() { "" } else { "," };
        let _ = writeln!(
            js,
            "    {{ \"method\": \"{name}\", \"decode_p50_ns\": {ns} }}{comma}"
        );
    }
    let _ = writeln!(js, "  ],");
    let sweep = {
        let t = sweep.trim_end();
        t.strip_suffix(',').unwrap_or(t).to_string()
    };
    let hetero = {
        let t = hetero.trim_end();
        t.strip_suffix(',').unwrap_or(t).to_string()
    };
    let _ = writeln!(js, "  \"size_sweep\": [\n{sweep}\n  ],");
    let _ = writeln!(js, "  \"size_sweep_heterogeneous\": [\n{hetero}\n  ],");
    let _ = writeln!(
        js,
        "  \"verdict\": \"shared structure wins on small records: dictionary {:.1} B/rec vs \
         per-record machine 2 {:.1} B/rec (raw {:.1} B/rec); break-even at {} records\"",
        dict_fp.per_record,
        m2_fp.per_record,
        raw_fp.per_record,
        json_opt(be_m2)
    );
    js.push_str("}\n");
    std::fs::write(dir.join("dictionary.json"), js)?;
    println!("wrote evidence/bench/dictionary.json (peak_rss={peak_rss} B)");
    Ok(())
}

fn hex(id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in id.iter() {
        let _ = write!(s, "{b:02x}");
    }
    s
}
