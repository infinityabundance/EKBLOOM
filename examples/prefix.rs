//! Parallel-prefix reconstruction benchmark for the `OrANS-NL2` encoded core.
//!
//! Measures the strategies:
//!   (a) `scalar_recurrence`   -- whole-core sequential decode,
//!   (b) `blockwise_scalar`    -- block summaries + prefix scan + per-block decode,
//!   (c) `parallel`            -- the same with `threads` in {1,2,4,8,16},
//!   (d) `construction`        -- the block-summary and prefix-scan cost, separately.
//!
//! over block sizes {64, 256, 1024, 4096, 16384}. Every reconstructed output is
//! checked byte-for-byte against the authoritative `NL2::decode` oracle. Cycles
//! per byte are estimated from wall-clock time and a calibrated 1-cycle dependent
//! chain (no `perf` capability is required; `scripts/container-perf.sh` provides
//! the real counters separately). Also measures `state_at_via_prefix` against
//! replay-from-genesis for random positions.
//!
//! Writes `evidence/bench/prefix.json`. Run with `sh scripts/run-prefix.sh`.

use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::metrics::now_ns;
use ekbloom::prefix::{
    block_summary, prefix_scan, reconstruct_blocks, reconstruct_scalar, state_at_via_prefix, Affine,
};
use ekbloom::state::Snapshot;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// Deterministic corpus size (~4 MiB).
const CORPUS_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed.
const CORPUS_SEED: u64 = 0xEB10_0000_9E37_79B9;

/// Block sizes whose construction/throughput trade-off is measured.
const BLOCK_SIZES: [usize; 5] = [64, 256, 1024, 4096, 16384];

/// Thread counts for the parallel strategy.
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];

/// Lengths for the short-input two-pass-vs-scalar comparison.
const SHORT_LENS: [usize; 13] = [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 1024, 4096, 65536];

/// Block size used for the short-input experiment.
const SHORT_BLOCK: usize = 64;

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

