//! **Standard-corpora court** — EKBLOOM on real standard data, at an equal budget.
//!
//! For each corpus (Silesia, enwik8, Calgary, plus a real text and ELF corpus) the
//! best body the joint optimiser [`OptimizedPlan::optimize`] admits under a
//! budget equal to the raw size, materialised and read through the bounded
//! engine, is compared against the strong conventional direct-access contenders
//! built under the same rules:
//!
//! * `raw-vec` — the source in a `Vec<u8>` (a positional slice),
//! * `lz4` — block-indexed LZ4 with a real offset index,
//! * `zstd-19` / `zstd-3` — chunked Zstandard at two levels,
//! * `pinned-lz4` / `pinned-zstd-19` — the same, retaining their decoded hot
//!   prefix **permanently** and counting those bytes in the footprint.
//!
//! Per corpus and contender the evidence records the retained footprint
//! (`physical_bytes`/`N`), the `byte_at` p50/p99, and the 4 KiB read p50/p99.
//! For EKBLOOM it additionally records — from the real file-backed **encoded
//! window counters** ([`ekbloom::FileBody::fetched_encoded_bytes`] /
//! `fetched_reads`) — the encoded bytes fetched and the `pread` syscalls per
//! 4 KiB read; the compressed codecs' per-read compressed bytes and syscalls are
//! recorded the same way.
//!
//! Corpora are downloaded and extracted **outside the repository** by
//! `scripts/run-corpora.sh`; only the hashes and results are committed. Writes
//! `evidence/bench/corpora.json`.

#[path = "../tests/common/corpora.rs"]
mod bench;
#[path = "../tests/common/e2e.rs"]
mod common;

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use ekbloom::{
    AccessTrace, CourtConfig, FileBody, ManifestationBudget, OptimizedPlan, ReadRequest,
};

/// Random single-byte samples per contender.
const BYTE_SAMPLES: usize = 4096;
/// Random 4 KiB samples per contender.
const READ4K_SAMPLES: usize = 512;
/// Random 4 KiB samples for the file-backed window-counter measurement.
const FILE_SAMPLES: usize = 256;
/// Hot prefix retained by the pinned-hot codecs.
const HOT_BYTES: u64 = 256 * 1024;

fn cap() -> usize {
    std::env::var("EKB_CORPORA_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v >= 4096)
        .unwrap_or(bench::DEFAULT_CAP)
}

/// Deterministic in-bounds offsets.
fn offsets(seed: u64, n: u64, len: u64, count: usize) -> Vec<u64> {
    let span = n.saturating_sub(len).max(1);
    let mut s = seed | 1;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 17) % span
        })
        .collect()
}

/// A single contender's measured row.
#[derive(Clone, Debug)]
struct Row {
    name: String,
    class: &'static str,
    physical_bytes: u64,
    e_v: f64,
    byte_at_p50: u64,
    byte_at_p99: u64,
    r4k_p50: u64,
    r4k_p99: u64,
    fetched_bytes_per_4k: u64,
    syscalls_per_4k: u64,
}

/// EKBLOOM's file-backed window counters per 4 KiB read.
#[derive(Clone, Copy, Debug, Default)]
struct FileCounters {
    fetched_max: u64,
    fetched_mean: f64,
    syscalls_max: u64,
    syscalls_mean: f64,
}

/// One corpus's complete report.
struct CorpusReport {
    name: String,
    kind: String,
    origin: String,
    source_len: u64,
    full_bytes: u64,
    full_blake3: String,
    used_blake3: String,
    budget: u64,
    engine_physical: u64,
    plan_summary: String,
    over_budget: bool,
    file: FileCounters,
    rows: Vec<Row>,
}

