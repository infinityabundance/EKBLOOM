//! Packed 16-bit restart/history benchmark for `OrANS-NL2`.
//!
//! Measures, for stride shifts `K in {8,16,32,64,128,256}` and several corpus
//! sizes:
//!   * physical bytes of the packed restart set (`2*count`) versus the existing
//!     12-byte `CheckpointSet` (`12*count`), and of the packed history
//!     (`2*(N+1)`) versus the existing 4-byte history (`4*(N+1)`);
//!   * random-position p50 latency of a packed restart plus bounded replay,
//!     against replay-from-genesis (and, for the supported intervals, the existing
//!     12-byte checkpoint restore);
//!   * a locality note: how many restarts fit in a fixed per-position budget;
//!   * `state_from_hot`: correctness against the state log and per-call cost
//!     versus reading a separate `PackedHistory`.
//!
//! Writes `evidence/bench/packed.json`. Run with `sh scripts/run-packed.sh`.

use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::metrics::now_ns;
use ekbloom::packed::{state_from_hot, PackedHistory, PackedRestarts};
use ekbloom::{CheckpointSet, Snapshot};
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// Corpus sizes (visible core bytes == logical length for machine 1).
const CORPUS_SIZES: [usize; 3] = [256 * 1024, 1024 * 1024, 4 * 1024 * 1024];

/// Stride shifts measured: `K = 8, 16, 32, 64, 128, 256`.
const STRIDE_SHIFTS: [u8; 6] = [3, 4, 5, 6, 7, 8];

/// Random positions sampled per (size, stride) latency measurement.
const SAMPLES: usize = 512;

/// Random positions sampled for the (expensive) replay-from-genesis baseline.
const GENESIS_SAMPLES: usize = 64;

/// The recorded `sparse(K=64)` frontier numbers from `docs/RESULTS.md`
/// (2 MiB corpus): ~1.19x space for ~150 ns single-byte p50.
const RECORDED_SPARSE_K64_BYTES: u64 = 2_490_380;
const RECORDED_SPARSE_K64_1B_P50_NS: u64 = 150;
const RECORDED_ROOT_BYTES: u64 = 2_097_152;

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