/// Small deterministic RNG for query positions.
fn next_rand(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

/// Estimated CPU frequency (Hz) from a calibrated two-op dependent chain, each
/// op ~1 cycle on the target, black-boxed so it cannot be optimised away.
fn estimate_hz() -> f64 {
    let iters: u64 = 100_000_000;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let t0 = now_ns();
    let mut i = 0u64;
    while i < iters {
        x = black_box(x).wrapping_add(1).rotate_left(1);
        i += 1;
    }
    black_box(x);
    let dt = now_ns().saturating_sub(t0).max(1);
    (iters as f64 * 2.0) / (dt as f64 / 1e9)
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

fn p50(mut ns: Vec<u64>) -> u64 {
    ns.sort_unstable();
    ns.get(ns.len() / 2).copied().unwrap_or(0)
}

fn build_summaries(core: &[u8], bs: usize) -> Vec<Affine> {
    let mut v = Vec::with_capacity(core.len().div_ceil(bs));
    let mut lo = 0usize;
    while lo < core.len() {
        let hi = lo.saturating_add(bs).min(core.len());
        v.push(block_summary(&core[lo..hi]));
        lo = hi;
    }
    v
}

fn block_refs(core: &[u8], bs: usize) -> Vec<&[u8]> {
    let mut v = Vec::with_capacity(core.len().div_ceil(bs));
    let mut lo = 0usize;
    while lo < core.len() {
        let hi = lo.saturating_add(bs).min(core.len());
        v.push(&core[lo..hi]);
        lo = hi;
    }
    v
}

fn gb_per_s(bytes: u64, ns: u64) -> f64 {
    if ns == 0 {
        0.0
    } else {
        bytes as f64 / ns as f64
    }
}

fn cycles_per_byte(bytes: u64, ns: u64, hz: f64) -> f64 {
    if bytes == 0 {
        0.0
    } else {
        // ns -> seconds (/1e9), then seconds/byte * cycles/second.
        (ns as f64 / 1e9 / bytes as f64) * hz
    }
}

fn replay_state(core: &[u8], i: u64) -> u16 {
    let mut omega = Snapshot::new(0, 0);
    let mut k = 0u64;
    while k < i {
        match NL2.reveal(core, &mut omega) {
            Ok(_) => {}
            Err(_) => break,
        }
        k += 1;
    }
    omega.state as u16
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    let core = lcg(CORPUS_SEED, CORPUS_LEN);
    let hash = blake3::hash(&core);
    let decoded = NL2.decode(&core, core.len() as u64)?;
    let hz = estimate_hz();
    let bytes = core.len() as u64;

    println!(
        "prefix: corpus_len={} blake3={} est_hz={:.3} GHz",
        core.len(),
        hash.to_hex(),
        hz / 1e9
    );

    // --- (a) scalar recurrence over the whole core ---------------------------
    let scalar_ns = bench(9, || {
        let out = reconstruct_scalar(0, &core);
        black_box(out);
    });
    let scalar_exact = reconstruct_scalar(0, &core) == decoded;
    println!(
        "scalar_recurrence: {:.2} GB/s, {:.3} cyc/B, exact={}",
        gb_per_s(bytes, scalar_ns),
        cycles_per_byte(bytes, scalar_ns, hz),
        scalar_exact
    );

    let mut rows = String::new();
    let mut construction = String::new();

    for &bs in &BLOCK_SIZES {
        let summaries = build_summaries(&core, bs);
        let nb = summaries.len();
        let prefix = prefix_scan(&summaries);
        let starts: Vec<u16> = prefix[..nb].iter().map(|a| a.apply(0)).collect();
        let blocks = block_refs(&core, bs);

        // (d) construction cost, measured separately.
        let build_ns = bench(9, || {
            let s = build_summaries(&core, bs);
            black_box(&s);
        });
        let scan_ns = bench(9, || {
            let p = prefix_scan(&summaries);
            black_box(&p);
        });
        let _ = writeln!(
            construction,
            "    {{ \"block_size\": {bs}, \"blocks\": {nb}, \
             \"summaries_ns\": {build_ns}, \"prefix_scan_ns\": {scan_ns}, \
             \"summaries_ns_per_block\": {:.2}, \"prefix_scan_ns_per_block\": {:.2}, \
             \"summaries_gb_per_s\": {:.3} }},",
            build_ns as f64 / nb as f64,
            scan_ns as f64 / nb as f64,
            gb_per_s(bytes, build_ns)
        );

        // (b) blockwise scalar (threads = 1).
        let blockwise_ns = bench(5, || {
            let out = reconstruct_blocks(&starts, &blocks, 1).unwrap();
            black_box(out);
        });
        let blockwise_exact = reconstruct_blocks(&starts, &blocks, 1).unwrap().concat() == decoded;
        let _ = writeln!(
            rows,
            "    {{ \"block_size\": {bs}, \"strategy\": \"blockwise_scalar\", \"threads\": 1, \
             \"ns\": {blockwise_ns}, \"gb_per_s\": {:.3}, \"cycles_per_byte\": {:.3}, \
             \"ns_per_block\": {:.2}, \"exact\": {blockwise_exact} }},",
            gb_per_s(bytes, blockwise_ns),
            cycles_per_byte(bytes, blockwise_ns, hz),
            blockwise_ns as f64 / nb as f64
        );
        println!(
            "  bs={bs:>5}: blockwise {:.2} GB/s, {:.3} cyc/B, {:.1} ns/blk, exact={}",
            gb_per_s(bytes, blockwise_ns),
            cycles_per_byte(bytes, blockwise_ns, hz),
            blockwise_ns as f64 / nb as f64,
            blockwise_exact
        );

        // (c) parallel block decode.
        for &t in &THREADS {
            let ns = bench(5, || {
                let out = reconstruct_blocks(&starts, &blocks, t).unwrap();
                black_box(out);
            });
            let exact = reconstruct_blocks(&starts, &blocks, t).unwrap().concat() == decoded;
            let _ = writeln!(
                rows,
                "    {{ \"block_size\": {bs}, \"strategy\": \"parallel\", \"threads\": {t}, \
                 \"ns\": {ns}, \"gb_per_s\": {:.3}, \"cycles_per_byte\": {:.3}, \
                 \"ns_per_block\": {:.2}, \"exact\": {exact} }},",
                gb_per_s(bytes, ns),
                cycles_per_byte(bytes, ns, hz),
                ns as f64 / nb as f64
            );
            println!(
                "           threads={t:>2}: {:.2} GB/s, {:.3} cyc/B, exact={}",
                gb_per_s(bytes, ns),
                cycles_per_byte(bytes, ns, hz),
                exact
            );
        }
        assert!(blockwise_exact, "blockwise reconstruction not byte-exact");
    }

    // --- short-input two-pass prefix vs scalar (reviewer's warning) ----------
    let max_short = *SHORT_LENS.last().unwrap_or(&1);
    let short_core = &core[..max_short.min(core.len())];
    let mut short = String::new();
    println!("short input (block={SHORT_BLOCK}):");
    for &l in &SHORT_LENS {
        let c = &short_core[..l];
        let reps = (2_000_000u64 / (l.max(1) as u64)).clamp(5, 20_000) as usize;
        let scalar_ns = bench(reps, || {
            let o = reconstruct_scalar(0, c);
            black_box(o);
        });
        let prefix_ns = bench(reps, || {
            let summaries = build_summaries(c, SHORT_BLOCK);
            let prefix = prefix_scan(&summaries);
            let nb = summaries.len();
            let starts: Vec<u16> = prefix[..nb].iter().map(|a| a.apply(0)).collect();
            let blocks = block_refs(c, SHORT_BLOCK);
            let o = reconstruct_blocks(&starts, &blocks, 1).unwrap();
            black_box(o);
        });
        let ratio = if scalar_ns == 0 {
            0.0
        } else {
            prefix_ns as f64 / scalar_ns as f64
        };
        let _ = writeln!(
            short,
            "    {{ \"len\": {l}, \"scalar_ns\": {scalar_ns}, \"prefix_ns\": {prefix_ns}, \
             \"ratio_prefix_over_scalar\": {ratio:.3} }},",
        );
        println!(
            "  len={l:>6}: scalar={scalar_ns:>9} ns  prefix={prefix_ns:>9} ns  ratio={ratio:.3}"
        );
    }

    // --- state_at_via_prefix: correctness and speed --------------------------
    let scn = 4096usize;
    let sc = lcg(CORPUS_SEED ^ 0x00AB_C0DE, scn);
    let slog = state_log(&sc, scn as u64)?;
    let ss: Vec<Affine> = sc.iter().map(|&b| block_summary(&[b])).collect();
    let sprefix = prefix_scan(&ss);
    let mut sa_mismatches = 0u64;
    for (i, &expect) in slog.iter().enumerate() {
        if u32::from(state_at_via_prefix(&sprefix, 0, 1, i as u64)) != expect {
            sa_mismatches += 1;
        }
    }
    let sa_correct = sa_mismatches == 0;

    // Speed: per-position summaries (stride = 1) over 1 MiB, random positions.
    let qn = 1024 * 1024usize;
    let qcore = lcg(CORPUS_SEED ^ 0x00DE_AD00, qn);
    let qsummaries: Vec<Affine> = qcore.iter().map(|&b| block_summary(&[b])).collect();
    let qprefix = prefix_scan(&qsummaries);
    let samples = 300usize;
    let mut rstate = 0x1234_5678_9ABC_DEF0u64;
    let mut query_ns = Vec::with_capacity(samples);
    let mut replay_ns = Vec::with_capacity(samples);
    for _ in 0..samples {
        let i = next_rand(&mut rstate) % (qn as u64);
        let t0 = now_ns();
        let v = state_at_via_prefix(&qprefix, 0, 1, i);
        black_box(v);
        query_ns.push(now_ns().saturating_sub(t0));

        let t0 = now_ns();
        let v = replay_state(&qcore, i);
        black_box(v);
        replay_ns.push(now_ns().saturating_sub(t0));
    }
    let q_p50 = p50(query_ns);
    let r_p50 = p50(replay_ns);
    let speedup = if q_p50 == 0 {
        0.0
    } else {
        r_p50 as f64 / q_p50 as f64
    };
    println!(
        "state_at_via_prefix: correct={sa_correct} (mismatches={sa_mismatches}); \
         query p50={q_p50} ns vs replay-from-genesis p50={r_p50} ns ({speedup:.1}x)"
    );

    // --- evidence JSON -------------------------------------------------------
    let peak_rss = ekbloom::peak_rss_bytes();
    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/prefix.rs (sh scripts/run-prefix.sh)\","
    );
    let _ = writeln!(js, "  \"machine\": \"OrANS-NL2 (machine_id=1)\",");
    let _ = writeln!(js, "  \"corpus_len\": {},", core.len());
    let _ = writeln!(js, "  \"corpus_seed\": \"0x{CORPUS_SEED:016X}\",");
    let _ = writeln!(js, "  \"corpus_blake3\": \"{}\",", hash.to_hex());
    let _ = writeln!(js, "  \"estimated_hz\": {hz:.6},");
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(js, "  \"block_sizes\": {:?},", BLOCK_SIZES);
    let _ = writeln!(js, "  \"thread_counts\": {:?},", THREADS);
    let _ = writeln!(js, "  \"scalar_recurrence\": {{");
    let _ = writeln!(js, "    \"ns\": {scalar_ns},");
    let _ = writeln!(js, "    \"gb_per_s\": {:.3},", gb_per_s(bytes, scalar_ns));
    let _ = writeln!(
        js,
        "    \"cycles_per_byte\": {:.3},",
        cycles_per_byte(bytes, scalar_ns, hz)
    );
    let _ = writeln!(js, "    \"exact\": {scalar_exact}");
    let _ = writeln!(js, "  }},");
    let _ = writeln!(js, "  \"construction\": [\n{}  ],", construction);
    let _ = writeln!(js, "  \"strategies\": [\n{}  ],", rows);
    let _ = writeln!(js, "  \"short_input\": [\n{}  ],", short);
    let _ = writeln!(js, "  \"state_at_via_prefix\": {{");
    let _ = writeln!(js, "    \"small_n\": {scn},");
    let _ = writeln!(js, "    \"mismatches\": {sa_mismatches},");
    let _ = writeln!(js, "    \"correct\": {sa_correct},");
    let _ = writeln!(js, "    \"query_n\": {qn},");
    let _ = writeln!(js, "    \"samples\": {samples},");
    let _ = writeln!(js, "    \"query_ns_p50\": {q_p50},");
    let _ = writeln!(js, "    \"replay_from_genesis_ns_p50\": {r_p50},");
    let _ = writeln!(js, "    \"speedup\": {speedup:.3}");
    let _ = writeln!(js, "  }},");
    let _ = writeln!(
        js,
        "  \"cycles_per_byte_note\": \"estimated = wall-clock ns/byte x calibrated ~2-op dependent-chain frequency (no perf capability required)\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("prefix.json"), js)?;
    println!("wrote evidence/bench/prefix.json (peak_rss={peak_rss} B)");
    Ok(())
}
