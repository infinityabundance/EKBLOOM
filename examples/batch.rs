//! Batched independent reconstruction benchmark for `src/batch.rs`.
//!
//! Reconstructs a large batch of independent machine-1 restart windows
//! (`WINDOWS` x `WINDOW_BYTES` bytes, each with its own exact restart state and its
//! own encoded bytes) with the CPU-threads backend at `1/2/4/8/16` threads, verifies
//! every output byte-for-byte against the authoritative `NL2::decode`, and reports
//! throughput (windows/s, GB/s) and scaling versus single-thread.
//!
//! It also reports the accelerator boundary honestly: `GpuBackend::available()` is
//! `false` and no GPU path is ever selected (the backend is the CPU-threads one for
//! every thread count). See `docs/GPU.md`.
//!
//! Writes `evidence/bench/batch.json`. Run with `sh scripts/run-batch.sh`.

use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

use ekbloom::machine::{OrMachine, NL2};
use ekbloom::{
    accelerator_available, accelerator_device_present, calibrate_clock_ghz, collect_environment,
    independent_window_jobs, now_ns, AcceleratorBackend, BatchBackend, BatchReconstructor, Dist,
    GpuBackend,
};

/// Symbols per independent window.
const WINDOW_BYTES: usize = 256;
/// Number of independent windows in the batch.
const WINDOWS: usize = 100_000;
/// Thread counts measured (the container caps at 8 CPUs; 16 oversubscribes).
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];
/// Repetitions per thread count (median/`Dist` reported).
const REPS: usize = 5;
/// Deterministic corpus seed.
const SEED: u64 = 0xEB10_0000_B47C_0001;

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

/// Bytes per nanosecond is numerically GB/s (see `docs/PREFIX.md`).
fn gb_per_s(bytes: u64, ns: u64) -> f64 {
    if ns == 0 {
        0.0
    } else {
        bytes as f64 / ns as f64
    }
}

fn windows_per_s(windows: usize, ns: u64) -> f64 {
    if ns == 0 {
        0.0
    } else {
        windows as f64 * 1e9 / ns as f64
    }
}

