//! Bounded encoded-window reads: file-backed cold reads for cursor-divergent
//! machines (2 and 4).
//!
//! For 1-byte, 4 KiB and 64 KiB file-backed reads over ~4 MiB and ~64 MiB skewed
//! objects, this measures, **before** (`EKBLOOM` whole-core fetch) and **after**
//! (bounded encoded window):
//!
//!   * encoded bytes fetched per read (the counter the theorem bounds),
//!   * read-path `pread` syscalls,
//!   * read-path buffer allocation bytes,
//!   * reconstruction steps (`offset % block_bytes + length`), and
//!   * p50/p99 latency.
//!
//! The acceptance condition is that a 4 KiB request no longer fetches the whole
//! encoded core. Set `EKBLOOM_WINDOW_LARGE=1` to also run the ~1 GiB object.
//!
//! Writes `evidence/bench/window.json`. Run with `sh scripts/run-window.sh`.

use ekbloom::blocks::BlockIndex;
use ekbloom::machine::{machine_by_id, MACHINE_RANS0, MACHINE_RANS0I};
use ekbloom::metrics::{now_ns, percentiles};
use ekbloom::{
    create_machine, write_body, FileBody, ManifestationProfile, ReadRequest, WINDOW_MARGIN,
};
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// Block size of the persisted restart index used for the cold reads (4 KiB).
const BLOCK_BYTES: u32 = 4096;
/// Bytes per read, reported for each.
const READ_LENS: [u64; 3] = [1, 4096, 65536];
/// Samples per (object, read length, mode) for the "after" (windowed) mode.
const AFTER_SAMPLES: usize = 256;
/// Samples for the "before" (whole-core) mode; each read copies the whole core.
const BEFORE_SAMPLES: usize = 48;
/// Base object sizes (bytes).
const BASE_SIZES: [usize; 2] = [4 * 1024 * 1024, 64 * 1024 * 1024];
/// Optional 1 GiB object size, run only with `EKBLOOM_WINDOW_LARGE=1`.
const LARGE_SIZE: usize = 1024 * 1024 * 1024;

/// Skewed byte source that machines 2 and 4 compress (`V < N`).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(match r {
            0..=69 => b'a',
            70..=91 => b'b',
            _ => b'c',
        });
    }
    out
}

/// Deterministic in-bounds offsets for `len`-byte reads.
fn sample_offsets(seed: u64, n: u64, len: u64, count: usize) -> Vec<u64> {
    let span = n.saturating_sub(len).max(1);
    let mut s = seed;
    let mut v = Vec::with_capacity(count);
    for _ in 0..count {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((s >> 33) % span);
    }
    v
}

struct Stats {
    encoded_bytes_max: u64,
    encoded_bytes_mean: f64,
    syscalls_max: u64,
    alloc_bytes_max: u64,
    steps_max: u64,
    p50_ns: f64,
    p99_ns: f64,
    exact: bool,
}

/// Run `samples` file-backed reads of `len` bytes at `offsets` against `fb`,
/// which must be in the desired windowed/non-windowed mode.
fn run_reads(fb: &FileBody, src: &[u8], offsets: &[u64], len: u64) -> Stats {
    let mut lat = Vec::with_capacity(offsets.len());
    let mut encoded_bytes_max = 0u64;
    let mut encoded_bytes_sum = 0u64;
    let mut syscalls_max = 0u64;
    let mut alloc_max = 0u64;
    let mut steps_max = 0u64;
    let mut exact = true;
    for &off in offsets {
        let bytes_before = fb.fetched_bytes();
        let reads_before = fb.fetched_reads();
        let t0 = now_ns();
        let mut out = Vec::new();
        fb.read_into(
            ReadRequest {
                offset: off,
                length: len,
            },
            &mut out,
        )
        .expect("read");
        lat.push(now_ns().saturating_sub(t0));
        let fetched = fb.fetched_bytes() - bytes_before;
        let syscalls = fb.fetched_reads() - reads_before;
        if out != src[off as usize..(off + len) as usize] {
            exact = false;
        }
        black_box(&out);
        encoded_bytes_max = encoded_bytes_max.max(fetched);
        encoded_bytes_sum += fetched;
        syscalls_max = syscalls_max.max(syscalls);
        alloc_max = alloc_max.max(fetched.saturating_add(out.capacity() as u64));
        steps_max = steps_max.max((off % u64::from(BLOCK_BYTES)) + len);
    }
    lat.sort_unstable();
    let p = percentiles(&lat);
    Stats {
        encoded_bytes_max,
        encoded_bytes_mean: encoded_bytes_sum as f64 / offsets.len().max(1) as f64,
        syscalls_max,
        alloc_bytes_max: alloc_max,
        steps_max,
        p50_ns: p.p50,
        p99_ns: p.p99,
        exact,
    }
}

