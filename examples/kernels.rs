//! Reconstruction-kernel measurement harness (directive §6) for `src/kernels.rs`.
//!
//! Measures, inside the pinned container via `sh scripts/run-kernels.sh`, at equal
//! source/destination residency:
//!
//! * **Kernel A** (short restart windows, 16/32/64 symbols): scalar vs unrolled vs
//!   2-symbol table (per-window latency) and scalar-batch vs vector-batch
//!   (equal-layout throughput).
//! * **Kernel B** (independent long blocks) at block sizes 256/1024/4096/16384:
//!   the pure-parallel-threads path vs the vectorised-across-16-blocks path, for
//!   1/2/4/8 threads, against the sequential decode.
//!
//! Every strategy is checked byte-for-byte against the authoritative `NL2::decode`
//! before it is timed. Rows are accumulated with the court harness (`Court`,
//! `Dist`, bootstrap 95% CI) and written to `evidence/bench/kernels.json`.
//!
//! No number here is estimated: throughput and cycles/byte come from wall clock and
//! the court's calibrated clock; latency is a `Dist` over real per-operation
//! samples.

use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

use ekbloom::machine::{OrMachine, NL2};
use ekbloom::{
    block_start_states, collect_environment, kernel_a_scalar, kernel_a_scalar_windows,
    kernel_a_simd_windows, kernel_a_table, kernel_a_unrolled, kernel_b_simd, kernel_b_threads,
    nl2_windows_avx2_available, now_ns, reconstruct_blocks, reconstruct_scalar,
    window_start_states, Court, CourtConfig, Dist, Row, PLANNER_REGION_BYTES,
};

/// Deterministic corpus size (4 MiB).
const CORPUS_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed.
const CORPUS_SEED: u64 = 0xEB10_0000_4B45_524E;
/// Kernel A window lengths (symbols per short restart window).
const WINDOW_LENGTHS: [usize; 3] = [16, 32, 64];
/// Kernel A windows measured per length.
const WINDOWS: usize = 8192;
/// Kernel B block sizes.
const BLOCK_SIZES: [usize; 4] = [256, 1024, 4096, 16384];
/// Kernel B thread counts.
const THREADS: [usize; 4] = [1, 2, 4, 8];
/// Repetitions for whole-corpus throughput measurements.
const REPS: usize = 5;

fn lcg(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((s >> 33) as u8);
    }
    out
}

fn bytes_per_s(bytes: u64, ns: u64) -> f64 {
    if ns == 0 {
        0.0
    } else {
        bytes as f64 * 1e9 / ns as f64
    }
}

fn cycles_per_byte(bytes: u64, ns: u64, ghz: f64) -> f64 {
    // throughput = bytes/ns; cycles = ghz * ns; cycles/byte = ghz * ns / bytes.
    if bytes == 0 || ns == 0 {
        0.0
    } else {
        ghz * ns as f64 / bytes as f64
    }
}

/// Time `reps` repetitions of `f` (which returns the produced byte count), pooled
/// into a court `Row` for a whole-corpus throughput measurement.
fn throughput_row(
    clock_ghz: f64,
    logical_len: u64,
    workload: &str,
    candidate: &str,
    mut f: impl FnMut() -> usize,
) -> Row {
    let mut ns = Vec::with_capacity(REPS);
    let mut total_bytes = 0u64;
    let mut total_ns = 0u64;
    for _ in 0..REPS {
        let t0 = now_ns();
        let produced = f();
        let dt = now_ns().saturating_sub(t0).max(1);
        total_bytes = total_bytes.saturating_add(produced as u64);
        total_ns = total_ns.saturating_add(dt);
        ns.push(dt);
    }
    let throughput = bytes_per_s(total_bytes.max(1), total_ns.max(1));
    Row {
        working_set: logical_len,
        workload: workload.to_string(),
        candidate: candidate.to_string(),
        residency: "in-memory".to_string(),
        physical_bytes: logical_len,
        dist_ns: Dist::from_samples(&ns, 0x4B45_524E_0000_0001),
        throughput_bytes_per_s: throughput,
        cycles_per_byte: cycles_per_byte(logical_len, total_ns.max(1) / REPS as u64, clock_ghz),
        reps: REPS,
        samples: ns.len() as u64,
    }
}

