//! Deterministic workload driver for hardware-counter measurement (directive §25).
//!
//! `scripts/container-perf.sh` wraps this binary in `perf stat` so the hardware
//! counters (cycles, instructions, cache misses, branch misses) of a representative
//! read workload are recorded as machine-generated evidence, never fabricated.
//!
//! Usage:
//!
//! ```text
//! perf_target <workload> [iterations] [profile]
//!   workload   seq | random-1b | range-4k
//!   iterations fixed count of the selected operation (default per workload)
//!   profile    path4 (default) | root
//! ```
//!
//! It builds a deterministic in-memory `.ekb` body once (`~1 MiB` LCG source,
//! `path(D=4)` by default or `root`) and runs the selected workload a fixed number
//! of times with a fixed access seed, then prints machine-readable totals on
//! stdout:
//!
//! ```text
//! workload=<name>
//! profile=<label>
//! iterations=<n>
//! logical_bytes=<sum of bytes read across the loop>
//! seconds=<wall seconds of the measured loop>
//! ```
//!
//! The script derives memory bandwidth as `logical_bytes / seconds`. Because the
//! source seed and the access seed are constants, `perf stat` measures the exact
//! same byte movement on every run. The workload loop is sized so the measured
//! phase dominates the one-off body construction (a few percent at most).

use ekbloom::{
    create, read_random_1b, read_ranges, read_sequential, EkbloomBody, ManifestationProfile,
};
use std::hint::black_box;
use std::time::Instant;

/// Deterministic source length (~1 MiB), matching `benches/ekbloom.rs`.
const SRC_LEN: usize = 1024 * 1024;
/// Deterministic LCG source seed.
const SOURCE_SEED: u64 = 0x0EB0_1234_5678_9ABC;
/// Deterministic access seed (same constant as the Divan bench).
const ACCESS_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// Number of precomputed single-byte offsets, cycled once per iteration.
const RANDOM_POOL: usize = 1 << 20;
/// Number of precomputed 4 KiB ranges, cycled once per iteration.
const RANGE_POOL: usize = 1 << 16;
/// Range window for the `range-4k` workload.
const RANGE_WINDOW: u64 = 4096;

/// A deterministic LCG byte source (`u8` from the high bits), as used by the
/// sweep and Divan harnesses.
fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
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

/// A deterministic set of `count` in-range single-byte offsets.
fn random_offsets(count: usize, n: u64) -> Vec<u64> {
    let mut s = ACCESS_SEED;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((s >> 20) % n);
    }
    out
}

/// A deterministic set of `count` 4 KiB ranges spread across the source.
fn random_ranges(count: usize, n: u64) -> Vec<(u64, u64)> {
    let span = n - RANGE_WINDOW;
    let mut s = ACCESS_SEED ^ 0x0FED_CBA9_8765_4321;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(((s >> 20) % (span + 1), RANGE_WINDOW));
    }
    out
}

/// Default iteration counts, chosen so each measured phase runs for a few seconds
/// on the certified path. `root` has no fast path, so its defaults are far
/// smaller (each op costs milliseconds; the goal is a bounded, honest sample).
fn default_iterations(workload: &str, root: bool) -> usize {
    match (workload, root) {
        ("seq", false) => 120_000,
        ("random-1b", false) => 2_000,
        ("range-4k", false) => 256,
        ("seq", true) => 256,
        ("random-1b", true) => 256,
        ("range-4k", true) => 128,
        _ => 1,
    }
}

/// Exit with a usage error on stderr.
fn fail(message: &str) -> ! {
    eprintln!("perf_target: {message}");
    eprintln!("usage: perf_target <seq|random-1b|range-4k> [iterations] [path4|root]");
    std::process::exit(2);
}

fn main() -> ekbloom::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let workload = args.get(1).map(String::as_str).unwrap_or("seq").to_string();
    let profile_arg = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("path4")
        .to_string();
    let root = match profile_arg.as_str() {
        "root" => true,
        "path4" => false,
        other => fail(&format!("unknown profile '{other}'")),
    };
    let iterations = match args.get(2) {
        Some(text) => text
            .parse::<usize>()
            .unwrap_or_else(|_| fail(&format!("invalid iteration count '{text}'"))),
        None => default_iterations(&workload, root),
    };
    if iterations == 0 {
        fail("iterations must be at least 1");
    }

    let source = lcg_bytes(SOURCE_SEED, SRC_LEN);
    let profile = if root {
        ManifestationProfile::Root
    } else {
        ManifestationProfile::Path { depth: 4 }
    };
    let body: EkbloomBody = create(&source, profile)?;
    let n = body.logical_len;

    let mut out: Vec<u8> = Vec::new();
    let mut logical_bytes: u64 = 0;

    let start = Instant::now();
    match workload.as_str() {
        "seq" => {
            for _ in 0..iterations {
                out.clear();
                read_sequential(&body, &mut out)?;
                logical_bytes += out.len() as u64;
                black_box(&out);
            }
        }
        "random-1b" => {
            let offsets = random_offsets(RANDOM_POOL, n);
            out.reserve(offsets.len());
            for _ in 0..iterations {
                out.clear();
                read_random_1b(&body, &offsets, &mut out)?;
                logical_bytes += out.len() as u64;
                black_box(&out);
            }
        }
        "range-4k" => {
            let ranges = random_ranges(RANGE_POOL, n);
            out.reserve(RANGE_POOL * RANGE_WINDOW as usize);
            for _ in 0..iterations {
                out.clear();
                read_ranges(&body, &ranges, &mut out)?;
                logical_bytes += out.len() as u64;
                black_box(&out);
            }
        }
        other => fail(&format!("unknown workload '{other}'")),
    }
    let seconds = start.elapsed().as_secs_f64();

    println!("workload={workload}");
    println!("profile={}", profile.label());
    println!("iterations={iterations}");
    println!("logical_bytes={logical_bytes}");
    println!("seconds={seconds:.9}");
    Ok(())
}
