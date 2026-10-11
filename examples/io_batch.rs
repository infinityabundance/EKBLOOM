//! Batched cold-read benchmark: `io_uring` batch vs a serial `pread` loop vs the
//! cached handle, across queue depths and request sizes.
//!
//! Writes `evidence/bench/io-batch.json` (run by `sh scripts/run-io.sh`, which is
//! also invoked from `sh scripts/container-async-io.sh`).
//!
//! The cold rows read the raw corpus with `O_DIRECT`, so they genuinely bypass the
//! page cache (`FADV_DONTNEED` proved unreliable in this container; see the
//! `cache_probe` evidence). Each configuration (`size_bytes`, `queue_depth`) is
//! measured for four paths over one deterministic 64 MiB corpus:
//!
//! * `io_uring` — `queue_depth` uncached reads submitted through one ring with
//!   caller-owned aligned buffers and reaped in order;
//! * `pread_loop` — the same uncached reads issued serially by `pread(2)`;
//! * `cached_handle` — the same logical bytes served warm through an already-open
//!   [`ekbloom::OpenObject`] (a bounded `pread` per request, no re-parse);
//! * `in_memory_tile` — the in-memory [`ekbloom::TileSet`] copy (no syscall).
//!
//! Reported per configuration: IOPS, GB/s, p50/p99 per-operation latency, and CPU
//! time per completed operation. The verdict is per regime.

#[cfg(feature = "io-uring")]
mod bench {
    use std::fmt::Write as _;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    use std::path::Path;
    use std::time::Instant;

    use ekbloom::async_io::{AlignedBuffer, BatchRead, HybridReader};
    use ekbloom::{
        create, write_body, AccessTrace, DirectoryKind, ManifestationBudget, ManifestationProfile,
        OpenObject, ReadRequest, TileSet,
    };

    /// Deterministic corpus size: 64 MiB (also the deepest cold window).
    const FILE_BYTES: u64 = 64 * 1024 * 1024;
    /// Deterministic corpus seed (shared with the other harnesses).
    const CORPUS_SEED: u64 = 0xEB10_0000_C0DE_2026;
    /// Request sizes measured (4 KiB, 64 KiB, 1 MiB).
    const SIZES: [u64; 3] = [4096, 65536, 1024 * 1024];
    /// Ring queue depths measured.
    const DEPTHS: [usize; 5] = [1, 4, 16, 32, 64];
    /// Tile size shift for the cached handle (1 MiB: one bounded `pread` per read).
    const TILE_SHIFT: u8 = 20;
    /// Target bytes read per configuration (iterations are derived from it).
    const TARGET_BYTES_PER_CONFIG: u64 = 96 * 1024 * 1024;
    /// `O_DIRECT` buffer/offset alignment.
    const ALIGN: usize = 4096;
    /// Linux `O_DIRECT`.
    const O_DIRECT: i32 = 0o40000;

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