/// Measure single-byte latencies for `f`, which returns the byte at an offset.
fn measure_byte_at<F: FnMut(u64) -> u8>(offs: &[u64], mut f: F) -> bench::Lat {
    let mut s = Vec::with_capacity(offs.len());
    for &o in offs {
        let t0 = Instant::now();
        let b = f(o);
        s.push(t0.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        std::hint::black_box(b);
    }
    bench::summarize(s)
}

/// Measure 4 KiB read latencies for `f`, which fills `out`.
fn measure_4k<F: FnMut(u64, &mut Vec<u8>)>(offs: &[u64], mut f: F) -> bench::Lat {
    let mut buf = Vec::with_capacity(4096);
    let mut s = Vec::with_capacity(offs.len());
    for &o in offs {
        buf.clear();
        let t0 = Instant::now();
        f(o, &mut buf);
        s.push(t0.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        std::hint::black_box(&buf);
    }
    bench::summarize(s)
}

/// A trace marking the whole object hot (so the optimiser may manifest widely).
fn whole_trace(n: u64) -> AccessTrace {
    AccessTrace {
        regions: vec![(0, n.max(1))],
        counts: vec![1000],
    }
}

/// Build the best body under `budget`; fall back to the `auto` base + a cold
/// block index if the optimiser admits no plan.
fn build_ekbloom(
    source: &[u8],
    budget: u64,
) -> ekbloom::Result<(ekbloom::EkbloomBody, String, bool)> {
    let n = source.len() as u64;
    let manifest_budget = ManifestationBudget {
        maximum_physical_bytes: budget,
    };
    let cfg = CourtConfig {
        reps: 1,
        interleave: true,
        seed: 0x0C09_0000_0001,
    };
    if let Ok((plan, _)) = OptimizedPlan::optimize(source, &whole_trace(n), &manifest_budget, &cfg)
    {
        let body = plan.build(source)?;
        let over = body.physical_bytes() > budget;
        let summary = format!(
            "optimize: machine {} / region {} B / {} committed B / p99 {:.1} ns",
            plan.machine_id(),
            plan.region_bytes(),
            plan.committed_bytes(),
            plan.expected_p99()
        );
        return Ok((body, summary, over));
    }
    let mut body = ekbloom::create_auto(source, ekbloom::ManifestationProfile::Root)?;
    let block = ekbloom::BlockIndex::build_machine(&body.core, n, 1024, body.machine_id)?;
    body.blocks.push(block);
    body.generation += 1;
    let over = body.physical_bytes() > budget;
    let summary = format!(
        "fallback: machine {} / auto base + 1024-byte block index / {} committed B",
        body.machine_id,
        body.physical_bytes()
    );
    Ok((body, summary, over))
}

#[allow(clippy::too_many_arguments)]
fn measure_corpus(
    name: &str,
    kind: &str,
    origin: &str,
    source: &[u8],
    full_bytes: u64,
    full_blake3: &str,
    used_blake3: &str,
    scratch: &Path,
) -> ekbloom::Result<CorpusReport> {
    let n = source.len() as u64;
    let budget = n;
    let hot = HOT_BYTES.min(n / 2).max(4096);

    // A deterministic hot prefix (for the pinned codecs) and access sets.
    let byte_offs = offsets(0xC0_0001, n, 1, BYTE_SAMPLES);
    let read_offs = offsets(0xC0_0002, n, 4096, READ4K_SAMPLES);
    let file_offs = offsets(0xC0_0003, n, 4096, FILE_SAMPLES);

    // ---- EKBLOOM ----
    let (body, plan_summary, over_budget) = build_ekbloom(source, budget)?;
    let engine = bench::engine_for(&body)?;
    let engine_physical = body.physical_bytes();

    // Byte-exactness spot check before timing.
    for &o in read_offs.iter().take(4) {
        let got = engine.read(
            &body,
            ReadRequest {
                offset: o,
                length: 4096,
            },
        )?;
        assert_eq!(
            got,
            &source[o as usize..(o + 4096) as usize],
            "ekbloom 4k mismatch"
        );
    }
    for &o in byte_offs.iter().take(4) {
        assert_eq!(
            engine.byte_at(&body, o)?,
            source[o as usize],
            "ekbloom byte mismatch"
        );
    }

    let ek_byte = measure_byte_at(&byte_offs, |o| engine.byte_at(&body, o).unwrap_or(0));
    let ek_read = measure_4k(&read_offs, |o, out| {
        out.resize(4096, 0);
        engine
            .read_into(
                &body,
                ReadRequest {
                    offset: o,
                    length: 4096,
                },
                out,
            )
            .expect("read");
        std::hint::black_box(out);
    });

    // ---- EKBLOOM file-backed encoded-window counters ----
    let ekb_path = scratch.join(format!("{name}.ekb"));
    ekbloom::write_body(&ekb_path, &body)?;
    let mut fb = FileBody::open(&ekb_path)?;
    fb.set_bounded_windows(true);
    let mut fetched_max = 0u64;
    let mut fetched_sum = 0u64;
    let mut sys_max = 0u64;
    let mut sys_sum = 0u64;
    for (i, &o) in file_offs.iter().enumerate() {
        let before_b = fb.fetched_encoded_bytes();
        let before_s = fb.fetched_reads();
        let got = fb.read(ReadRequest {
            offset: o,
            length: 4096,
        })?;
        assert_eq!(
            got,
            &source[o as usize..(o + 4096) as usize],
            "file 4k mismatch"
        );
        if i < 2 {
            assert_eq!(
                fb.read(ReadRequest {
                    offset: o,
                    length: 4096
                })?,
                got
            );
        }
        let df = fb.fetched_encoded_bytes() - before_b;
        let ds = fb.fetched_reads() - before_s;
        fetched_max = fetched_max.max(df);
        fetched_sum += df;
        sys_max = sys_max.max(ds);
        sys_sum += ds;
    }
    let cnt = file_offs.len().max(1) as f64;
    let file = FileCounters {
        fetched_max,
        fetched_mean: fetched_sum as f64 / cnt,
        syscalls_max: sys_max,
        syscalls_mean: sys_sum as f64 / cnt,
    };
    let _ = std::fs::remove_file(&ekb_path);

    let mut rows = vec![Row {
        name: "ekbloom".to_string(),
        class: "ekbloom",
        physical_bytes: engine_physical,
        e_v: engine_physical as f64 / n as f64,
        byte_at_p50: ek_byte.p50,
        byte_at_p99: ek_byte.p99,
        r4k_p50: ek_read.p50,
        r4k_p99: ek_read.p99,
        fetched_bytes_per_4k: fetched_max,
        syscalls_per_4k: sys_max,
    }];

    // ---- raw-vec ----
    let raw_byte = measure_byte_at(&byte_offs, |o| source[o as usize]);
    let raw_read = measure_4k(&read_offs, |o, out| {
        out.extend_from_slice(&source[o as usize..(o + 4096) as usize]);
    });
    rows.push(Row {
        name: "raw-vec".to_string(),
        class: "baseline",
        physical_bytes: n,
        e_v: 1.0,
        byte_at_p50: raw_byte.p50,
        byte_at_p99: raw_byte.p99,
        r4k_p50: raw_read.p50,
        r4k_p99: raw_read.p99,
        fetched_bytes_per_4k: 4096,
        syscalls_per_4k: 0,
    });

    // ---- codec stores ----
    let mut lz4 = bench::Store::lz4(&scratch.join("lz4"), source, 0)?;
    rows.push(store_row(
        "lz4", &mut lz4, source, n, &byte_offs, &read_offs,
    ));
    let mut z19 = bench::Store::zstd(&scratch.join("z19"), source, 19, 0)?;
    rows.push(store_row(
        "zstd-19", &mut z19, source, n, &byte_offs, &read_offs,
    ));
    let mut z3 = bench::Store::zstd(&scratch.join("z3"), source, 3, 0)?;
    rows.push(store_row(
        "zstd-3", &mut z3, source, n, &byte_offs, &read_offs,
    ));
    let mut plz4 = bench::Store::lz4(&scratch.join("plz4"), source, hot)?;
    rows.push(store_row(
        "pinned-lz4",
        &mut plz4,
        source,
        n,
        &byte_offs,
        &read_offs,
    ));
    let mut pz19 = bench::Store::zstd(&scratch.join("pz19"), source, 19, hot)?;
    rows.push(store_row(
        "pinned-zstd-19",
        &mut pz19,
        source,
        n,
        &byte_offs,
        &read_offs,
    ));

    Ok(CorpusReport {
        name: name.to_string(),
        kind: kind.to_string(),
        origin: origin.to_string(),
        source_len: n,
        full_bytes,
        full_blake3: full_blake3.to_string(),
        used_blake3: used_blake3.to_string(),
        budget,
        engine_physical,
        plan_summary,
        over_budget,
        file,
        rows,
    })
}

/// Measure one codec store's byte_at and 4 KiB latencies plus its per-4-KiB
/// compressed bytes and `pread` syscalls, after a byte-exactness spot check.
fn store_row(
    label: &str,
    store: &mut bench::Store,
    source: &[u8],
    n: u64,
    byte_offs: &[u64],
    read_offs: &[u64],
) -> Row {
    let physical = store.physical_bytes();
    for &o in read_offs.iter().take(4) {
        assert_eq!(
            store.read(o, 4096),
            &source[o as usize..(o + 4096) as usize],
            "{label} mismatch"
        );
    }
    let s_byte = measure_byte_at(byte_offs, |o| store.byte_at(o));
    let mut last_fetched = 0u64;
    let mut last_sys = 0u64;
    let s_read = measure_4k(read_offs, |o, out| {
        let got = store.read(o, 4096);
        last_fetched = store.last_fetched();
        last_sys = store.last_syscalls();
        out.extend_from_slice(&got);
    });
    Row {
        name: label.to_string(),
        class: "baseline",
        physical_bytes: physical,
        e_v: physical as f64 / n as f64,
        byte_at_p50: s_byte.p50,
        byte_at_p99: s_byte.p99,
        r4k_p50: s_read.p50,
        r4k_p99: s_read.p99,
        fetched_bytes_per_4k: last_fetched,
        syscalls_per_4k: last_sys,
    }
}

/// Read a `sha256sum`-style manifest (`<hex>  <name>`) if present.
fn read_manifest(path: &str) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let hash = it.next()?.to_string();
            let name = it.next()?.trim_start_matches('*').to_string();
            Some((name, hash))
        })
        .collect()
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let cap = cap();
    let scratch = std::env::temp_dir().join(format!("ekbloom-corpora-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;

    // Standard corpora (if downloaded + extracted) plus the container's own text
    // and ELF corpora.
    let corpora_dir = std::env::var("EKB_CORPORA_DIR").unwrap_or_default();
    let standard = if corpora_dir.is_empty() {
        Vec::new()
    } else {
        bench::load_standard(Path::new(&corpora_dir), cap)
    };
    let own = common::load(common::Caps {
        text: cap,
        elf: cap,
        mixed: 0,
    });
    let archives = read_manifest(&std::env::var("EKB_CORPORA_MANIFEST").unwrap_or_default());

    let mut reports: Vec<CorpusReport> = Vec::new();
    for c in &standard {
        println!("== {} ({} B full, cap {} B) ==", c.name, c.full_bytes, cap);
        let r = measure_corpus(
            c.name,
            c.kind,
            &c.origin,
            &c.source,
            c.full_bytes,
            &c.full_blake3,
            &c.used_blake3,
            &scratch,
        )?;
        print_report(&r);
        reports.push(r);
    }
    for c in own.iter() {
        if c.name != "text" && c.name != "elf" {
            continue;
        }
        println!("== {} ({} B) ==", c.name, c.source.len());
        let r = measure_corpus(
            c.name,
            c.kind,
            &c.origin,
            &c.source,
            c.source.len() as u64,
            &common::blake3_hex(&c.source),
            &common::blake3_hex(&c.source),
            &scratch,
        )?;
        print_report(&r);
        reports.push(r);
    }

    let _ = std::fs::remove_dir_all(&scratch);
    write_json(dir, cap, &archives, &reports)?;
    println!(
        "wrote evidence/bench/corpora.json ({} corpora)",
        reports.len()
    );
    Ok(())
}

fn print_report(r: &CorpusReport) {
    println!("  plan: {}", r.plan_summary);
    println!(
        "  ekbloom file 4k: fetched_max={} (mean {:.0}) syscalls_max={}",
        r.file.fetched_max, r.file.fetched_mean, r.file.syscalls_max
    );
    for row in &r.rows {
        println!(
            "  {:<16} E_V={:.3} byte_at p50={:>6} p99={:>7} ns  4k p50={:>6} p99={:>7} ns",
            row.name, row.e_v, row.byte_at_p50, row.byte_at_p99, row.r4k_p50, row.r4k_p99
        );
    }
}

fn write_json(
    dir: &Path,
    cap: usize,
    archives: &[(String, String)],
    reports: &[CorpusReport],
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/corpora.rs (sh scripts/run-corpora.sh)\","
    );
    let _ = writeln!(
        s,
        "  \"method\": \"per corpus: best body from OptimizedPlan::optimize under a budget equal to the raw size, read through the bounded engine, vs block-indexed LZ4, chunked zstd-19/zstd-3, raw Vec, and pinned-hot LZ4/zstd-19 retaining a 256 KiB decoded prefix; byte_at and 4 KiB latencies are nearest-rank percentiles of deterministic random samples; EKBLOOM window counters are the real FileBody fetched_bytes/fetched_reads deltas\","
    );
    let _ = writeln!(s, "  \"corpus_cap_bytes\": {cap},");
    let _ = writeln!(s, "  \"hot_bytes_pinned\": {HOT_BYTES},");
    let _ = writeln!(s, "  \"samples\": {{\"byte_at\": {BYTE_SAMPLES}, \"read4k\": {READ4K_SAMPLES}, \"file4k\": {FILE_SAMPLES}}},");
    s.push_str("  \"archives\": [");
    for (i, (name, hash)) in archives.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{{\"name\": \"{}\", \"sha256\": \"{}\"}}", name, hash);
    }
    s.push_str("],\n");
    s.push_str("  \"corpora\": [\n");
    for (i, r) in reports.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", r.name);
        let _ = writeln!(s, "      \"kind\": \"{}\",", r.kind);
        let _ = writeln!(s, "      \"origin\": \"{}\",", json_escape(&r.origin));
        let _ = writeln!(s, "      \"source_len\": {},", r.source_len);
        let _ = writeln!(s, "      \"full_bytes\": {},", r.full_bytes);
        let _ = writeln!(s, "      \"full_blake3\": \"{}\",", r.full_blake3);
        let _ = writeln!(s, "      \"used_blake3\": \"{}\",", r.used_blake3);
        let _ = writeln!(s, "      \"budget_bytes\": {},", r.budget);
        let _ = writeln!(s, "      \"engine_physical_bytes\": {},", r.engine_physical);
        let _ = writeln!(s, "      \"over_budget\": {},", r.over_budget);
        let _ = writeln!(s, "      \"plan\": \"{}\",", json_escape(&r.plan_summary));
        let _ = writeln!(
            s,
            "      \"file_4k\": {{\"fetched_bytes_max\": {}, \"fetched_bytes_mean\": {:.1}, \"syscalls_max\": {}, \"syscalls_mean\": {:.2}}},",
            r.file.fetched_max, r.file.fetched_mean, r.file.syscalls_max, r.file.syscalls_mean
        );
        s.push_str("      \"rows\": [\n");
        for (j, row) in r.rows.iter().enumerate() {
            let _ = write!(
                s,
                "        {{\"name\": \"{}\", \"class\": \"{}\", \"physical_bytes\": {}, \"e_v\": {:.4}, \"byte_at_p50_ns\": {}, \"byte_at_p99_ns\": {}, \"r4k_p50_ns\": {}, \"r4k_p99_ns\": {}, \"fetched_bytes_per_4k\": {}, \"syscalls_per_4k\": {}}}",
                row.name,
                row.class,
                row.physical_bytes,
                row.e_v,
                row.byte_at_p50,
                row.byte_at_p99,
                row.r4k_p50,
                row.r4k_p99,
                row.fetched_bytes_per_4k,
                row.syscalls_per_4k
            );
            s.push_str(if j + 1 == r.rows.len() { "\n" } else { ",\n" });
        }
        s.push_str("      ]\n");
        s.push_str(if i + 1 == reports.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ],\n");
    let _ = writeln!(
        s,
        "  \"note\": \"pinned-* rows with byte_at/4k equal to their unpinned counterpart are serving the pinned hot prefix; the honest comparison is footprint vs latency, not a single score\""
    );
    s.push_str("}\n");
    std::fs::write(dir.join("corpora.json"), s)?;
    Ok(())
}

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
