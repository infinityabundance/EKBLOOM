//! FUSE read-path study: the real end-to-end cost of reading a **fully
//! materialised** object through the read-only `OrFs` mount.
//!
//! Five read paths are compared on the same 2 MiB object:
//!
//! 1. **backing surface** — the mount serves reads by a direct bounded copy out
//!    of the object's logically-aligned output surface (its certified
//!    whole-object path-bloom `hot` array inside the `.ekb`), with no decode and
//!    no cache probe (`ReadMode::Auto`);
//! 2. **mapped surface** — the same, but the backing file is a standalone,
//!    logically-aligned export mapped through the sanctioned safe `MappedSurface`;
//! 3. **materialised dispatch** (`ReadMode::Decode`) — the mount dispatches to
//!    `OrFs::read`, whose engine path serves this object from the path-bloom `hot`
//!    array. This is the row the earlier study called "decode": it is **not** a
//!    decode, it is a materialised-dispatch route, and it is kept here relabelled
//!    so the two are never conflated;
//! 4. **genuine machine decode** (`ReadMode::Machine`) — the mount forces
//!    `OrFs::read_machine`: only the header and the authoritative core are read and
//!    the machine is replayed from `Omega_0`, bypassing every tile/bloom/block/
//!    checkpoint/history structure;
//! 5. **direct raw file** — `std::fs::read` of the logically-aligned export.
//!
//! It also probes FUSE **passthrough** support at runtime and records the honest
//! verdict (unavailable through `fuser` 0.15).
//!
//! Writes `evidence/bench/fuse-perf.json` (run by `sh scripts/container-fuse.sh`).

#[cfg(all(feature = "fuse", target_os = "linux"))]
mod study {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::fs::File;
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use ekbloom::{
        mount, mount_with, probe_passthrough, ManifestationProfile, MountOptions, OpenObject, OrFs,
        ReadMode, ReadRequest,
    };

    /// Deterministic object size (2 MiB).
    const OBJ_BYTES: u64 = 2 * 1024 * 1024;
    /// Deterministic corpus seed (shared with the other harnesses).
    const CORPUS_SEED: u64 = 0xEB10_0000_C0DE_2026;
    /// Small read window measured per sample.
    const WINDOW: u64 = 4096;
    /// Samples for the per-window rows.
    const WINDOW_SAMPLES: usize = 256;
    /// Samples for the (expensive) full-read rows.
    const FULL_SAMPLES: usize = 15;
    /// Samples for the (much more expensive) genuine machine-decode full reads.
    const MACHINE_FULL_SAMPLES: usize = 3;
    /// Samples for the genuine machine-decode window row.
    const MACHINE_WINDOW_SAMPLES: usize = 48;

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