fn cycles_per_byte(bytes: u64, ns: u64, ghz: f64) -> f64 {
    if bytes == 0 || ns == 0 {
        0.0
    } else {
        ghz * ns as f64 / bytes as f64
    }
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

fn main() -> ekbloom::Result<()> {
    let total_bytes = WINDOW_BYTES
        .checked_mul(WINDOWS)
        .expect("window batch size fits usize");

    let source = lcg(SEED, total_bytes);
    let core = NL2.encode(&source)?;
    let decoded = NL2.decode(&core, total_bytes as u64)?;
    assert_eq!(decoded, source, "encode/decode roundtrip mismatch");

    // One self-contained job per window: exact restart state + own encoded bytes.
    let jobs = independent_window_jobs(&core, WINDOW_BYTES)?;
    assert_eq!(jobs.len(), WINDOWS, "unexpected window count");

    let ghz = calibrate_clock_ghz();
    let hash = blake3::hash(&core);
    let env = collect_environment();
    let gpu = GpuBackend;
    let acc_available = accelerator_available();
    let device_present = accelerator_device_present();
    let backend = BatchReconstructor::new(1).backend();

    println!(
        "batch: windows={} window_bytes={} total={} B clock={:.3} GHz",
        WINDOWS, WINDOW_BYTES, total_bytes, ghz
    );
    println!(
        "  backend={} accelerator={} available={} device_present={}",
        backend.name(),
        gpu.name(),
        acc_available,
        device_present
    );

    let mut rows_json = String::new();
    let mut base_gb = 0.0f64;
    for &threads in &THREADS {
        let rec = BatchReconstructor::new(threads);
        assert_eq!(
            rec.backend(),
            BatchBackend::CpuThreads,
            "no accelerator path may be selected"
        );
        let mut out: Vec<Vec<u8>> = vec![Vec::new(); jobs.len()];

        // Byte-exactness against the authoritative decode, once per thread count.
        rec.run(&jobs, &mut out)?;
        let flat: Vec<u8> = out.concat();
        let exact = flat == decoded;
        assert!(
            exact,
            "batch reconstruction not byte-exact at threads={threads}"
        );

        let mut ns = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let t0 = now_ns();
            rec.run(&jobs, &mut out)?;
            black_box(&out);
            ns.push(now_ns().saturating_sub(t0).max(1));
        }
        let dist = Dist::from_samples(&ns, 0xB47C_0000_0000_0001 ^ threads as u64);
        let ns_p50 = dist.p50;
        let gb = gb_per_s(total_bytes as u64, ns_p50 as u64);
        let wps = windows_per_s(WINDOWS, ns_p50 as u64);
        let cyc = cycles_per_byte(total_bytes as u64, ns_p50 as u64, ghz);
        if threads == 1 {
            base_gb = gb;
        }
        let scale = if base_gb > 0.0 { gb / base_gb } else { 0.0 };

        println!(
            "  threads={threads:>2}: {wps:>11.0} windows/s  {gb:>6.3} GB/s  \
             {cyc:>6.3} cyc/B  scale={scale:>5.2}x  exact={exact}"
        );

        let _ = writeln!(rows_json, "    {{");
        let _ = writeln!(rows_json, "      \"threads\": {threads},");
        let _ = writeln!(rows_json, "      \"ns_p50\": {ns_p50:.3},");
        let _ = writeln!(rows_json, "      \"ci95_lo\": {:.3},", dist.ci95_lo);
        let _ = writeln!(rows_json, "      \"ci95_hi\": {:.3},", dist.ci95_hi);
        let _ = writeln!(rows_json, "      \"windows_per_s\": {wps:.3},");
        let _ = writeln!(rows_json, "      \"gb_per_s\": {gb:.6},");
        let _ = writeln!(rows_json, "      \"cycles_per_byte\": {cyc:.6},");
        let _ = writeln!(rows_json, "      \"scaling_vs_t1\": {scale:.4},");
        let _ = writeln!(rows_json, "      \"reps\": {REPS},");
        let _ = writeln!(rows_json, "      \"exact\": {exact},");
        let _ = writeln!(rows_json, "      \"backend\": \"{}\"", backend.name());
        let _ = writeln!(rows_json, "    }},");
    }
    // Drop the trailing comma of the last row so the array is valid JSON.
    let trimmed = rows_json.trim_end();
    let rows_json = trimmed.strip_suffix(',').unwrap_or(trimmed).to_string();

    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/batch.rs (sh scripts/run-batch.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"harness\": \"src/court.rs (Dist, bootstrap 95% CI) + src/batch.rs\","
    );
    let _ = writeln!(js, "  \"machine\": \"OrANS-NL2 (machine_id=1)\",");
    let _ = writeln!(js, "  \"workload\": \"independent restart windows\",");
    let _ = writeln!(js, "  \"window_bytes\": {WINDOW_BYTES},");
    let _ = writeln!(js, "  \"windows\": {WINDOWS},");
    let _ = writeln!(js, "  \"total_bytes\": {total_bytes},");
    let _ = writeln!(js, "  \"corpus_seed\": \"0x{SEED:016X}\",");
    let _ = writeln!(js, "  \"corpus_blake3\": \"{}\",", hash.to_hex());
    let _ = writeln!(js, "  \"thread_counts\": {:?},", THREADS);
    let _ = writeln!(js, "  \"reps\": {REPS},");
    let _ = writeln!(js, "  \"clock_ghz_estimate\": {ghz:.6},");
    let _ = writeln!(js, "  \"backend\": \"{}\",", backend.name());
    let _ = writeln!(js, "  \"accelerator_backend\": \"{}\",", gpu.name());
    let _ = writeln!(js, "  \"accelerator_available\": {acc_available},");
    let _ = writeln!(js, "  \"accelerator_device_present\": {device_present},");
    let _ = writeln!(js, "  \"environment\": {{");
    let _ = writeln!(js, "    \"pinning\": \"{}\",", json_escape(&env.pinning));
    let _ = writeln!(js, "    \"governor\": \"{}\",", json_escape(&env.governor));
    let _ = writeln!(js, "    \"smt\": \"{}\",", json_escape(&env.smt));
    let _ = writeln!(js, "    \"page_size\": \"{}\"", json_escape(&env.page_size));
    let _ = writeln!(js, "  }},");
    let _ = writeln!(js, "  \"rows\": [\n{rows_json}\n  ],");
    let _ = writeln!(
        js,
        "  \"gpu_verdict\": \"not implemented: no accelerator backend exists, GpuBackend::available() is false and no GPU path is taken; the batch API is the boundary such a backend would plug into. A GPU is not the default for single-byte host latency (transfer/scheduling overhead dominates) but is a plausible extension for very large independent batches already resident in device memory.\","
    );
    let _ = writeln!(
        js,
        "  \"note\": \"jobs are independent (each from a known restart state + its own encoded block), so throughput scales with cores; scaling_vs_t1 is the ratio to the single-thread row. Every row is byte-exact vs NL2::decode.\""
    );
    js.push_str("}\n");

    let path = Path::new("evidence/bench/batch.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, js)?;
    println!("wrote evidence/bench/batch.json ({} rows)", THREADS.len());
    Ok(())
}
