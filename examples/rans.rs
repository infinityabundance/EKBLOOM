//! Machine 2 (`OrANS-RANS0`) prepared-context decode benchmark.
//!
//! `machine_rans::reveal` historically re-parsed and re-validated the 512-byte
//! model header **per symbol**. This harness measures the fix: a validated,
//! reusable [`RansContext`] (frequencies + prefix sums + a direct 4096-entry
//! symbol table) built **once**, versus rebuilding that context per symbol.
//!
//! Rows:
//!   * `prepared_decode`     — `RANS0.decode` (one context, n steps),
//!   * `per_symbol_reveal`   — `RANS0.reveal` in a loop (one context *per step*),
//!   * `context_build`       — the one-off `RansContext::from_core` cost.
//!
//! Every reconstructed output is checked byte-for-byte against the source. Writes
//! `evidence/bench/rans.json`; run with `sh scripts/run-rans.sh`.

use ekbloom::machine::OrMachine;
use ekbloom::machine_rans::{Rans0, RansContext};
use ekbloom::metrics::now_ns;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// The machine 2 instance under test.
const RANS0: Rans0 = Rans0;
/// Corpus size per shape (~512 KiB): a fair fix-cost/throughput balance.
const CORPUS_LEN: usize = 512 * 1024;
/// Repetitions for each timed row.
const REPS: usize = 3;

/// Deterministic pseudorandom bytes from a small LCG (no new dependencies).
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

/// English-like low-entropy text: a small alphabet with a skewed distribution.
fn skewed(len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b" etaoinshrdlu,.ET\"'\n";
    const WEIGHTS: &[u32] = &[40, 25, 18, 15, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 3, 2, 2];
    let total: u32 = WEIGHTS.iter().sum();
    let mut s: u64 = 0x5EED_1234_ABCD_0001;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut pick = ((s >> 33) as u32) % total;
        for (i, &w) in WEIGHTS.iter().enumerate() {
            if pick < w {
                out.push(ALPHABET[i % ALPHABET.len()]);
                break;
            }
            pick -= w;
        }
    }
    out
}

/// Median-of-reps wall clock (ns) for one closure.
fn bench<F: FnMut()>(reps: usize, mut f: F) -> u64 {
    let mut s = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t0 = now_ns();
        f();
        s.push(now_ns().saturating_sub(t0));
    }
    s.sort_unstable();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

fn gb_per_s(bytes: u64, ns: u64) -> f64 {
    if ns == 0 {
        0.0
    } else {
        bytes as f64 / ns as f64
    }
}

/// One measured corpus shape.
struct Shape {
    name: &'static str,
    source: Vec<u8>,
    core: Vec<u8>,
}

fn measure(shape: &Shape) -> String {
    let n = shape.source.len() as u64;
    let core = &shape.core;

    // Prepared: build the context once and decode with it.
    let prepared_ns = bench(REPS, || {
        let out = RANS0.decode(core, n).unwrap();
        black_box(out);
    });
    let prepared_exact = RANS0.decode(core, n).unwrap() == shape.source;

    // Per-symbol: rebuild the context on every step (the pre-fix behaviour).
    let per_symbol_ns = bench(REPS, || {
        let mut omega = RANS0.init(core).unwrap();
        for _ in 0..n {
            let s = RANS0.reveal(core, &mut omega).unwrap();
            black_box(s);
        }
    });

    // The one-off context construction cost, measured alone.
    let build_ns = bench(REPS * 4, || {
        let ctx = RansContext::from_core(core).unwrap();
        black_box(&ctx);
    });

    let speedup = if prepared_ns == 0 {
        0.0
    } else {
        per_symbol_ns as f64 / prepared_ns as f64
    };
    println!(
        "  {:<8}: N={} V={} V/N={:.3}  prepared {:>6} ns ({:.2} GB/s)  \
         per-symbol {:>9} ns  speedup {:.1}x  exact={}",
        shape.name,
        n,
        core.len(),
        core.len() as f64 / n as f64,
        prepared_ns,
        gb_per_s(n, prepared_ns),
        per_symbol_ns,
        speedup,
        prepared_exact
    );

    let mut row = String::new();
    let _ = writeln!(row, "    {{");
    let _ = writeln!(row, "      \"shape\": \"{}\",", shape.name);
    let _ = writeln!(row, "      \"source_len\": {n},");
    let _ = writeln!(row, "      \"core_len\": {},", core.len());
    let _ = writeln!(
        row,
        "      \"v_over_n\": {:.4},",
        core.len() as f64 / n as f64
    );
    let _ = writeln!(row, "      \"prepared_decode_ns\": {prepared_ns},");
    let _ = writeln!(
        row,
        "      \"prepared_decode_gb_per_s\": {:.3},",
        gb_per_s(n, prepared_ns)
    );
    let _ = writeln!(row, "      \"prepared_exact\": {prepared_exact},");
    let _ = writeln!(row, "      \"per_symbol_reveal_ns\": {per_symbol_ns},");
    let _ = writeln!(
        row,
        "      \"per_symbol_reveal_gb_per_s\": {:.3},",
        gb_per_s(n, per_symbol_ns)
    );
    let _ = writeln!(row, "      \"context_build_ns\": {build_ns},");
    let _ = writeln!(row, "      \"speedup\": {speedup:.3}");
    let _ = write!(row, "    }}");
    row
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    let shapes = [
        Shape {
            name: "skewed",
            source: skewed(CORPUS_LEN),
            core: Vec::new(),
        },
        Shape {
            name: "uniform",
            source: lcg(0xC0FF_EE00_1234_5678, CORPUS_LEN),
            core: Vec::new(),
        },
    ];
    let shapes: Vec<Shape> = shapes
        .into_iter()
        .map(|mut s| {
            s.core = RANS0.encode(&s.source).unwrap();
            s
        })
        .collect();

    println!("machine 2 (OrANS-RANS0) prepared-context decode:");
    let rows: Vec<String> = shapes.iter().map(measure).collect();

    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/rans.rs (sh scripts/run-rans.sh)\","
    );
    let _ = writeln!(js, "  \"machine\": \"OrANS-RANS0 (machine_id=2)\",");
    let _ = writeln!(js, "  \"corpus_len\": {CORPUS_LEN},");
    let _ = writeln!(js, "  \"reps\": {REPS},");
    let _ = writeln!(
        js,
        "  \"note\": \"prepared = one validated RansContext (freq+cum+4096-entry table) reused for the whole decode; per-symbol = the context rebuilt on every reveal step. The pre-fix code parsed the model header (a strict subset of a RansContext rebuild) per step, so the reported speedup is an upper bound.\","
    );
    let _ = writeln!(js, "  \"shapes\": [");
    for (i, row) in rows.iter().enumerate() {
        js.push_str(row);
        if i + 1 == rows.len() {
            js.push('\n');
        } else {
            js.push_str(",\n");
        }
    }
    js.push_str("  ]\n}\n");
    std::fs::write(dir.join("rans.json"), js)?;
    println!("wrote evidence/bench/rans.json");
    Ok(())
}