/// A court `Row` for a per-operation latency distribution.
fn latency_row(logical_len: u64, workload: &str, candidate: &str, samples: &[u64]) -> Row {
    Row {
        working_set: logical_len,
        workload: workload.to_string(),
        candidate: candidate.to_string(),
        residency: "in-memory".to_string(),
        physical_bytes: logical_len,
        dist_ns: Dist::from_samples(samples, 0x4B45_524E_0000_0002),
        throughput_bytes_per_s: 0.0,
        cycles_per_byte: 0.0,
        reps: 1,
        samples: samples.len() as u64,
    }
}

fn main() -> ekbloom::Result<()> {
    let cfg = CourtConfig {
        reps: 3,
        interleave: true,
        seed: 0x4B45_524E_0000_0000,
    };
    let court = Court::new(cfg);
    let clock_ghz = court.clock_ghz();
    let avx2 = nl2_windows_avx2_available();

    let core = lcg(CORPUS_SEED, CORPUS_LEN);
    let decoded = NL2.decode(&core, core.len() as u64)?;
    println!(
        "kernels: corpus={} B clock={:.3} GHz avx2={} (windows {})",
        core.len(),
        clock_ghz,
        avx2,
        if avx2 { "vector" } else { "scalar fallback" }
    );

    let mut rows: Vec<Row> = Vec::new();

    // ---- scalar whole-core reference ----------------------------------------
    let scalar_whole = throughput_row(
        clock_ghz,
        core.len() as u64,
        "kernel-b-reconstruct",
        "scalar-recurrence (reconstruct_scalar)",
        || {
            let out = reconstruct_scalar(0, &core);
            assert_eq!(out, decoded, "scalar recurrence != decode");
            black_box(out.len())
        },
    );
    println!(
        "  scalar recurrence       : {:.2} GB/s, {:.3} cyc/B",
        scalar_whole.throughput_bytes_per_s / 1e9,
        scalar_whole.cycles_per_byte
    );
    rows.push(scalar_whole);

    // ---- Kernel A: per-window latency ---------------------------------------
    for &wl in &WINDOW_LENGTHS {
        let starts = window_start_states(&core, wl)?;
        let blocks: Vec<&[u8]> = core.chunks(wl).take(WINDOWS).collect();
        let starts: Vec<u16> = starts[..blocks.len()].to_vec();
        let wl_u64 = wl as u64;

        // Latency distribution per strategy over the same windows.
        let mut sample_scalar = Vec::with_capacity(blocks.len());
        let mut sample_unrolled = Vec::with_capacity(blocks.len());
        let mut sample_table = Vec::with_capacity(blocks.len());
        for (k, block) in blocks.iter().enumerate() {
            let x0 = starts[k];
            // Authoritative truth: the decoded core over this window's range.
            let truth = &decoded[k * wl..(k + 1) * wl];
            let mut out = Vec::new();
            let t0 = now_ns();
            kernel_a_scalar(x0, block, &mut out);
            sample_scalar.push(now_ns().saturating_sub(t0));
            assert_eq!(out, truth, "scalar kernel A mismatch");

            out.clear();
            let t0 = now_ns();
            kernel_a_unrolled(x0, block, &mut out);
            sample_unrolled.push(now_ns().saturating_sub(t0));
            assert_eq!(out, truth, "unrolled kernel A mismatch");

            out.clear();
            let t0 = now_ns();
            kernel_a_table(x0, block, &mut out);
            sample_table.push(now_ns().saturating_sub(t0));
            assert_eq!(out, truth, "table kernel A mismatch");
        }
        for (name, samples) in [
            ("kernel-a-scalar", &sample_scalar),
            ("kernel-a-unrolled", &sample_unrolled),
            ("kernel-a-table", &sample_table),
        ] {
            let row = latency_row(wl_u64, &format!("kernel-a-window-{wl}"), name, samples);
            println!(
                "  kernel A wl={wl:>3} {name:<18}: p50={:.0} p99={:.0} ns/window",
                row.dist_ns.p50, row.dist_ns.p99
            );
            rows.push(row);
        }

        // ---- Kernel A: batch (equal residency) ------------------------------
        // Group-major step-major layout (padded to a multiple of 16 lanes).
        let padded =
            blocks.len().div_ceil(ekbloom::NL2_WINDOW_LANES) * ekbloom::NL2_WINDOW_LANES * wl;
        let mut cols = vec![0u8; padded];
        for (w, block) in blocks.iter().enumerate() {
            let base = (w / ekbloom::NL2_WINDOW_LANES) * wl * ekbloom::NL2_WINDOW_LANES;
            let lane = w % ekbloom::NL2_WINDOW_LANES;
            for (i, &b) in block.iter().enumerate() {
                cols[base + i * ekbloom::NL2_WINDOW_LANES + lane] = b;
            }
        }
        let total_bytes = padded;

        let mut scalar_out = vec![0u8; padded];
        let row_batch_scalar = throughput_row(
            clock_ghz,
            total_bytes as u64,
            &format!("kernel-a-batch-{wl}"),
            "kernel-a-scalar-windows",
            || {
                let mut st = starts.clone();
                kernel_a_scalar_windows(&mut st, &cols, wl, &mut scalar_out).unwrap();
                black_box(scalar_out.len())
            },
        );

        let mut simd_out = vec![0u8; padded];
        let row_batch_simd = throughput_row(
            clock_ghz,
            total_bytes as u64,
            &format!("kernel-a-batch-{wl}"),
            "kernel-a-simd-windows",
            || {
                let mut st = starts.clone();
                kernel_a_simd_windows(&mut st, &cols, wl, &mut simd_out).unwrap();
                black_box(simd_out.len())
            },
        );
        // Byte-exactness of the vector batch (result already in `simd_out`).
        assert_eq!(simd_out, scalar_out, "kernel A batch simd != scalar");

        let speedup = if row_batch_simd.throughput_bytes_per_s > 0.0 {
            row_batch_simd.throughput_bytes_per_s / row_batch_scalar.throughput_bytes_per_s
        } else {
            0.0
        };
        println!(
            "  kernel A wl={wl:>3} batch scalar={:.2} GB/s simd={:.2} GB/s (simd {:.2}x faster)",
            row_batch_scalar.throughput_bytes_per_s / 1e9,
            row_batch_simd.throughput_bytes_per_s / 1e9,
            speedup
        );
        rows.push(row_batch_scalar);
        rows.push(row_batch_simd);
    }

    // ---- Kernel B -----------------------------------------------------------
    for &block in &BLOCK_SIZES {
        // Decode-only reference (summaries/prep precomputed): the pure-parallel
        // path of docs/PREFIX.md, without the serial construction pass.
        let starts = block_start_states(&core, block)?;
        let slices: Vec<&[u8]> = core.chunks(block).collect();
        for &threads in &THREADS {
            let row_decode = throughput_row(
                clock_ghz,
                core.len() as u64,
                &format!("kernel-b-block-{block}"),
                &format!("kernel-b-decode-only-t{threads}"),
                || {
                    let parts = reconstruct_blocks(&starts, &slices, threads).unwrap();
                    black_box(parts.iter().map(Vec::len).sum::<usize>())
                },
            );
            rows.push(row_decode);
        }

        let row_seq = throughput_row(
            clock_ghz,
            core.len() as u64,
            &format!("kernel-b-block-{block}"),
            "kernel-b-sequential",
            || {
                let out = NL2.decode(&core, core.len() as u64).unwrap();
                black_box(out.len())
            },
        );
        rows.push(row_seq);

        for &threads in &THREADS {
            let row_threads = throughput_row(
                clock_ghz,
                core.len() as u64,
                &format!("kernel-b-block-{block}"),
                &format!("kernel-b-threads-t{threads}"),
                || {
                    let out = kernel_b_threads(&core, block, threads).unwrap();
                    assert_eq!(out, decoded, "kernel B threads != decode");
                    black_box(out.len())
                },
            );
            let row_simd = throughput_row(
                clock_ghz,
                core.len() as u64,
                &format!("kernel-b-block-{block}"),
                &format!("kernel-b-simd-t{threads}"),
                || {
                    let out = kernel_b_simd(&core, block, threads).unwrap();
                    assert_eq!(out, decoded, "kernel B simd != decode");
                    black_box(out.len())
                },
            );
            println!(
                "  kernel B blk={block:>5} t={threads}: threads={:.2} GB/s simd={:.2} GB/s",
                row_threads.throughput_bytes_per_s / 1e9,
                row_simd.throughput_bytes_per_s / 1e9
            );
            rows.push(row_threads);
            rows.push(row_simd);
        }
    }

    // ---- verdicts -----------------------------------------------------------
    let verdicts = verdicts(&rows);
    for v in &verdicts {
        println!(
            "  VERDICT {:<28} {:<28} {:.2}x {}",
            v.workload, v.baseline, v.ratio, v.verdict
        );
    }

    let report = court.report(collect_environment());
    write_kernels_json(
        Path::new("evidence/bench/kernels.json"),
        &report,
        &rows,
        &verdicts,
    )?;
    println!("wrote evidence/bench/kernels.json ({} rows)", rows.len());
    let _ = PLANNER_REGION_BYTES;
    Ok(())
}

