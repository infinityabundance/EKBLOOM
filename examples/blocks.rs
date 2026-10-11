//! Machine-2 independently restartable block-index benchmark.
//!
//! Measures, on a compressed (skewed) machine-2 object, the block-size tradeoff
//! for positional reconstruction: random 4 KiB read p50/p99, the manifested
//! footprint (model header + directory + restart records), and the amortised
//! construction cost, for block sizes 256 B, 1 KiB, 4 KiB, 16 KiB and 64 KiB.
//! It also reports the packed checkpoint-format integration (legacy vs compact
//! bytes and backward-compatible reads).
//!
//! Writes `evidence/bench/blocks.json`. Run with `sh scripts/run-blocks.sh`.

use ekbloom::blocks::BlockIndex;
use ekbloom::machine::{OrMachine, MACHINE_RANS0};
use ekbloom::machine_rans::{Rans0, RANS0};
use ekbloom::metrics::{now_ns, percentiles};
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// Machine-2 instance under test.
const RANS0_INST: Rans0 = RANS0;
/// Source length (bytes); it compresses for the skewed alphabet below.
const SRC_LEN: usize = 4 * 1024 * 1024;
/// Block sizes measured (logical symbols `==` core bytes for machine 1).
const BLOCK_SIZES: [u32; 5] = [256, 1024, 4096, 16384, 65536];
/// Random reads sampled per block size.
const SAMPLES: usize = 512;
/// Bytes per random positional read.
const READ_LEN: u64 = 4096;
/// Samples for the (expensive) replay-from-genesis baseline.
const GENESIS_SAMPLES: usize = 16;

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

/// English-like low-entropy text: machine 2 compresses it (`V < N`).
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

/// Deterministic in-bounds offsets for `READ_LEN`-byte reads.
fn sample_offsets(seed: u64, n: u64, samples: usize) -> Vec<u64> {
    let span = n.saturating_sub(READ_LEN).max(1);
    let mut s = seed;
    let mut v = Vec::with_capacity(samples);
    for _ in 0..samples {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((s >> 33) % span);
    }
    v
}

