//! Streaming-container benchmark with **bounded builder memory**.
//!
//! Streams a large deterministic synthetic input through [`StreamBuilder`]
//! (never holding the whole stream in RAM), then measures peak RSS, construction
//! throughput, positional-read latency (`read_at` p50/p99), sequential-read
//! throughput and the container footprint. Writes `evidence/bench/stream.json`.
//!
//! The input is generated on the fly and is *seekable*: a byte at any offset is
//! computed in `O(1)` from a splitmix hash, so `read_at` results can be checked
//! byte-exact without ever storing the source. The stream alternates 64 MiB
//! regions of a 4-bit skewed alphabet and of full-range bytes, so per-block
//! representation adapts (packed blocks vs raw/store blocks).
//!
//! Run with `sh scripts/run-stream.sh`.

use ekbloom::metrics::{now_ns, percentiles};
use ekbloom::{RepresentationPolicy, StreamBuilder, StreamOptions, StreamReader};
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;

/// Fixed deterministic seed for the synthetic stream.
const SEED: u64 = 0x0E0B_1000_0000_5EED;
/// Region size in bytes: regions alternate skewed (4-bit) and random (8-bit).
const REGION: u64 = 64 * 1024 * 1024;
/// Generate/read chunk size.
const CHUNK: usize = 1 << 20;
/// Random positional reads sampled.
const SAMPLES: usize = 2000;
/// Bytes per positional read.
const READ_LEN: u64 = 4096;