    fn percentile(sorted: &[u64], q: f64) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let rank = (q * sorted.len() as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    #[derive(Clone, Copy)]
    struct Stats {
        p50: u64,
        p99: u64,
        min: u64,
        mean: f64,
    }

    fn stats(mut samples: Vec<u64>) -> Stats {
        samples.sort_unstable();
        let mean = if samples.is_empty() {
            0.0
        } else {
            samples.iter().map(|&v| v as f64).sum::<f64>() / samples.len() as f64
        };
        Stats {
            p50: percentile(&samples, 0.50),
            p99: percentile(&samples, 0.99),
            min: samples.first().copied().unwrap_or(0),
            mean,
        }
    }

    struct FullRow {
        name: &'static str,
        stats: Stats,
        mbps: f64,
    }

    struct WindowRow {
        name: &'static str,
        stats: Stats,
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

    /// Find the concealed committed `.ekb` generation for the single object.
    fn concealed_ekb(root: &Path) -> PathBuf {
        let concealed = root.join(".ekbloom");
        let dir = std::fs::read_dir(&concealed)
            .expect(".ekbloom exists")
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .expect("one concealed object directory")
            .path();
        dir.join("manifests")
    }

    pub fn run() -> ekbloom::Result<()> {
        if std::fs::metadata("/dev/fuse").is_err() {
            eprintln!("SKIP: /dev/fuse is not available; run scripts/container-fuse.sh");
            return Ok(());
        }

        let base = Path::new("target/fuse-perf-tmp");
        let root = base.join("root");
        let mnt_surface = base.join("mnt-surface");
        let mnt_mapped = base.join("mnt-mapped");
        let mnt_decode = base.join("mnt-decode");
        let mnt_machine = base.join("mnt-machine");
        let _ = std::fs::remove_dir_all(base);
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&mnt_surface)?;
        std::fs::create_dir_all(&mnt_mapped)?;
        std::fs::create_dir_all(&mnt_decode)?;
        std::fs::create_dir_all(&mnt_machine)?;

        let corpus = lcg_bytes(CORPUS_SEED, OBJ_BYTES as usize);
        {
            let fs = OrFs::open(&root)?;
            // A fully materialised object: `Path` stores the whole object as a
            // certified path-bloom `hot` array, so it exposes a logically-aligned
            // output surface and a decode would be a single bounded copy.
            fs.create(
                "data/obj.bin",
                &corpus,
                ManifestationProfile::Path { depth: 4 },
            )?;
        }
        let ekb = concealed_ekb(&root);
        let logical = "data/obj.bin";

        // The discovered aligned surface (absolute offset of logical byte 0).
        let surface = {
            let fs = OrFs::open(&root)?;
            fs.aligned_surface(logical)?
                .ok_or_else(|| bad("Path object exposed no aligned surface"))?
        };

        // A standalone, logically-aligned export (logical byte 0 == offset 0): the
        // kind of file FUSE passthrough would need, and the backing file for the
        // `MappedSurface` row.
        let export = base.join("export.bin");
        {
            let fs = OrFs::open(&root)?;
            let exported = fs.read(
                logical,
                ReadRequest {
                    offset: 0,
                    length: OBJ_BYTES,
                },
            )?;
            if exported != corpus {
                return Err(bad("exported bytes do not match the source corpus"));
            }
            std::fs::write(&export, &exported)?;
        }

        // Mount four projections of the same object: backing surface (auto),
        // standalone mapped surface (explicit), materialised dispatch (decode) and
        // genuine machine decode (machine).
        let fs = OrFs::open(&root)?;
        let h_surface = mount(fs, &mnt_surface, false)?;
        let fs = OrFs::open(&root)?;
        let h_mapped = mount_with(
            fs,
            &mnt_mapped,
            false,
            &MountOptions {
                surfaces: HashMap::from([(logical.to_string(), export.clone())]),
                ..MountOptions::default()
            },
        )?;
        let fs = OrFs::open(&root)?;
        let h_decode = mount_with(
            fs,
            &mnt_decode,
            false,
            &MountOptions {
                read_mode: ReadMode::Decode,
                ..MountOptions::default()
            },
        )?;
        let fs = OrFs::open(&root)?;
        let h_machine = mount_with(
            fs,
            &mnt_machine,
            false,
            &MountOptions {
                read_mode: ReadMode::Machine,
                ..MountOptions::default()
            },
        )?;

        let mounted_surface = mnt_surface.join(logical);
        let mounted_mapped = mnt_mapped.join(logical);
        let mounted_decode = mnt_decode.join(logical);
        let mounted_machine = mnt_machine.join(logical);

        // Byte-exactness through every mount before measuring anything.
        for (label, path) in [
            ("surface", &mounted_surface),
            ("mapped", &mounted_mapped),
            ("decode", &mounted_decode),
            ("machine", &mounted_machine),
        ] {
            if std::fs::read(path)? != corpus {
                return Err(bad(&format!("{label} mount did not match the source")));
            }
        }

        // ---- full-object reads (end-to-end, including open) --------------------
        // Warm the kernel page cache for each target before sampling, so the rows
        // are compared on a warm cache rather than by loop order.
        let mut full = Vec::new();

        let _ = std::fs::read(&mounted_surface)?;
        let mut samples = Vec::with_capacity(FULL_SAMPLES);
        for _ in 0..FULL_SAMPLES {
            let t0 = Instant::now();
            let got = std::fs::read(&mounted_surface)?;
            samples.push(t0.elapsed().as_nanos() as u64);
            if got.len() as u64 != OBJ_BYTES {
                return Err(bad("surface full read returned the wrong length"));
            }
        }
        full.push(full_row("fuse backing surface (std::fs::read)", samples));

        let _ = std::fs::read(&mounted_mapped)?;
        let mut samples = Vec::with_capacity(FULL_SAMPLES);
        for _ in 0..FULL_SAMPLES {
            let t0 = Instant::now();
            let got = std::fs::read(&mounted_mapped)?;
            samples.push(t0.elapsed().as_nanos() as u64);
            if got.len() as u64 != OBJ_BYTES {
                return Err(bad("mapped full read returned the wrong length"));
            }
        }
        full.push(full_row("fuse mapped surface (standalone file)", samples));

        let _ = std::fs::read(&mounted_decode)?;
        let mut samples = Vec::with_capacity(FULL_SAMPLES);
        for _ in 0..FULL_SAMPLES {
            let t0 = Instant::now();
            let got = std::fs::read(&mounted_decode)?;
            samples.push(t0.elapsed().as_nanos() as u64);
            if got.len() as u64 != OBJ_BYTES {
                return Err(bad("decode full read returned the wrong length"));
            }
        }
        full.push(full_row(
            "fuse materialised dispatch (ReadMode::Decode)",
            samples,
        ));

        // Genuine authoritative machine reconstruction (much more expensive: the
        // machine is replayed from Omega_0 per request, with no manifested route).
        let _ = std::fs::read(&mounted_machine)?;
        let mut samples = Vec::with_capacity(MACHINE_FULL_SAMPLES);
        for _ in 0..MACHINE_FULL_SAMPLES {
            let t0 = Instant::now();
            let got = std::fs::read(&mounted_machine)?;
            samples.push(t0.elapsed().as_nanos() as u64);
            if got.len() as u64 != OBJ_BYTES {
                return Err(bad("machine full read returned the wrong length"));
            }
        }
        full.push(full_row(
            "fuse genuine machine decode (ReadMode::Machine)",
            samples,
        ));

        let _ = std::fs::read(&export)?;
        let mut samples = Vec::with_capacity(FULL_SAMPLES);
        for _ in 0..FULL_SAMPLES {
            let t0 = Instant::now();
            let got = std::fs::read(&export)?;
            samples.push(t0.elapsed().as_nanos() as u64);
            if got.len() as u64 != OBJ_BYTES {
                return Err(bad("raw full read returned the wrong length"));
            }
        }
        full.push(full_row("raw file (std::fs::read export.bin)", samples));

        // ---- per-window reads ---------------------------------------------------
        let aligned = OBJ_BYTES.saturating_sub(WINDOW);
        let offsets: Vec<u64> = (0..WINDOW_SAMPLES)
            .map(|i| ((i as u64 * 2654435761) % aligned) & !4095)
            .collect();

        let mut window = Vec::new();

        // Backing surface, persistent fd: after the first touch the kernel page
        // cache serves the data.
        {
            let file = File::open(&mounted_surface)?;
            let mut buf = vec![0u8; WINDOW as usize];
            file.read_exact_at(&mut buf, offsets[0])?;
            let mut samples = Vec::with_capacity(offsets.len());
            for &off in &offsets {
                let t0 = Instant::now();
                file.read_exact_at(&mut buf, off)?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            window.push(WindowRow {
                name: "fuse backing surface (persistent fd, warm)",
                stats: stats(samples),
            });
        }

        // Backing surface, reopen per read: the real uncached FUSE request cost.
        {
            let mut buf = vec![0u8; WINDOW as usize];
            let mut samples = Vec::with_capacity(offsets.len());
            for &off in &offsets {
                let t0 = Instant::now();
                let file = File::open(&mounted_surface)?;
                file.read_exact_at(&mut buf, off)?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            window.push(WindowRow {
                name: "fuse backing surface (open per read, uncached)",
                stats: stats(samples),
            });
        }

        // Materialised dispatch, reopen per read: the route the earlier study
        // labelled "decode". For this `Path` object it reads the path-bloom hot
        // array — it does not decode.
        {
            let mut buf = vec![0u8; WINDOW as usize];
            let mut samples = Vec::with_capacity(offsets.len());
            for &off in &offsets {
                let t0 = Instant::now();
                let file = File::open(&mounted_decode)?;
                file.read_exact_at(&mut buf, off)?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            window.push(WindowRow {
                name: "fuse materialised dispatch (ReadMode::Decode)",
                stats: stats(samples),
            });
        }

        // Genuine machine decode, reopen per read: authoritative replay from
        // Omega_0 over the core (no tile/bloom/block/checkpoint/history route).
        {
            let mut buf = vec![0u8; WINDOW as usize];
            let probes: Vec<u64> = offsets
                .iter()
                .copied()
                .take(MACHINE_WINDOW_SAMPLES)
                .collect();
            let mut samples = Vec::with_capacity(probes.len());
            for &off in &probes {
                let t0 = Instant::now();
                let file = File::open(&mounted_machine)?;
                file.read_exact_at(&mut buf, off)?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            window.push(WindowRow {
                name: "fuse genuine machine decode (ReadMode::Machine)",
                stats: stats(samples),
            });
        }

        // Mapped standalone surface, persistent fd (no syscall once resident).
        {
            let file = File::open(&mounted_mapped)?;
            let mut buf = vec![0u8; WINDOW as usize];
            file.read_exact_at(&mut buf, offsets[0])?;
            let mut samples = Vec::with_capacity(offsets.len());
            for &off in &offsets {
                let t0 = Instant::now();
                file.read_exact_at(&mut buf, off)?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            window.push(WindowRow {
                name: "fuse mapped surface (persistent fd, warm)",
                stats: stats(samples),
            });
        }

        // Direct `OpenObject` handle over the same `.ekb`, for reference.
        {
            let object = OpenObject::open(&ekb)?;
            let _ = object.read(ReadRequest {
                offset: offsets[0],
                length: WINDOW,
            })?;
            let mut samples = Vec::with_capacity(offsets.len());
            for &off in &offsets {
                let t0 = Instant::now();
                let got = object.read(ReadRequest {
                    offset: off,
                    length: WINDOW,
                })?;
                samples.push(t0.elapsed().as_nanos() as u64);
                if got.len() as u64 != WINDOW {
                    return Err(bad("direct window read returned the wrong length"));
                }
            }
            window.push(WindowRow {
                name: "direct OpenObject window (persistent)",
                stats: stats(samples),
            });
        }

        // Raw file, persistent fd.
        {
            let file = File::open(&export)?;
            let mut buf = vec![0u8; WINDOW as usize];
            file.read_exact_at(&mut buf, offsets[0])?;
            let mut samples = Vec::with_capacity(offsets.len());
            for &off in &offsets {
                let t0 = Instant::now();
                file.read_exact_at(&mut buf, off)?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            window.push(WindowRow {
                name: "raw file window (persistent fd)",
                stats: stats(samples),
            });
        }

        // ---- capability probes --------------------------------------------------
        let probe = probe_passthrough();
        let io_uring_reason = "fuser 0.15.1 does not negotiate FUSE-over-io_uring (the \
             FUSE_HAS_IO_URING init flag and per-queue uring setup added in Linux 6.14+); no \
             API is exposed, so the mount uses the classic /dev/fuse request loop.";

        // ---- verdict ------------------------------------------------------------
        let f_decode = &full[2].stats;
        let f_machine = &full[3].stats;
        let f_raw = &full[4].stats;
        let w_surface = &window[1].stats;
        let w_decode = &window[2].stats;
        let w_machine = &window[3].stats;
        let w_mapped = &window[4].stats;
        let verdict = format!(
            "CORRECTION: for this fully-materialised `Path` object the earlier 'decode' row measured a \
             MATERIALISED-DISPATCH route (the `.ekb` path-bloom `hot` array served by `OrFs::read`), \
             not decoding: it costs {:.1} us per uncached 4 KiB read and {:.0} us full. Genuine machine \
             decode (`ReadMode::Machine`, authoritative replay from Omega_0 per request with no \
             tile/bloom/block/checkpoint/history route) costs {:.1} us per uncached 4 KiB read \
             ({:.0}x the dispatch) and {:.0} us full. The backing surface stays {:.1} us per uncached \
             4 KiB read; the standalone mapped surface is the resident floor at {:.2} us. Raw file \
             read: {:.0} us full. FUSE passthrough is NOT available through fuser (max ABI {}); \
             kernel {} {}. The `.ekb` is not a valid passthrough backing file (headers/core precede \
             the hot blob), but its path-bloom hot array is logically aligned and is what the backing \
             surface serves.",
            w_decode.p50 as f64 / 1000.0,
            f_decode.p50 as f64 / 1000.0,
            w_machine.p50 as f64 / 1000.0,
            w_machine.p50 as f64 / w_decode.p50.max(1) as f64,
            f_machine.p50 as f64 / 1000.0,
            w_surface.p50 as f64 / 1000.0,
            w_mapped.p50 as f64 / 1000.0,
            f_raw.p50 as f64 / 1000.0,
            probe.fuser_max_abi,
            probe.kernel_release,
            if probe.kernel_supports {
                "has the passthrough ABI"
            } else {
                "predates the passthrough ABI"
            },
        );
        println!(
            "aligned surface: base_offset={} logical_len={}",
            surface.base_offset, surface.logical_len
        );
        println!("full reads:");
        for row in &full {
            println!(
                "  {:<42} p50 {:>10} ns  min {:>10} ns  {:>8.1} MB/s",
                row.name, row.stats.p50, row.stats.min, row.mbps
            );
        }
        println!("window ({WINDOW} B) reads:");
        for row in &window {
            println!(
                "  {:<52} p50 {:>9} ns  p99 {:>9} ns",
                row.name, row.stats.p50, row.stats.p99
            );
        }
        println!("passthrough probe: {probe:?}");
        println!("verdict: {verdict}");

        // ---- evidence ----------------------------------------------------------
        let mut s = String::new();
        s.push_str("{\n");
        let _ = writeln!(s, "  \"fuse_available\": true,");
        let _ = writeln!(
            s,
            "  \"kernel\": \"{}\",",
            json_escape(&probe.kernel_release)
        );
        let _ = writeln!(s, "  \"object_bytes\": {OBJ_BYTES},");
        let _ = writeln!(s, "  \"object_profile\": \"path(D=4)\",");
        let _ = writeln!(s, "  \"window_bytes\": {WINDOW},");
        let _ = writeln!(s, "  \"window_samples\": {WINDOW_SAMPLES},");
        let _ = writeln!(s, "  \"machine_window_samples\": {MACHINE_WINDOW_SAMPLES},");
        let _ = writeln!(
            s,
            "  \"read_modes\": {{\"backing\": \"Auto/backing-surface\", \"materialised_dispatch\": \"Decode\", \"genuine_machine_decode\": \"Machine\"}},"
        );
        s.push_str(
            "  \"correction\": \"The previous 'decode' row (ReadMode::Decode) measured a materialised-dispatch route - the .ekb path-bloom hot array served by OrFs::read - not decoding. ReadMode::Machine is the genuine authoritative machine reconstruction baseline.\",\n",
        );
        let _ = writeln!(
            s,
            "  \"logically_aligned_export\": \"target/fuse-perf-tmp/export.bin\","
        );
        let _ = writeln!(s, "  \"aligned_surface\": {{");
        let _ = writeln!(s, "    \"kind\": \"ekb path-bloom hot array\",");
        let _ = writeln!(s, "    \"base_offset\": {},", surface.base_offset);
        let _ = writeln!(s, "    \"logical_len\": {}", surface.logical_len);
        let _ = writeln!(s, "  }},");
        s.push_str("  \"passthrough\": {\n");
        let _ = writeln!(s, "    \"supported\": {},", probe.available);
        let _ = writeln!(s, "    \"fuser_supports\": {},", probe.fuser_supports);
        let _ = writeln!(s, "    \"fuser_max_abi\": \"{}\",", probe.fuser_max_abi);
        let _ = writeln!(s, "    \"kernel_supports_abi\": {},", probe.kernel_supports);
        let _ = writeln!(s, "    \"reason\": \"{}\",", json_escape(&probe.reason));
        let _ = writeln!(
            s,
            "    \"ekb_is_not_a_valid_backing_file\": \"a .ekb generation has headers/core before the bloom hot blob, so file offset != logical offset; only the certified path-bloom hot array inside it (or a standalone export) is logically aligned\""
        );
        s.push_str("  },\n");
        s.push_str("  \"io_uring_fuse\": {\n");
        let _ = writeln!(s, "    \"supported\": false,");
        let _ = writeln!(s, "    \"reason\": \"{}\"", json_escape(io_uring_reason));
        s.push_str("  },\n");
        s.push_str("  \"full_read\": [\n");
        for (i, row) in full.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"name\": \"{}\",", row.name);
            let _ = writeln!(s, "      \"p50_ns\": {},", row.stats.p50);
            let _ = writeln!(s, "      \"p99_ns\": {},", row.stats.p99);
            let _ = writeln!(s, "      \"min_ns\": {},", row.stats.min);
            let _ = writeln!(s, "      \"mbps\": {:.1}", row.mbps);
            let _ = writeln!(s, "    }}{}", if i + 1 == full.len() { "" } else { "," });
        }
        s.push_str("  ],\n");
        s.push_str("  \"window_read\": [\n");
        for (i, row) in window.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"name\": \"{}\",", row.name);
            let _ = writeln!(s, "      \"p50_ns\": {},", row.stats.p50);
            let _ = writeln!(s, "      \"p99_ns\": {},", row.stats.p99);
            let _ = writeln!(s, "      \"mean_ns\": {:.1}", row.stats.mean);
            let _ = writeln!(s, "    }}{}", if i + 1 == window.len() { "" } else { "," });
        }
        s.push_str("  ],\n");
        let _ = writeln!(s, "  \"verdict\": \"{}\"", json_escape(&verdict));
        s.push_str("}\n");

        let out = std::env::var("EKB_FUSE_PERF_EVIDENCE")
            .unwrap_or_else(|_| "evidence/bench/fuse-perf.json".to_string());
        let out = Path::new(&out);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out, s)?;
        println!("wrote {}", out.display());

        // Unmount (drop) before removing directories.
        drop(h_surface);
        drop(h_mapped);
        drop(h_decode);
        drop(h_machine);
        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    fn full_row(name: &'static str, samples: Vec<u64>) -> FullRow {
        let st = stats(samples);
        let secs = st.mean / 1e9;
        let mbps = if secs > 0.0 {
            (OBJ_BYTES as f64 / (1024.0 * 1024.0)) / secs
        } else {
            0.0
        };
        FullRow {
            name,
            stats: st,
            mbps,
        }
    }

    fn bad(reason: &str) -> ekbloom::EkbloomError {
        ekbloom::EkbloomError::InvalidArgument {
            what: reason.to_string(),
        }
    }
}

fn main() {
    #[cfg(all(feature = "fuse", target_os = "linux"))]
    {
        if let Err(e) = study::run() {
            eprintln!("fuse_read: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(all(feature = "fuse", target_os = "linux")))]
    {
        eprintln!(
            "fuse_read requires the `fuse` feature on Linux: \
             cargo run --release --features fuse --example fuse_read"
        );
    }
}