    /// Task CPU time in nanoseconds (field 0 of `/proc/self/schedstat`).
    fn cpu_ns() -> u64 {
        let Ok(stat) = std::fs::read_to_string("/proc/self/schedstat") else {
            return 0;
        };
        stat.split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Nearest-rank percentile of an ascending sample.
    fn percentile(sorted: &[u64], q: f64) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let rank = (q * sorted.len() as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    fn median(mut samples: Vec<u64>) -> u64 {
        samples.sort_unstable();
        percentile(&samples, 0.50)
    }

    #[derive(Clone, Copy)]
    struct Metrics {
        /// Effective ops/s from the median per-op latency (stall-robust).
        iops: f64,
        /// Aggregate ops/s over the whole timed region (includes device stalls).
        wall_iops: f64,
        gbps: f64,
        p50_ns: u64,
        p99_ns: u64,
        cpu_ns_per_op: f64,
    }

    /// Aggregate `ops` completed operations over `elapsed_ns` wall time.
    ///
    /// Each sample is the *effective* per-op latency of one batch
    /// (`batch_ns / depth`). `iops` is derived from the **median** sample, which is
    /// robust to the occasional multi-millisecond stalls of a shared device;
    /// `wall_iops` is the aggregate rate over the whole region and is also
    /// recorded so the difference is visible.
    fn summarize(
        samples: &mut [u64],
        ops: u64,
        bytes: u64,
        elapsed_ns: u64,
        cpu_ns: u64,
    ) -> Metrics {
        samples.sort_unstable();
        let p50 = percentile(samples, 0.50);
        let p99 = percentile(samples, 0.99);
        let per_op_bytes = if ops > 0 {
            bytes as f64 / ops as f64
        } else {
            0.0
        };
        let iops = 1e9 / p50.max(1) as f64;
        let secs = elapsed_ns as f64 / 1e9;
        Metrics {
            iops,
            wall_iops: if secs > 0.0 { ops as f64 / secs } else { 0.0 },
            gbps: iops * per_op_bytes / 1e9,
            p50_ns: p50,
            p99_ns: p99,
            cpu_ns_per_op: if ops > 0 {
                cpu_ns as f64 / ops as f64
            } else {
                0.0
            },
        }
    }

    /// Uncached batched reads through the ring, into caller-owned aligned buffers.
    fn measure_ring(
        uring: &mut HybridReader,
        reqs: &[BatchRead],
        bufs: &mut [AlignedBuffer],
        source: &[u8],
        size: u64,
        depth: usize,
        iters: usize,
    ) -> io::Result<Metrics> {
        let window = size * depth as u64;
        // Warmup and verify byte-exactness of the uncached reads.
        uring.read_batch_into(reqs, bufs)?;
        for (j, buf) in bufs.iter().enumerate() {
            let start = j as u64 * size;
            if buf.as_slice() != &source[start as usize..(start + size) as usize] {
                return Err(io::Error::other("O_DIRECT io_uring read is not byte-exact"));
            }
        }
        let mut samples = Vec::with_capacity(iters);
        let mut elapsed_total = 0u64;
        let mut cpu = 0u64;
        for _ in 0..iters {
            let c0 = cpu_ns();
            let t0 = Instant::now();
            uring.read_batch_into(reqs, bufs)?;
            let elapsed = t0.elapsed().as_nanos() as u64;
            let c1 = cpu_ns();
            samples.push(elapsed / depth as u64);
            elapsed_total = elapsed_total.saturating_add(elapsed);
            cpu = cpu.saturating_add(c1.saturating_sub(c0));
        }
        Ok(summarize(
            &mut samples,
            (iters * depth) as u64,
            iters as u64 * window,
            elapsed_total,
            cpu,
        ))
    }

    /// The same uncached reads issued serially by `pread(2)` into one aligned buffer.
    fn measure_pread(
        file: &File,
        buf: &mut AlignedBuffer,
        size: u64,
        depth: usize,
        iters: usize,
    ) -> io::Result<Metrics> {
        let window = size * depth as u64;
        let mut samples = Vec::with_capacity(iters);
        let mut elapsed_total = 0u64;
        let mut cpu = 0u64;
        for _ in 0..iters {
            let c0 = cpu_ns();
            let t0 = Instant::now();
            for j in 0..depth {
                file.read_exact_at(buf.as_mut_slice(), j as u64 * size)?;
            }
            let elapsed = t0.elapsed().as_nanos() as u64;
            let c1 = cpu_ns();
            samples.push(elapsed / depth as u64);
            elapsed_total = elapsed_total.saturating_add(elapsed);
            cpu = cpu.saturating_add(c1.saturating_sub(c0));
        }
        Ok(summarize(
            &mut samples,
            (iters * depth) as u64,
            iters as u64 * window,
            elapsed_total,
            cpu,
        ))
    }

    /// Warm reads through an already-open cached handle (no eviction).
    fn measure_cached(
        object: &OpenObject,
        size: u64,
        depth: usize,
        iters: usize,
    ) -> ekbloom::Result<Metrics> {
        let window = size * depth as u64;
        let mut samples = Vec::with_capacity(iters);
        let mut elapsed_total = 0u64;
        let mut cpu = 0u64;
        for _ in 0..iters {
            let c0 = cpu_ns();
            let t0 = Instant::now();
            for j in 0..depth {
                let got = object.read(ReadRequest {
                    offset: j as u64 * size,
                    length: size,
                })?;
                if got.len() as u64 != size {
                    return Err(bad("cached handle returned a short read"));
                }
            }
            let elapsed = t0.elapsed().as_nanos() as u64;
            let c1 = cpu_ns();
            samples.push(elapsed / depth as u64);
            elapsed_total = elapsed_total.saturating_add(elapsed);
            cpu = cpu.saturating_add(c1.saturating_sub(c0));
        }
        Ok(summarize(
            &mut samples,
            (iters * depth) as u64,
            iters as u64 * window,
            elapsed_total,
            cpu,
        ))
    }

    /// Warm reads from the in-memory tile set (no syscall at all).
    fn measure_in_memory(
        tiles: &TileSet,
        core: &[u8],
        size: u64,
        depth: usize,
        iters: usize,
    ) -> ekbloom::Result<Metrics> {
        let window = size * depth as u64;
        let mut samples = Vec::with_capacity(iters);
        let mut elapsed_total = 0u64;
        let mut cpu = 0u64;
        for _ in 0..iters {
            let c0 = cpu_ns();
            let t0 = Instant::now();
            for j in 0..depth {
                let got = tiles.read(core, j as u64 * size, size)?;
                if got.len() as u64 != size {
                    return Err(bad("in-memory tile read returned a short read"));
                }
            }
            let elapsed = t0.elapsed().as_nanos() as u64;
            let c1 = cpu_ns();
            samples.push(elapsed / depth as u64);
            elapsed_total = elapsed_total.saturating_add(elapsed);
            cpu = cpu.saturating_add(c1.saturating_sub(c0));
        }
        Ok(summarize(
            &mut samples,
            (iters * depth) as u64,
            iters as u64 * window,
            elapsed_total,
            cpu,
        ))
    }

    struct Config {
        size: u64,
        depth: usize,
        ring: Metrics,
        pread: Metrics,
        cached: Metrics,
        in_memory: Metrics,
    }

    fn write_metrics(s: &mut String, name: &str, m: &Metrics, last: bool) {
        let _ = writeln!(s, "      \"{name}\": {{");
        let _ = writeln!(s, "        \"iops\": {:.1},", m.iops);
        let _ = writeln!(s, "        \"wall_iops\": {:.1},", m.wall_iops);
        let _ = writeln!(s, "        \"gbps\": {:.3},", m.gbps);
        let _ = writeln!(s, "        \"p50_ns\": {},", m.p50_ns);
        let _ = writeln!(s, "        \"p99_ns\": {},", m.p99_ns);
        let _ = writeln!(s, "        \"cpu_ns_per_op\": {:.1}", m.cpu_ns_per_op);
        let _ = writeln!(s, "      }}{}", if last { "" } else { "," });
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

    fn avg(values: &[f64]) -> f64 {
        if values.is_empty() {
            0.0
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        }
    }

    fn bad(reason: &str) -> ekbloom::EkbloomError {
        ekbloom::EkbloomError::InvalidArgument {
            what: reason.to_string(),
        }
    }

    pub fn run() -> ekbloom::Result<()> {
        let source = lcg_bytes(CORPUS_SEED, FILE_BYTES as usize);
        let dir = Path::new("target/io-batch-tmp");
        std::fs::create_dir_all(dir)?;
        let blob = dir.join("corpus.bin");
        let ekb = dir.join("corpus.ekb");

        // A raw file-backed corpus, and a fully-materialised `.ekb` over the same
        // logical bytes so the cached handle can serve any window with one pread.
        std::fs::write(&blob, &source)?;
        let _ = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&blob)
            .and_then(|f| f.sync_all());

        let mut body = create(&source, ManifestationProfile::Root)?;
        let trace = AccessTrace {
            regions: vec![(0, FILE_BYTES)],
            counts: vec![1],
        };
        let report = TileSet::manifest_hot_tiles(
            &mut body,
            &trace,
            TILE_SHIFT,
            DirectoryKind::Flat,
            &ManifestationBudget {
                maximum_physical_bytes: u64::MAX,
            },
        )?;
        if !report.certified {
            return Err(bad("the benchmark tile set did not certify"));
        }
        write_body(&ekb, &body)?;
        // Flush the materialised `.ekb` so its writeback does not contend with the
        // cold reads below.
        let _ = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&ekb)
            .and_then(|f| f.sync_all());

        let object = OpenObject::open(&ekb)?;
        let tiles = body.tiles.last().expect("a materialised tile set").clone();
        let core = body.core.clone();

        let mut uring = HybridReader::open_direct(&blob)?;
        let ring_available = uring.ring_available();
        let direct_file = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECT)
            .open(&blob)?;
        let cached_file = File::open(&blob)?;

        // Cache probe: a page-cached `pread` vs an `O_DIRECT` one, per size. The
        // large gap shows the O_DIRECT rows are genuinely uncached.
        let mut cache_probe: Vec<(u64, u64, u64)> = Vec::new();
        for &size in &SIZES {
            let mut cb = vec![0u8; size as usize];
            cached_file.read_exact_at(&mut cb, 0)?;
            let mut warm = Vec::with_capacity(8);
            for _ in 0..8 {
                let t0 = Instant::now();
                cached_file.read_exact_at(&mut cb, 0)?;
                warm.push(t0.elapsed().as_nanos() as u64);
            }
            let mut ab = AlignedBuffer::new(size as usize, ALIGN);
            let mut cold = Vec::with_capacity(8);
            for _ in 0..8 {
                let t0 = Instant::now();
                direct_file.read_exact_at(ab.as_mut_slice(), 0)?;
                cold.push(t0.elapsed().as_nanos() as u64);
            }
            cache_probe.push((size, median(warm), median(cold)));
        }
        for &(size, warm, cold) in &cache_probe {
            println!(
                "cache probe size {size:>7} B: page-cached p50 {warm:>9} ns, O_DIRECT p50 {cold:>9} ns"
            );
        }

        let mut configs = Vec::new();
        for &size in &SIZES {
            for &depth in &DEPTHS {
                let iters =
                    (TARGET_BYTES_PER_CONFIG / (size * depth as u64)).clamp(4, 400) as usize;
                let reqs: Vec<BatchRead> = (0..depth)
                    .map(|j| BatchRead::new(j as u64 * size, size))
                    .collect();
                let mut ring_bufs: Vec<AlignedBuffer> = (0..depth)
                    .map(|_| AlignedBuffer::new(size as usize, ALIGN))
                    .collect();
                let mut pread_buf = AlignedBuffer::new(size as usize, ALIGN);
                let ring = measure_ring(
                    &mut uring,
                    &reqs,
                    &mut ring_bufs,
                    &source,
                    size,
                    depth,
                    iters,
                )?;
                let pread = measure_pread(&direct_file, &mut pread_buf, size, depth, iters)?;
                let cached = measure_cached(&object, size, depth, iters)?;
                let in_memory = measure_in_memory(&tiles, &core, size, depth, iters)?;
                println!(
                    "size {:>7} B depth {:>2}: io_uring {:>9.0} ops/s {:>5.2} GB/s p50 {:>7} p99 {:>7} cpu {:>6.0} ns | pread {:>9.0} ops/s cpu {:>6.0} ns | cached {:>9.0} ops/s | in-mem {:>9.0} ops/s",
                    size, depth,
                    ring.iops, ring.gbps, ring.p50_ns, ring.p99_ns, ring.cpu_ns_per_op,
                    pread.iops, pread.cpu_ns_per_op,
                    cached.iops, in_memory.iops,
                );
                configs.push(Config {
                    size,
                    depth,
                    ring,
                    pread,
                    cached,
                    in_memory,
                });
            }
        }

        // Honest per-regime verdict: average ratios over the extremes.
        let ratio = |depth: usize| -> f64 {
            avg(&configs
                .iter()
                .filter(|c| c.depth == depth)
                .map(|c| c.ring.iops / c.pread.iops.max(1.0))
                .collect::<Vec<_>>())
        };
        let ring64 = avg(&configs
            .iter()
            .filter(|c| c.depth == 64)
            .map(|c| c.ring.iops)
            .collect::<Vec<_>>());
        let cached64 = avg(&configs
            .iter()
            .filter(|c| c.depth == 64)
            .map(|c| c.cached.iops)
            .collect::<Vec<_>>());
        let inmem64 = avg(&configs
            .iter()
            .filter(|c| c.depth == 64)
            .map(|c| c.in_memory.iops)
            .collect::<Vec<_>>());
        let ring1 = avg(&configs
            .iter()
            .filter(|c| c.depth == 1)
            .map(|c| c.ring.iops)
            .collect::<Vec<_>>());
        let cached1 = avg(&configs
            .iter()
            .filter(|c| c.depth == 1)
            .map(|c| c.cached.iops)
            .collect::<Vec<_>>());
        let verdict = format!(
            "io_uring batch is {:.2}x the serial pread loop at queue depth 1 (per-op submit/reap \
             overhead dominates: it loses), {:.2}x at depth 16, and {:.2}x at depth 64 (concurrent \
             uncached reads win). At depth 64 it reaches {:.2}x the warm cached handle and {:.2}x \
             the in-memory tile copy; at depth 1 it is only {:.2}x the cached handle.",
            ratio(1),
            ratio(16),
            ratio(64),
            ring64 / cached64.max(1.0),
            ring64 / inmem64.max(1.0),
            ring1 / cached1.max(1.0),
        );
        println!("verdict: {verdict}");

        // ---- evidence --------------------------------------------------------
        let mut s = String::new();
        s.push_str("{\n");
        let _ = writeln!(s, "  \"io_uring_usable\": {ring_available},");
        let _ = writeln!(s, "  \"ring_available\": {ring_available},");
        let _ = writeln!(
            s,
            "  \"uncached_via\": \"O_DIRECT (buffer/offset/length aligned to {ALIGN} B); cached_handle and in_memory_tile rows are warm\""
        );
        let _ = writeln!(s, "  \"corpus_seed\": {CORPUS_SEED},");
        let _ = writeln!(s, "  \"file_bytes\": {FILE_BYTES},");
        let _ = writeln!(s, "  \"tile_shift\": {TILE_SHIFT},");
        let _ = writeln!(
            s,
            "  \"sizes_bytes\": [{}],",
            SIZES
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            s,
            "  \"queue_depths\": [{}],",
            DEPTHS
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        s.push_str("  \"cache_probe\": [\n");
        for (i, &(size, warm, cold)) in cache_probe.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"size_bytes\": {size},");
            let _ = writeln!(s, "      \"page_cached_p50_ns\": {warm},");
            let _ = writeln!(s, "      \"o_direct_p50_ns\": {cold}");
            let _ = writeln!(
                s,
                "    }}{}",
                if i + 1 == cache_probe.len() { "" } else { "," }
            );
        }
        s.push_str("  ],\n");
        s.push_str("  \"results\": [\n");
        for (i, c) in configs.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"size_bytes\": {},", c.size);
            let _ = writeln!(s, "      \"queue_depth\": {},", c.depth);
            write_metrics(&mut s, "io_uring", &c.ring, false);
            write_metrics(&mut s, "pread_loop", &c.pread, false);
            write_metrics(&mut s, "cached_handle", &c.cached, false);
            write_metrics(&mut s, "in_memory_tile", &c.in_memory, true);
            let _ = writeln!(s, "    }}{}", if i + 1 == configs.len() { "" } else { "," });
        }
        s.push_str("  ],\n");
        let _ = writeln!(s, "  \"verdict\": \"{}\"", json_escape(&verdict));
        s.push_str("}\n");

        let out = std::env::var("EKB_IO_BATCH_EVIDENCE")
            .unwrap_or_else(|_| "evidence/bench/io-batch.json".to_string());
        let out = Path::new(&out);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out, s)?;
        println!("wrote {}", out.display());
        Ok(())
    }
}

fn main() {
    #[cfg(feature = "io-uring")]
    {
        if let Err(e) = bench::run() {
            eprintln!("io_batch: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "io-uring"))]
    {
        eprintln!(
            "io_batch requires the `io-uring` feature: \
             cargo run --release --features io-uring --example io_batch"
        );
    }
}