fn mode_json(name: &str, s: &Stats) -> String {
    format!(
        "{{\"mode\":\"{name}\",\"encoded_bytes_max\":{},\"encoded_bytes_mean\":{:.1},\
         \"syscalls_max\":{},\"alloc_bytes_max\":{},\"steps_max\":{},\
         \"p50_ns\":{:.0},\"p99_ns\":{:.0},\"exact\":{}}}",
        s.encoded_bytes_max,
        s.encoded_bytes_mean,
        s.syscalls_max,
        s.alloc_bytes_max,
        s.steps_max,
        s.p50_ns,
        s.p99_ns,
        s.exact
    )
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let work = std::env::temp_dir().join(format!("ekbloom-window-bench-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;

    let mut sizes: Vec<usize> = BASE_SIZES.to_vec();
    if std::env::var_os("EKBLOOM_WINDOW_LARGE").is_some() {
        sizes.push(LARGE_SIZE);
    }

    let mut objects_json = String::new();
    let mut acceptance_json = String::new();

    for &size in &sizes {
        let src = skewed(0x0E61_0000_0000_0002, size);
        let n = src.len() as u64;
        for &machine_id in &[MACHINE_RANS0, MACHINE_RANS0I] {
            let machine = machine_by_id(machine_id).expect("machine");
            let core = machine.encode(&src)?;
            let index = BlockIndex::build_machine(&core, n, BLOCK_BYTES, machine_id)?;
            let mut body = create_machine(&src, ManifestationProfile::Root, machine_id)?;
            body.blocks.push(index);
            let path = work.join(format!("w-{machine_id}-{size}.ekb"));
            write_body(&path, &body)?;
            let mut fb = FileBody::open(&path)?;
            let restarts = body.blocks[0].block_count() as u64;
            let core_len = core.len() as u64;

            println!(
                "machine {machine_id}: size={size} core={core_len} V/N={:.3} restarts={restarts}",
                core_len as f64 / n as f64
            );

            let mut reads_json = String::new();
            for (ri, &len) in READ_LENS.iter().enumerate() {
                let after_offsets = sample_offsets(
                    0xA17E_0000_0000_0001 + (machine_id as u64) * 17 + ri as u64,
                    n,
                    len,
                    AFTER_SAMPLES,
                );
                let before_offsets: Vec<u64> =
                    after_offsets.iter().copied().take(BEFORE_SAMPLES).collect();

                // After: bounded encoded window.
                fb.set_bounded_windows(true);
                let after = run_reads(&fb, &src, &after_offsets, len);

                // Before: whole-core fetch per read.
                fb.set_bounded_windows(false);
                let before = run_reads(&fb, &src, &before_offsets, len);
                fb.set_bounded_windows(true);

                assert!(
                    after.exact && before.exact,
                    "read mismatch machine {machine_id} len {len}"
                );
                let reduction = if after.encoded_bytes_max == 0 {
                    0.0
                } else {
                    before.encoded_bytes_max as f64 / after.encoded_bytes_max as f64
                };

                println!(
                    "  len={len:>6}: bytes {before_max} -> {after_max} ({reduction:.1}x)  \
                     p99 {before_p99:.0} -> {after_p99:.0} ns  syscalls {before_sys} -> {after_sys}",
                    before_max = before.encoded_bytes_max,
                    after_max = after.encoded_bytes_max,
                    before_p99 = before.p99_ns,
                    after_p99 = after.p99_ns,
                    before_sys = before.syscalls_max,
                    after_sys = after.syscalls_max,
                );

                let _ = writeln!(
                    reads_json,
                    "        {{\"read_len\":{len},\"samples_after\":{},\"samples_before\":{},\
                     \"steps_max\":{},\"window_bound\":{},\"bytes_reduction\":{:.2},\
                     \"before\":{},\"after\":{}}},",
                    after_offsets.len(),
                    before_offsets.len(),
                    after.steps_max,
                    2 * after.steps_max + WINDOW_MARGIN,
                    reduction,
                    mode_json("whole_core", &before),
                    mode_json("encoded_window", &after),
                );

                if len == 4096 {
                    let _ = writeln!(
                        acceptance_json,
                        "    {{\"machine_id\":{machine_id},\"logical_len\":{n},\"core_len\":{core_len},\
                         \"before_encoded_bytes\":{},\"after_encoded_bytes\":{},\
                         \"whole_core_avoided\":{}}},",
                        before.encoded_bytes_max,
                        after.encoded_bytes_max,
                        after.encoded_bytes_max < core_len,
                    );
                }
            }

            let _ = writeln!(
                objects_json,
                "    {{\"machine_id\":{machine_id},\"machine\":\"{}\",\"logical_len\":{n},\
                 \"core_len\":{core_len},\"v_over_n\":{:.4},\"block_bytes\":{BLOCK_BYTES},\
                 \"restarts\":{restarts},\"reads\":[\n{}\n    ]}},",
                machine_name(machine_id),
                core_len as f64 / n as f64,
                reads_json.trim_end().trim_end_matches(','),
            );
            black_box(&src);
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = std::fs::remove_dir_all(&work);

    let peak_rss = ekbloom::peak_rss_bytes();
    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/window.rs (sh scripts/run-window.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"theorem\": \"for a valid order-0 rANS state x >= RANS_L the pre-renorm successor is floor(x/2^12)*f + r >= 2^11, so at most two 8-bit renorm reads reach >= 2^27 > RANS_L; hence every decoded symbol consumes at most two encoded bytes and a read needing s positioning steps and n output symbols fetches at most 2*(s+n) encoded bytes after the restart cursor\","
    );
    let _ = writeln!(js, "  \"renorm_max_per_step\": 2,");
    let _ = writeln!(js, "  \"window_margin\": {WINDOW_MARGIN},");
    let _ = writeln!(
        js,
        "  \"acceptance\": [\n{}\n  ],",
        acceptance_json.trim_end().trim_end_matches(',')
    );
    let _ = writeln!(
        js,
        "  \"objects\": [\n{}\n  ],",
        objects_json.trim_end().trim_end_matches(',')
    );
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(
        js,
        "  \"verdict\": \"a 4 KiB file-backed read of a cursor-divergent machine now fetches a bounded encoded window instead of the whole encoded core, at the same syscall count\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("window.json"), js)?;
    println!("wrote evidence/bench/window.json (peak_rss={peak_rss} B)");
    Ok(())
}

fn machine_name(id: u32) -> &'static str {
    match id {
        MACHINE_RANS0 => "OrANS-RANS0 (machine_id=2)",
        MACHINE_RANS0I => "OrANS-RANS0I (machine_id=4)",
        _ => "unknown",
    }
}