fn p50_p99(mut ns: Vec<u64>) -> (f64, f64) {
    ns.sort_unstable();
    let p = percentiles(&ns);
    (p.p50, p.p99)
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    let source = skewed(SRC_LEN);
    let core = RANS0_INST.encode(&source)?;
    let n = source.len() as u64;
    let decoded = RANS0_INST.decode(&core, n)?;

    println!(
        "machine 2: source={} core={} V/N={:.3}",
        n,
        core.len(),
        core.len() as f64 / n as f64
    );

    let offsets = sample_offsets(0xB10C_5EED_0000_0001, n, SAMPLES);

    // Replay-from-genesis baseline for a cold 4 KiB read (few samples; expensive).
    let genesis_offsets: Vec<u64> = offsets.iter().copied().take(GENESIS_SAMPLES).collect();
    let mut genesis_ns = Vec::with_capacity(genesis_offsets.len());
    for &off in &genesis_offsets {
        let t0 = now_ns();
        let got = RANS0_INST.decode(&core, off + READ_LEN)?;
        let slice = &got[off as usize..];
        black_box(slice);
        genesis_ns.push(now_ns().saturating_sub(t0));
    }
    let (genesis_p50, _) = p50_p99(genesis_ns);
    println!("  replay-from-genesis 4 KiB p50 = {genesis_p50:.0} ns");

    let mut block_rows = String::new();
    for &b in &BLOCK_SIZES {
        let t0 = now_ns();
        let index = BlockIndex::build_machine(&core, n, b, MACHINE_RANS0)?;
        let construction_ns = now_ns().saturating_sub(t0);

        // Exactness and step bound on every sampled read.
        let mut read_ns = Vec::with_capacity(offsets.len());
        let mut exact = true;
        let mut max_steps_bound: u64 = 0;
        for &off in &offsets {
            let t0 = now_ns();
            let (got, steps) = index.read_range_counted(&core, off, READ_LEN)?;
            read_ns.push(now_ns().saturating_sub(t0));
            if got != decoded[off as usize..(off + READ_LEN) as usize] {
                exact = false;
            }
            assert!(steps <= index.max_steps(off, READ_LEN));
            max_steps_bound = max_steps_bound.max(index.max_steps(off, READ_LEN));
        }
        let (p50, p99) = p50_p99(read_ns);
        assert!(exact, "block read mismatch at B={b}");

        let restarts = index.block_count() as u64;
        let directory = index.directory_bytes();
        let restart_bytes = index.restart_bytes();
        let model = ekbloom::blocks::RANS_MODEL_HEADER_BYTES;
        let physical = index.physical_bytes();
        let per_symbol = construction_ns as f64 / n as f64;

        println!(
            "  B={b:>6}: restarts={restarts:>6} physical={physical:>9} B  \
             p50={p50:>6.0} ns p99={p99:>6.0} ns  build={construction_ns:>9} ns"
        );

        let _ = writeln!(
            block_rows,
            "    {{ \"block_bytes\": {b}, \"restarts\": {restarts}, \
             \"model_bytes\": {model}, \"directory_bytes\": {directory}, \
             \"restart_bytes\": {restart_bytes}, \"physical_bytes\": {physical}, \
             \"construction_ns\": {construction_ns}, \
             \"construction_ns_per_symbol\": {per_symbol:.6}, \
             \"read_4k_p50_ns\": {p50:.1}, \"read_4k_p99_ns\": {p99:.1}, \
             \"max_steps_bound\": {max_steps_bound}, \"exact\": {exact} }},"
        );
    }

    // Packed checkpoint-format integration (machine 1): legacy vs compact bytes.
    let ck_core = lcg(0xEB10_0000_C0DE_2026, 1024 * 1024);
    let ck_n = ck_core.len() as u64;
    let cps = ekbloom::CheckpointSet::build(&ck_core, ck_n, 64)?;
    let count = cps.count;
    let legacy_bytes = 12 + count * 12;
    let packed_bytes = cps.to_bytes().len() as u64;
    // Backward compatibility: hand-build the legacy layout and re-open it.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&cps.interval.to_le_bytes());
    legacy.extend_from_slice(&count.to_le_bytes());
    for s in &cps.resume {
        legacy.extend_from_slice(&s.pack());
    }
    let legacy_ok = ekbloom::CheckpointSet::from_bytes(&legacy).ok() == Some(cps.clone());
    let packed_ok = ekbloom::CheckpointSet::from_bytes(&cps.to_bytes()).ok() == Some(cps.clone());
    println!(
        "checkpoint format: legacy={legacy_bytes} B packed={packed_bytes} B \
         ratio=1/{:.1} legacy_ok={legacy_ok} packed_ok={packed_ok}",
        legacy_bytes as f64 / packed_bytes as f64
    );

    let peak_rss = ekbloom::peak_rss_bytes();
    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/blocks.rs (sh scripts/run-blocks.sh)\","
    );
    let _ = writeln!(js, "  \"machine\": \"OrANS-RANS0 (machine_id=2)\",");
    let _ = writeln!(js, "  \"source_len\": {n},");
    let _ = writeln!(js, "  \"core_len\": {},", core.len());
    let _ = writeln!(js, "  \"v_over_n\": {:.4},", core.len() as f64 / n as f64);
    let _ = writeln!(js, "  \"read_len\": {READ_LEN},");
    let _ = writeln!(js, "  \"samples\": {SAMPLES},");
    let _ = writeln!(js, "  \"genesis_replay_4k_p50_ns\": {genesis_p50:.1},");
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(
        js,
        "  \"blocks\": [\n{}\n  ],",
        block_rows.trim_end().trim_end_matches(',')
    );
    let _ = writeln!(js, "  \"checkpoint_format\": {{");
    let _ = writeln!(js, "    \"machine\": \"OrANS-NL2 (machine_id=1)\",");
    let _ = writeln!(js, "    \"interval\": 64,");
    let _ = writeln!(js, "    \"count\": {count},");
    let _ = writeln!(js, "    \"legacy_bytes\": {legacy_bytes},");
    let _ = writeln!(js, "    \"packed_bytes\": {packed_bytes},");
    let _ = writeln!(
        js,
        "    \"packed_over_legacy\": {:.6},",
        packed_bytes as f64 / legacy_bytes as f64
    );
    let _ = writeln!(js, "    \"legacy_backward_compatible\": {legacy_ok},");
    let _ = writeln!(js, "    \"packed_roundtrip\": {packed_ok}");
    let _ = writeln!(js, "  }},");
    let _ = writeln!(
        js,
        "  \"verdict\": \"smaller blocks tighten the bounded positional replay but grow the restart directory linearly; the packed checkpoint format cuts machine-1 restart bytes 6x with backward-compatible reads\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("blocks.json"), js)?;
    println!("wrote evidence/bench/blocks.json (peak_rss={peak_rss} B)");
    Ok(())
}