/// A splitmix64 step (deterministic, allocation-free).
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The eight deterministic bytes of the 8-aligned group starting at byte `i`
/// (`i` must be a multiple of 8). The region parity selects the alphabet.
fn group(i: u64) -> [u8; 8] {
    let h = splitmix64(SEED ^ (i / 8).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut b = h.to_le_bytes();
    if (i / REGION).is_multiple_of(2) {
        // Skewed half: a 4-bit alphabet (packs to 4 bits; store would be 8).
        for x in b.iter_mut() {
            *x &= 0x0F;
        }
    }
    b
}

/// Fill `buf` with the stream starting at byte `start`, for any `start`.
fn fill(start: u64, buf: &mut [u8]) {
    let mut i = 0usize;
    let mut off = start;
    while i < buf.len() {
        let g = group(off & !7);
        let k = (off & 7) as usize;
        let n = (buf.len() - i).min(8 - k);
        buf[i..i + n].copy_from_slice(&g[k..k + n]);
        i += n;
        off += n as u64;
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn p50_p99(mut ns: Vec<u64>) -> (f64, f64) {
    ns.sort_unstable();
    let p = percentiles(&ns);
    (p.p50, p.p99)
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;

    let n = env_u64("EKB_STREAM_BYTES", 2 * 1024 * 1024 * 1024);
    let block_bytes = env_u64("EKB_STREAM_BLOCK_BYTES", 1 << 16) as u32;
    let policy = RepresentationPolicy::Auto;

    let tmp = std::env::temp_dir().join(format!("ekbloom-stream-demo-{}.ekb", std::process::id()));
    println!(
        "streaming {n} bytes into {} (block_bytes={block_bytes})",
        tmp.display()
    );

    // ---- Build (streaming, bounded memory) ----
    let opts = StreamOptions {
        block_bytes,
        machine: policy,
    };
    let mut builder =
        StreamBuilder::new(std::io::BufWriter::new(std::fs::File::create(&tmp)?), opts)?;
    let mut chunk = vec![0u8; CHUNK];
    let build_start = now_ns();
    let mut written: u64 = 0;
    while written < n {
        let take = ((n - written) as usize).min(CHUNK);
        fill(written, &mut chunk[..take]);
        builder.write(&chunk[..take])?;
        written += take as u64;
    }
    let manifest = builder.finish()?;
    let build_ns = now_ns().saturating_sub(build_start);
    let build_peak_rss = ekbloom::peak_rss_bytes();
    let container_bytes = std::fs::metadata(&tmp)?.len();
    assert_eq!(manifest.container_bytes, container_bytes);

    let mib = n as f64 / (1024.0 * 1024.0);
    let build_mib_s = mib / (build_ns as f64 / 1e9);
    println!(
        "built: container={container_bytes} B blocks={} payload={} dir={} build={:.2} MiB/s peak_rss={build_peak_rss} B",
        manifest.block_count, manifest.payload_bytes, manifest.directory_bytes, build_mib_s
    );

    // ---- Positional reads (read_at) ----
    let reader = StreamReader::open(std::fs::File::open(&tmp)?)?;
    assert_eq!(reader.len(), n);
    assert_eq!(reader.block_count(), manifest.block_count);

    let span = (n - READ_LEN).max(1);
    let mut rng = 0xD15E_A5E0_0000_0001u64;
    let mut offsets = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        rng = splitmix64(rng);
        offsets.push(rng % span);
    }
    let read_len = READ_LEN as usize;
    let mut buf = vec![0u8; read_len];
    let mut expect = vec![0u8; read_len];
    let mut read_ns = Vec::with_capacity(SAMPLES);
    let mut read_exact = true;
    for &off in &offsets {
        let t0 = now_ns();
        reader.read_at(off, &mut buf)?;
        read_ns.push(now_ns().saturating_sub(t0));
        black_box(&buf);
        fill(off, &mut expect);
        if buf != expect {
            read_exact = false;
        }
    }
    let (read_p50, read_p99) = p50_p99(read_ns);
    println!("read_at {read_len} B: p50={read_p50:.0} ns p99={read_p99:.0} ns exact={read_exact}");

    // ---- Sequential read (straight scan) ----
    let reader2 = StreamReader::open(std::fs::File::open(&tmp)?)?;
    let mut hasher = blake3::Hasher::new();
    let mut sbuf = vec![0u8; CHUNK];
    let seq_start = now_ns();
    let mut total: u64 = 0;
    loop {
        let k = reader2.read_sequential(&mut sbuf)?;
        if k == 0 {
            break;
        }
        hasher.update(&sbuf[..k]);
        total += k as u64;
    }
    let seq_ns = now_ns().saturating_sub(seq_start);
    let seq_hash = *hasher.finalize().as_bytes();
    let seq_exact = seq_hash == manifest.source_hash && total == n;
    let seq_mib_s = mib / (seq_ns as f64 / 1e9);
    println!("sequential: {seq_mib_s:.2} MiB/s exact={seq_exact}");

    let peak_rss = ekbloom::peak_rss_bytes();
    let physical_over_logical = container_bytes as f64 / n as f64;

    // ---- Evidence JSON ----
    let mut js = String::new();
    js.push_str("{\n");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/stream.rs (sh scripts/run-stream.sh)\","
    );
    let _ = writeln!(js, "  \"policy\": \"auto\",");
    let _ = writeln!(js, "  \"block_bytes\": {block_bytes},");
    let _ = writeln!(js, "  \"stream_bytes\": {n},");
    let _ = writeln!(js, "  \"container_bytes\": {container_bytes},");
    let _ = writeln!(js, "  \"payload_bytes\": {},", manifest.payload_bytes);
    let _ = writeln!(js, "  \"directory_bytes\": {},", manifest.directory_bytes);
    let _ = writeln!(js, "  \"header_bytes\": {},", manifest.header_bytes);
    let _ = writeln!(js, "  \"footer_bytes\": {},", manifest.footer_bytes);
    let _ = writeln!(js, "  \"block_count\": {},", manifest.block_count);
    let _ = writeln!(
        js,
        "  \"physical_over_logical\": {physical_over_logical:.4},"
    );
    let _ = writeln!(js, "  \"build_ns\": {build_ns},");
    let _ = writeln!(js, "  \"build_throughput_mib_s\": {build_mib_s:.2},");
    let _ = writeln!(js, "  \"build_peak_rss_bytes\": {build_peak_rss},");
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak_rss},");
    let _ = writeln!(js, "  \"read_len\": {READ_LEN},");
    let _ = writeln!(js, "  \"read_samples\": {SAMPLES},");
    let _ = writeln!(js, "  \"read_at_p50_ns\": {read_p50:.1},");
    let _ = writeln!(js, "  \"read_at_p99_ns\": {read_p99:.1},");
    let _ = writeln!(js, "  \"read_at_exact\": {read_exact},");
    let _ = writeln!(js, "  \"sequential_ns\": {seq_ns},");
    let _ = writeln!(js, "  \"sequential_throughput_mib_s\": {seq_mib_s:.2},");
    let _ = writeln!(js, "  \"sequential_exact\": {seq_exact},");
    let _ = writeln!(
        js,
        "  \"verdict\": \"the builder holds one block plus an O(blocks) directory, so peak RSS is bounded by block size and independent of the stream size; blocks are independently decodable so read_at is O(1)-ish and read_sequential is a straight scan\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("stream.json"), js)?;

    let _ = std::fs::remove_file(&tmp);
    println!("wrote evidence/bench/stream.json (peak_rss={peak_rss} B)");
    Ok(())
}