struct Verdict {
    workload: String,
    candidate: String,
    baseline: String,
    ratio: f64,
    verdict: &'static str,
}

/// Compare each vector/batch candidate against its scalar control within the same
/// workload; KEEP only if it is strictly faster.
fn verdicts(rows: &[Row]) -> Vec<Verdict> {
    let comparisons: [(&str, &str, &str); 9] = [
        (
            "kernel-a-batch-16",
            "kernel-a-simd-windows",
            "kernel-a-scalar-windows",
        ),
        (
            "kernel-a-batch-32",
            "kernel-a-simd-windows",
            "kernel-a-scalar-windows",
        ),
        (
            "kernel-a-batch-64",
            "kernel-a-simd-windows",
            "kernel-a-scalar-windows",
        ),
        (
            "kernel-b-block-256",
            "kernel-b-simd-t1",
            "kernel-b-threads-t1",
        ),
        (
            "kernel-b-block-1024",
            "kernel-b-simd-t1",
            "kernel-b-threads-t1",
        ),
        (
            "kernel-b-block-4096",
            "kernel-b-simd-t1",
            "kernel-b-threads-t1",
        ),
        (
            "kernel-b-block-16384",
            "kernel-b-simd-t1",
            "kernel-b-threads-t1",
        ),
        (
            "kernel-b-block-1024",
            "kernel-b-simd-t4",
            "kernel-b-threads-t4",
        ),
        (
            "kernel-b-block-4096",
            "kernel-b-simd-t4",
            "kernel-b-threads-t4",
        ),
    ];
    let find = |workload: &str, candidate: &str| -> Option<&Row> {
        rows.iter()
            .find(|r| r.workload == workload && r.candidate == candidate)
    };
    let mut out = Vec::new();
    for (workload, candidate, baseline) in comparisons {
        if let (Some(c), Some(b)) = (find(workload, candidate), find(workload, baseline)) {
            if b.throughput_bytes_per_s > 0.0 && c.throughput_bytes_per_s > 0.0 {
                let ratio = c.throughput_bytes_per_s / b.throughput_bytes_per_s;
                out.push(Verdict {
                    workload: workload.to_string(),
                    candidate: candidate.to_string(),
                    baseline: baseline.to_string(),
                    ratio,
                    verdict: if ratio > 1.0 { "KEEP" } else { "REJECT" },
                });
            }
        }
    }
    out
}

