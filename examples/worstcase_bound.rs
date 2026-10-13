//! **Worst-case bound verification** — EKBLOOM's core promise vs rANS.
//!
//! For a body with a **large restart interval**, every read is served from a
//! certified restart and performs a bounded number of positioning steps. The
//! engine *claims* `worst_case_steps_for(request)`; [`Engine::read_counted_steps`]
//! reports the `positioning` steps a read **actually** performs. This example
//! drives the engine over **1,000,000 random positions** (plus a decode-bound
//! rANS pass and a multi-path cold → hot → cold pass) and asserts, for **every**
//! sample:
//!
//! ```text
//! measured_positioning_steps <= worst_case_steps_for(request)
//! ```
//!
//! The decoded bytes are checked byte-for-byte against the source on every
//! sample, so a bound that "holds" by returning wrong bytes is caught. The
//! evidence records the maximum measured steps, the maximum claimed bound, and
//! the violation count — which must be **zero**.
//!
//! Writes `evidence/bench/bound.json`; run by `sh scripts/run-bound.sh`.

#[path = "../tests/common/corpora.rs"]
mod bench;

use ekbloom::{MACHINE_RANS0, MACHINE_STORE};
use std::fmt::Write as _;
use std::path::Path;

/// Deterministic LCG byte source (incompressible base for the step bound).
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

/// One verified pass over a body.
struct Pass {
    name: &'static str,
    machine_id: u32,
    structure: String,
    logical_len: u64,
    per_region_bound: u64,
    lens: Vec<u64>,
    paths: String,
    outcome: bench::BoundOutcome,
}

/// A compact `path=count` view of the chosen plan's region distribution.
fn distribution(engine: &ekbloom::Engine) -> String {
    let mut out = String::new();
    for (path, count) in engine.plan_ref().distribution() {
        if !out.is_empty() {
            out.push_str(" + ");
        }
        let _ = write!(out, "{}={}", path.as_str(), count);
    }
    out
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    // 4 MiB source: with a 1 MiB block restart there are 4 independent restarts.
    let source = lcg(0x0B01_D000_0000_0001, 4 * 1024 * 1024);
    let n = source.len() as u64;

    let mut passes: Vec<Pass> = Vec::new();

    // ---- Pass A: 1,000,000 random positions, a large 1 MiB block restart. ----
    // A verbatim (machine 3) body makes each of the 1,000,000 samples cheap while
    // still reporting the real per-region `block_bytes - 1` positioning count.
    {
        let body = bench::body_with_block(&source, MACHINE_STORE, 1 << 20)?;
        let engine = bench::engine_for(&body)?;
        let per_region = engine.worst_case_steps();
        let outcome = bench::verify_bound(&engine, &body, &source, 1_000_000, &[1], 0xB0_0001)?;
        println!(
            "A block-1MiB/1B: samples={} max_measured={} max_bound={} violations={} mismatches={} paths={}",
            outcome.samples,
            outcome.max_measured_steps,
            outcome.max_bound_steps,
            outcome.violations,
            outcome.mismatches,
            distribution(&engine)
        );
        passes.push(Pass {
            name: "block-1MiB-1byte",
            machine_id: MACHINE_STORE,
            structure: format!("block-restart(block_bytes={})", 1 << 20),
            logical_len: n,
            per_region_bound: per_region,
            lens: vec![1],
            paths: distribution(&engine),
            outcome,
        });
    }

    // ---- Pass B: a decode-bound rANS machine with a large checkpoint interval. ----
    // Machine 2 (order-0 rANS) actually *steps* through the restart skip, so the
    // measured positioning is real decode work — the bound must hold there too.
    {
        let body = bench::body_with_checkpoint(&source, MACHINE_RANS0, 65536)?;
        let engine = bench::engine_for(&body)?;
        let per_region = engine.worst_case_steps();
        let outcome = bench::verify_bound(&engine, &body, &source, 50_000, &[1], 0xB0_0002)?;
        println!(
            "B ckpt-65536/1B: samples={} max_measured={} max_bound={} violations={} mismatches={} paths={}",
            outcome.samples,
            outcome.max_measured_steps,
            outcome.max_bound_steps,
            outcome.violations,
            outcome.mismatches,
            distribution(&engine)
        );
        passes.push(Pass {
            name: "checkpoint-65536-rANS-1byte",
            machine_id: MACHINE_RANS0,
            structure: "checkpoint(interval=65536)".to_string(),
            logical_len: n,
            per_region_bound: per_region,
            lens: vec![1],
            paths: distribution(&engine),
            outcome,
        });
    }

    // ---- Pass C: multi-path cold → hot → cold with large reads. ----
    // A narrow 60 KiB middle band (offset so its end is not block-aligned) is
    // manifested as hot tiles; the surrounding ~4 MiB is a 64 KiB block restart.
    // A 256 KiB request crossing the band pays one bounded restart per
    // dispatched segment (cold → hot → cold), so the request-level bound is up
    // to `2·(B-1) = 131,070` and the measured steps can exceed the per-region
    // `worst_case_steps` (`B-1 = 65,535`) while staying within the bound.
    {
        let band_start = 2u64 << 20;
        let band_len = 60u64 << 10;
        let body = bench::body_with_block_and_tiles(
            &source,
            MACHINE_STORE,
            65536,
            (band_start, band_len),
            12,
        )?;
        let engine = bench::engine_for(&body)?;
        let per_region = engine.worst_case_steps();
        let outcome = bench::verify_bound(&engine, &body, &source, 150_000, &[262_144], 0xB0_0003)?;
        println!(
            "C cold/hot/cold: samples={} max_measured={} max_bound={} violations={} mismatches={} paths={}",
            outcome.samples,
            outcome.max_measured_steps,
            outcome.max_bound_steps,
            outcome.violations,
            outcome.mismatches,
            distribution(&engine)
        );
        passes.push(Pass {
            name: "multi-path-cold-hot-cold",
            machine_id: MACHINE_STORE,
            structure: "block-restart(65536) + hot-tiles(60 KiB band)".to_string(),
            logical_len: n,
            per_region_bound: per_region,
            lens: vec![262_144],
            paths: distribution(&engine),
            outcome,
        });
    }

    let total_samples: u64 = passes.iter().map(|p| p.outcome.samples).sum();
    let total_violations: u64 = passes.iter().map(|p| p.outcome.violations).sum();
    let total_mismatches: u64 = passes.iter().map(|p| p.outcome.mismatches).sum();
    assert_eq!(total_violations, 0, "the worst-case bound was violated");
    assert_eq!(total_mismatches, 0, "a bounded read returned wrong bytes");

    write_json(
        dir,
        n,
        &passes,
        total_samples,
        total_violations,
        total_mismatches,
    )?;
    println!("wrote evidence/bench/bound.json");
    Ok(())
}

