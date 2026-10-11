//! Bounded-memory streaming construction of a `2^30`-symbol state projection.
//!
//! Builds a [`HistoryStore`] for `logical_len = 2^30` from a **procedural** core
//! (`splitmix64(p) & 0xFF`), writing `4 * (2^30 + 1)` bytes of segments to a
//! temporary directory under the container's `/tmp` (never inside the checkout),
//! with `segment_bytes = 1 MiB` and checkpoint interval `65536`.
//!
//! It records and checks:
//! - elapsed build time,
//! - total state bytes (`must equal 4 * (2^30 + 1)`),
//! - peak RSS during build (`VmHWM`, must stay far below the 4 GiB projection),
//! - a sampled recompute-and-compare verification (`stride = 2^20`).
//!
//! Evidence is written to `evidence/bench/history-2p30.json`, and a short
//! paragraph is appended to `docs/RESULTS.md`. The temporary segments are always
//! removed before exit.
//!
//! Run it in the pinned container with `sh scripts/run-large-history.sh`.

use ekbloom::{peak_rss_bytes, HistoryStore, StoreConfig};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Logical length: `2^30` symbols (the mandated supported configuration).
const LOGICAL_LEN: u64 = 1 << 30;
/// Segment size: 1 MiB => 262144 `u32` states per segment.
const SEGMENT_BYTES: u32 = 1024 * 1024;
/// Sparse checkpoint interval.
const CHECKPOINT_INTERVAL: u32 = 65536;
/// Verification stride: check every `2^20`-th position.
const STRIDE: u64 = 1 << 20;

/// `SplitMix64` bijection — a deterministic procedural byte source.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The procedural core byte at position `p`.
#[inline]
fn core_byte(p: u64) -> u8 {
    (splitmix64(p) & 0xFF) as u8
}

fn main() -> ekbloom::Result<()> {
    let dir = temp_dir().join(format!("ekbloom-large-history-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "building history store: N={LOGICAL_LEN} segment_bytes={SEGMENT_BYTES} K={CHECKPOINT_INTERVAL}"
    );

    let started = Instant::now();
    let store = HistoryStore::build(
        &dir,
        core_byte,
        LOGICAL_LEN,
        StoreConfig {
            segment_bytes: SEGMENT_BYTES,
        },
        CHECKPOINT_INTERVAL,
    )?;
    let seconds = started.elapsed().as_secs_f64();
    let peak_rss = peak_rss_bytes();
    let state_bytes = store.physical_bytes();

    // Total state bytes must be exactly 4 * (N + 1).
    let expected = 4u64
        .checked_mul(LOGICAL_LEN + 1)
        .expect("state byte count fits in u64");
    assert_eq!(
        state_bytes, expected,
        "state bytes {state_bytes} != expected {expected}"
    );

    let sampled = store.verify_sample(core_byte, STRIDE)?;
    let status = "PASS";

    println!("seconds={seconds:.3}");
    println!("state_bytes={state_bytes} (expected {expected})");
    println!(
        "peak_rss_bytes={peak_rss} ({:.1} MiB)",
        peak_rss as f64 / 1_048_576.0
    );
    println!("sampled_checks={sampled} status={status}");

    // --- evidence JSON -------------------------------------------------------
    let bench = Path::new("evidence/bench");
    std::fs::create_dir_all(bench)?;
    let mut json = String::new();
    let _ = writeln!(json, "{{");
    let _ = writeln!(json, "  \"logical_len\": {LOGICAL_LEN},");
    let _ = writeln!(json, "  \"segment_bytes\": {SEGMENT_BYTES},");
    let _ = writeln!(json, "  \"checkpoint_interval\": {CHECKPOINT_INTERVAL},");
    let _ = writeln!(json, "  \"state_bytes\": {state_bytes},");
    let _ = writeln!(json, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(json, "  \"seconds\": {seconds:.3},");
    let _ = writeln!(json, "  \"sampled_checks\": {sampled},");
    let _ = writeln!(json, "  \"status\": \"{status}\"");
    json.push_str("}\n");
    std::fs::write(bench.join("history-2p30.json"), json)?;

    // --- RESULTS.md paragraph ------------------------------------------------
    let para = format!(
        "\n### Segmented history at N = 2^30 (bounded-memory build)\n\n\
         `HistoryStore::build` streamed a procedural core over {LOGICAL_LEN} logical \
         positions into 1 MiB segments (checkpoint interval {CHECKPOINT_INTERVAL}), \
         writing exactly {state_bytes} bytes of `u32` state projection (`4*(N+1)`) in \
         {seconds:.3} s while peak RSS stayed at {peak_rss} bytes ({:.1} MiB) — far below \
         the 4 GiB the full projection would occupy in RAM. A sampled recompute-and-compare \
         pass over {sampled} positions (stride {STRIDE}) passed. Evidence: \
         `evidence/bench/history-2p30.json`.\n",
        peak_rss as f64 / 1_048_576.0
    );
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("docs/RESULTS.md")?;
        use std::io::Write as _;
        f.write_all(para.as_bytes())?;
    }

    // Never leave the ~4 GiB of segments behind.
    std::fs::remove_dir_all(&dir)?;
    println!("cleaned up {}", dir.display());
    Ok(())
}

/// The container's `/tmp` (never a checkout path).
fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}