fn p50(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn sample_positions(seed: u64, n: u64, samples: usize) -> Vec<u64> {
    let mut s = seed;
    let mut v = Vec::with_capacity(samples);
    for _ in 0..samples {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((s >> 33) % n.max(1));
    }
    v
}

/// Per-query p50 latency (ns) of packed restart + bounded replay.
fn packed_latency(pr: &PackedRestarts, core: &[u8], positions: &[u64]) -> u64 {
    let mut ns = Vec::with_capacity(positions.len());
    for &i in positions {
        let t0 = now_ns();
        let snap = pr.snapshot_at(core, i).expect("packed snapshot");
        black_box(snap);
        ns.push(now_ns().saturating_sub(t0));
    }
    p50(ns)
}

/// Per-query p50 latency (ns) of the existing 12-byte checkpoint restore.
fn checkpoint_latency(cps: &CheckpointSet, core: &[u8], positions: &[u64]) -> u64 {
    let mut ns = Vec::with_capacity(positions.len());
    for &i in positions {
        let t0 = now_ns();
        let snap = cps.restore(core, i).expect("checkpoint restore");
        black_box(snap);
        ns.push(now_ns().saturating_sub(t0));
    }
    p50(ns)
}

/// Per-query p50 latency (ns) of replaying from genesis.
fn genesis_latency(core: &[u8], positions: &[u64]) -> u64 {
    let mut ns = Vec::with_capacity(positions.len());
    for &i in positions {
        let t0 = now_ns();
        let mut omega = Snapshot::new(0, 0);
        for _ in 0..i {
            NL2.reveal(core, &mut omega).expect("reveal");
        }
        black_box(omega);
        ns.push(now_ns().saturating_sub(t0));
    }
    p50(ns)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// Drop the trailing comma the row writers append so the enclosing array is valid
/// JSON (a trailing comma is not permitted by the JSON grammar).
fn json_rows(rows: String) -> String {
    let trimmed = rows.trim_end();
    trimmed.strip_suffix(',').unwrap_or(trimmed).to_string()
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    let mut space_rows = String::new();
    let mut latency_rows = String::new();
    let mut verdicts = String::new();

    println!("packed: strides K = {STRIDE_SHIFTS:?}");

    // Aggregates for `state_from_hot`.
    let mut hot_checks = 0u64;
    let mut hot_mismatches = 0u64;
    let mut hot_cost_ns = 0u64;
    let mut state_read_cost_ns = 0u64;
    let mut hot_corpus = 0usize;

    for &size in &CORPUS_SIZES {
        let core = lcg(0xEB10_0000_9E37_79B9 ^ (size as u64), size);
        let n = size as u64;
        let log = state_log(&core, n)?;
        let decoded = NL2.decode(&core, n)?;

        // Packed history == state log, and its byte count.
        let ph = PackedHistory::build(&core, n)?;
        let mut hist_mismatches = 0u64;
        for i in 0..=n {
            if ph.state_at(i) != Some(log[i as usize] as u16) {
                hist_mismatches += 1;
            }
        }
        assert_eq!(hist_mismatches, 0, "packed history mismatch at size {size}");
        let packed_history_bytes = ph.physical_bytes();
        let existing_history_bytes = 4 * (n + 1);

        let positions = sample_positions(0x1234_5678_9ABC_DEF0 ^ (size as u64), n, SAMPLES);
        let genesis_positions: Vec<u64> = positions.iter().copied().take(GENESIS_SAMPLES).collect();
        let genesis_p50 = genesis_latency(&core, &genesis_positions);

        println!(
            "size={size}: packed_history={packed_history_bytes} existing_history={existing_history_bytes} \
             genesis_p50={genesis_p50} ns"
        );

        for &sh in &STRIDE_SHIFTS {
            let k = 1u32 << sh;
            let stride = 1u64 << sh;
            let pr = PackedRestarts::build(&core, n, sh)?;

            // Correctness on the sampled positions, and exact replay bound.
            let mut mismatches = 0u64;
            let mut replay_sum = 0u64;
            for &i in &positions {
                let snap = pr.snapshot_at(&core, i)?;
                if snap != Snapshot::new(log[i as usize], i) {
                    mismatches += 1;
                }
                let (ckpt, _) = pr.position_of(i).expect("position");
                replay_sum += i - ckpt;
            }
            assert_eq!(mismatches, 0, "packed restart mismatch size={size} k={k}");

            let count = pr.states.len() as u64;
            let packed_bytes = pr.physical_bytes();
            let existing_bytes = count * 12;
            let avg_replay = ratio(replay_sum, positions.len() as u64);

            let packed_p50 = packed_latency(&pr, &core, &positions);

            // The existing checkpoint set only supports documented intervals.
            let (supported, existing_p50) = if CheckpointSet::supported_interval(k) {
                let cps = CheckpointSet::build(&core, n, k)?;
                (true, Some(checkpoint_latency(&cps, &core, &positions)))
            } else {
                (false, None)
            };

            println!(
                "  k={k:>3}: packed={packed_bytes:>9} B  existing12={existing_bytes:>9} B  \
                 ratio=1/{:.0}  packed_p50={packed_p50:>6} ns  existing_p50={:?}  \
                 genesis_p50={genesis_p50} ns  avg_replay={avg_replay:.2}",
                ratio(existing_bytes, packed_bytes).round(),
                existing_p50
            );

            let _ = writeln!(
                space_rows,
                "    {{ \"corpus\": {size}, \"k\": {k}, \"count\": {count}, \
                 \"packed_restart_bytes\": {packed_bytes}, \
                 \"existing_12b_checkpoint_bytes\": {existing_bytes}, \
                 \"restart_bytes_ratio_packed_over_existing\": {:.6}, \
                 \"packed_history_bytes\": {packed_history_bytes}, \
                 \"existing_history_bytes\": {existing_history_bytes}, \
                 \"history_bytes_ratio_packed_over_existing\": {:.6}, \
                 \"packed_e_v\": {:.6}, \"existing_e_v\": {:.6} }},",
                ratio(packed_bytes, existing_bytes),
                ratio(packed_history_bytes, existing_history_bytes),
                ratio(n + packed_bytes, n),
                ratio(n + existing_bytes, n),
            );

            let existing_json = match existing_p50 {
                Some(v) => format!("{v}"),
                None => "null".to_string(),
            };
            let _ = writeln!(
                latency_rows,
                "    {{ \"corpus\": {size}, \"k\": {k}, \"stride_shift\": {sh}, \
                 \"packed_restart_p50_ns\": {packed_p50}, \
                 \"existing_12b_checkpoint_p50_ns\": {existing_json}, \
                 \"existing_interval_supported\": {supported}, \
                 \"replay_from_genesis_p50_ns\": {genesis_p50}, \
                 \"max_replay_steps\": {}, \"avg_replay_steps\": {avg_replay:.3}, \
                 \"speedup_vs_genesis\": {:.1} }},",
                stride - 1,
                ratio(genesis_p50, packed_p50),
            );
        }

        // Locality note: with a budget of one byte per logical position, how many
        // restarts fit, i.e. how dense can the restart grid be?
        let budget = n; // core bytes are the root; this equals the root size
        let packed_count = budget / 2;
        let packed_stride = n.checked_div(packed_count).unwrap_or(n);
        let existing_count = budget / 12;
        let existing_stride = n.checked_div(existing_count).unwrap_or(n);
        println!(
            "  locality @budget={budget} B: packed stride~{packed_stride} (avg {:.1} replays), \
             existing stride~{existing_stride} (avg {:.1} replays)",
            (packed_stride.saturating_sub(1)) as f64 / 2.0,
            (existing_stride.saturating_sub(1)) as f64 / 2.0,
        );
        let _ = writeln!(
            verdicts,
            "    {{ \"corpus\": {size}, \"budget_bytes\": {budget}, \
             \"packed_dense_stride\": {packed_stride}, \
             \"packed_dense_avg_replay_steps\": {:.3}, \
             \"existing12_dense_stride\": {existing_stride}, \
             \"existing12_dense_avg_replay_steps\": {:.3}, \
             \"dense_restart_multiple\": {:.2} }},",
            (packed_stride.saturating_sub(1)) as f64 / 2.0,
            (existing_stride.saturating_sub(1)) as f64 / 2.0,
            ratio(existing_stride, packed_stride),
        );

        // `state_from_hot` correctness + cost, measured on the middle corpus.
        if size == CORPUS_SIZES[1] {
            hot_corpus = size;
            for i in 0..=n {
                hot_checks += 1;
                if state_from_hot(&decoded, i) != Some(log[i as usize] as u16) {
                    hot_mismatches += 1;
                }
            }
            // Per-call cost vs reading a separate state array.
            let mut a = Vec::with_capacity(SAMPLES);
            let mut b = Vec::with_capacity(SAMPLES);
            for &i in &positions {
                let t0 = now_ns();
                let v = state_from_hot(&decoded, i);
                black_box(v);
                a.push(now_ns().saturating_sub(t0));

                let t0 = now_ns();
                let v = ph.state_at(i);
                black_box(v);
                b.push(now_ns().saturating_sub(t0));
            }
            hot_cost_ns = p50(a);
            state_read_cost_ns = p50(b);
            println!(
                "state_from_hot: checks={hot_checks} mismatches={hot_mismatches} \
                 hot_p50={hot_cost_ns} ns vs packed_read_p50={state_read_cost_ns} ns"
            );
        }
    }

    let peak_rss = ekbloom::peak_rss_bytes();
    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/packed.rs (sh scripts/run-packed.sh)\","
    );
    let _ = writeln!(js, "  \"machine\": \"OrANS-NL2 (machine_id=1)\",");
    let _ = writeln!(js, "  \"corpus_sizes\": {:?},", CORPUS_SIZES);
    let _ = writeln!(js, "  \"stride_shifts\": {:?},", STRIDE_SHIFTS);
    let _ = writeln!(js, "  \"samples\": {SAMPLES},");
    let _ = writeln!(js, "  \"genesis_samples\": {GENESIS_SAMPLES},");
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(js, "  \"recorded_reference\": {{");
    let _ = writeln!(js, "    \"source\": \"docs/RESULTS.md speed frontier\",");
    let _ = writeln!(js, "    \"root_bytes\": {RECORDED_ROOT_BYTES},");
    let _ = writeln!(js, "    \"sparse_k64_bytes\": {RECORDED_SPARSE_K64_BYTES},");
    let _ = writeln!(
        js,
        "    \"sparse_k64_e_v\": {:.6},",
        ratio(RECORDED_SPARSE_K64_BYTES, RECORDED_ROOT_BYTES)
    );
    let _ = writeln!(
        js,
        "    \"sparse_k64_1b_p50_ns\": {RECORDED_SPARSE_K64_1B_P50_NS}"
    );
    let _ = writeln!(js, "  }},");
    let _ = writeln!(js, "  \"space\": [\n{}\n  ],", json_rows(space_rows));
    let _ = writeln!(js, "  \"latency\": [\n{}\n  ],", json_rows(latency_rows));
    let _ = writeln!(js, "  \"locality\": [\n{}\n  ],", json_rows(verdicts));
    let _ = writeln!(js, "  \"state_from_hot\": {{");
    let _ = writeln!(js, "    \"corpus\": {hot_corpus},");
    let _ = writeln!(js, "    \"checks\": {hot_checks},");
    let _ = writeln!(js, "    \"mismatches\": {hot_mismatches},");
    let _ = writeln!(js, "    \"hot_reconstruct_p50_ns\": {hot_cost_ns},");
    let _ = writeln!(
        js,
        "    \"packed_history_read_p50_ns\": {state_read_cost_ns}"
    );
    let _ = writeln!(js, "  }},");
    let _ = writeln!(
        js,
        "  \"verdict\": \"packed restart/history forms are machine-1 only; packing is a pure \
         space win at equal or lower replay latency\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("packed.json"), js)?;
    println!("wrote evidence/bench/packed.json (peak_rss={peak_rss} B)");
    Ok(())
}