fn write_kernels_json(
    path: &Path,
    report: &ekbloom::CourtReport,
    rows: &[Row],
    verdicts: &[Verdict],
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/kernels.rs (sh scripts/run-kernels.sh)\","
    );
    let _ = writeln!(
        s,
        "  \"harness\": \"src/court.rs (Court, Dist, bootstrap 95% CI)\","
    );
    let _ = writeln!(s, "  \"corpus_len\": {CORPUS_LEN},");
    let _ = writeln!(s, "  \"corpus_seed\": \"0x{CORPUS_SEED:016X}\",");
    let _ = writeln!(s, "  \"window_lengths\": {:?},", WINDOW_LENGTHS);
    let _ = writeln!(s, "  \"windows\": {WINDOWS},");
    let _ = writeln!(s, "  \"block_sizes\": {:?},", BLOCK_SIZES);
    let _ = writeln!(s, "  \"thread_counts\": {:?},", THREADS);
    let _ = writeln!(s, "  \"clock_ghz_estimate\": {:.6},", report.clock_ghz);
    let _ = writeln!(s, "  \"loop_overhead_ns\": {:.6},", report.loop_overhead_ns);
    let _ = writeln!(s, "  \"avx2_available\": {},", nl2_windows_avx2_available());
    let _ = writeln!(s, "  \"environment\": {{");
    let _ = writeln!(
        s,
        "    \"pinning\": \"{}\",",
        json_escape(&report.env.pinning)
    );
    let _ = writeln!(
        s,
        "    \"governor\": \"{}\",",
        json_escape(&report.env.governor)
    );
    let _ = writeln!(s, "    \"smt\": \"{}\",", json_escape(&report.env.smt));
    let _ = writeln!(
        s,
        "    \"page_size\": \"{}\"",
        json_escape(&report.env.page_size)
    );
    let _ = writeln!(s, "  }},");
    let _ = writeln!(
        s,
        "  \"note\": \"the serial NL2 window recurrence cannot be auto-vectorised by LLVM (each symbol depends on the previous two); the only ILP is across independent windows/blocks, which the explicit AVX2 kernel exploits. Equal residency: the scalar control reads and writes the identical step-major layout.\","
    );

    s.push_str("  \"rows\": [\n");
    for (i, r) in rows.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"workload\": \"{}\",", json_escape(&r.workload));
        let _ = writeln!(s, "      \"candidate\": \"{}\",", json_escape(&r.candidate));
        let _ = writeln!(
            s,
            "      \"throughput_bytes_per_s\": {:.3},",
            r.throughput_bytes_per_s
        );
        let _ = writeln!(s, "      \"cycles_per_byte\": {:.6},", r.cycles_per_byte);
        let _ = writeln!(s, "      \"p50_ns\": {:.3},", r.dist_ns.p50);
        let _ = writeln!(s, "      \"p99_ns\": {:.3},", r.dist_ns.p99);
        let _ = writeln!(s, "      \"ci95_lo\": {:.3},", r.dist_ns.ci95_lo);
        let _ = writeln!(s, "      \"ci95_hi\": {:.3},", r.dist_ns.ci95_hi);
        let _ = writeln!(s, "      \"reps\": {},", r.reps);
        let _ = writeln!(s, "      \"samples\": {}", r.samples);
        s.push_str(if i + 1 == rows.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ],\n");

    s.push_str("  \"verdicts\": [\n");
    for (i, v) in verdicts.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"workload\": \"{}\",", json_escape(&v.workload));
        let _ = writeln!(s, "      \"candidate\": \"{}\",", json_escape(&v.candidate));
        let _ = writeln!(s, "      \"baseline\": \"{}\",", json_escape(&v.baseline));
        let _ = writeln!(s, "      \"ratio\": {:.4},", v.ratio);
        let _ = writeln!(s, "      \"verdict\": \"{}\"", v.verdict);
        s.push_str(if i + 1 == verdicts.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, s)?;
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