fn write_json(
    dir: &Path,
    logical_len: u64,
    passes: &[Pass],
    total_samples: u64,
    total_violations: u64,
    total_mismatches: u64,
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/worstcase_bound.rs (sh scripts/run-bound.sh)\","
    );
    let _ = writeln!(
        s,
        "  \"promise\": \"for every sample, measured positioning steps <= worst_case_steps_for(request); the request-level bound is O(segments), never O(prefix)\","
    );
    let _ = writeln!(s, "  \"logical_len\": {logical_len},");
    let _ = writeln!(s, "  \"total_samples\": {total_samples},");
    let _ = writeln!(s, "  \"total_violations\": {total_violations},");
    let _ = writeln!(s, "  \"total_mismatches\": {total_mismatches},");
    s.push_str("  \"passes\": [\n");
    for (i, p) in passes.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", p.name);
        let _ = writeln!(s, "      \"machine_id\": {},", p.machine_id);
        let _ = writeln!(s, "      \"structure\": \"{}\",", p.structure);
        let _ = writeln!(s, "      \"logical_len\": {},", p.logical_len);
        let _ = writeln!(s, "      \"per_region_bound\": {},", p.per_region_bound);
        let _ = writeln!(s, "      \"read_lengths\": {:?},", p.lens);
        let _ = writeln!(s, "      \"plan_paths\": \"{}\",", p.paths);
        let _ = writeln!(s, "      \"samples\": {},", p.outcome.samples);
        let _ = writeln!(
            s,
            "      \"max_measured_steps\": {},",
            p.outcome.max_measured_steps
        );
        let _ = writeln!(
            s,
            "      \"max_bound_steps\": {},",
            p.outcome.max_bound_steps
        );
        let _ = writeln!(s, "      \"violations\": {},", p.outcome.violations);
        let _ = writeln!(s, "      \"mismatches\": {},", p.outcome.mismatches);
        let _ = writeln!(s, "      \"worst_ratio\": {:.6}", p.outcome.worst_ratio);
        s.push_str(if i + 1 == passes.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ],\n");
    let _ = writeln!(
        s,
        "  \"verdict\": \"every measured positioning count was <= its claimed request-level bound ({total_violations} violations across {total_samples} samples); every decoded range was byte-exact\""
    );
    s.push_str("}\n");
    std::fs::write(dir.join("bound.json"), s)?;
    Ok(())
}
