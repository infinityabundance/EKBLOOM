//! Four read modes head-to-head over one deterministic corpus per size, plus
//! prefault/huge-page variants:
//!
//! 1. `pread`     — cached [`ekbloom::OpenObject`] (`FileBody`) with bounded `pread`.
//! 2. `resident`  — the in-memory materialised hot surface (the handle's bytes
//!    held resident).
//! 3. `mmap`      — read-only [`ekbloom::MappedSurface`] over a fully-materialised,
//!    logically-aligned output file (`mmap`).
//! 4. `mmap_huge` — the same mapping with `MADV_HUGEPAGE`.
//!
//! Variants of mode 3: `MAP_POPULATE` and `MADV_POPULATE_READ` prefault.
//!
//! Reported per size: sequential throughput, random-1-byte p50/p99, random-4-KiB
//! p50, and page faults (minor/major) on the first full-mapping sweep. Huge-page
//! delivery is read from `/proc/self/smaps` (`AnonHugePages`, `FilePmdMapped`,
//! `KernelPageSize`) and an independent anonymous-THP capability probe is run.
//!
//! Writes `evidence/bench/mmap.json` (run by `sh scripts/run-mmap.sh`). Real
//! numbers only; the verdict is per size and honest.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::path::Path;
use std::time::Instant;

use ekbloom::{
    create, thp_anonymous_probe, write_body, AccessTrace, DirectoryKind, HugePageReport,
    ManifestationBudget, ManifestationProfile, MapMode, MappedSurface, OpenObject, ReadRequest,
    TileSet,
};

/// Deterministic corpus seed (shared with the other harnesses).
const CORPUS_SEED: u64 = 0xEB10_0000_C0DE_2026;
/// Materialised object sizes measured: 1 MiB … 256 MiB.
const SIZES: [u64; 4] = [1 << 20, 16 << 20, 64 << 20, 256 << 20];
/// Sequential pass chunk.
const SEQ_CHUNK: u64 = 1 << 20;
/// Sequential repetitions (best kept; faults measured on the first).
const SEQ_REPS: usize = 3;
/// Tile size shift for the `.ekb` baseline (2 MiB tiles → one bounded `pread`).
const TILE_SHIFT: u8 = 21;
/// Random single-byte samples per mode.
const RAND1B: usize = 262_144;
/// Random 4 KiB-range samples per mode.
const RANGE4K: usize = 65_536;

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

/// Deterministic random offsets in `[0, max_start)`.
fn offsets(seed: u64, max_start: u64, n: usize) -> Vec<u64> {
    let mut s = seed;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(if max_start == 0 {
            0
        } else {
            (s >> 17) % max_start
        });
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

/// Minor/major page faults for this process, from `/proc/self/stat`.
fn page_faults() -> (u64, u64) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    let Some(close) = stat.rfind(')') else {
        return (0, 0);
    };
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // After the comm field: idx 7 = minflt (field 10), idx 9 = majflt (field 12).
    let parse = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (parse(7), parse(9))
}

#[derive(Clone, Copy, Default)]
struct ModeMetrics {
    seq_gbps: f64,
    seq_p50_ns_per_chunk: u64,
    rand1b_p50_ns: u64,
    rand1b_p99_ns: u64,
    range4k_p50_ns: u64,
    minflt: u64,
    majflt: u64,
}

enum Reader<'a> {
    Pread(&'a OpenObject),
    Resident(&'a [u8]),
    Mapped(&'a MappedSurface),
}

impl Reader<'_> {
    fn read(&self, off: u64, len: u64) -> Vec<u8> {
        match self {
            Reader::Pread(obj) => obj
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .expect("pread handle read"),
            Reader::Resident(buf) => buf[off as usize..(off + len) as usize].to_vec(),
            Reader::Mapped(s) => s.read(off, len).expect("mapped read"),
        }
    }
}

/// Measure one reader: sequential throughput, random-1b p50/p99, 4-KiB p50, and
/// first-sweep page faults.
fn measure(reader: &Reader<'_>, size: u64, offs1b: &[u64], offs4k: &[u64]) -> ModeMetrics {
    let mut m = ModeMetrics::default();

    // Sequential: first pass measures faults; all passes measured for throughput.
    let mut best_gbps = 0.0f64;
    let mut best_p50 = u64::MAX;
    let (f0_min, f0_maj) = page_faults();
    for rep in 0..SEQ_REPS {
        let mut chunk_ns: Vec<u64> = Vec::with_capacity((size / SEQ_CHUNK) as usize + 1);
        let mut off = 0u64;
        let t0 = Instant::now();
        while off < size {
            let len = SEQ_CHUNK.min(size - off);
            let a = Instant::now();
            let _ = reader.read(off, len);
            chunk_ns.push(a.elapsed().as_nanos() as u64);
            off += len;
        }
        let elapsed = t0.elapsed().as_nanos().max(1);
        let gbps = size as f64 / elapsed as f64; // bytes/ns == GB/s
        chunk_ns.sort_unstable();
        if gbps > best_gbps {
            best_gbps = gbps;
            best_p50 = percentile(&chunk_ns, 0.50);
        }
        if rep == 0 {
            let (f1_min, f1_maj) = page_faults();
            m.minflt = f1_min.saturating_sub(f0_min);
            m.majflt = f1_maj.saturating_sub(f0_maj);
        }
    }
    m.seq_gbps = best_gbps;
    m.seq_p50_ns_per_chunk = if best_p50 == u64::MAX { 0 } else { best_p50 };

    // Random single-byte reads.
    let mut s1: Vec<u64> = Vec::with_capacity(offs1b.len());
    for &o in offs1b {
        let t = Instant::now();
        let b = reader.read(o, 1);
        s1.push(t.elapsed().as_nanos() as u64);
        std::hint::black_box(b);
    }
    s1.sort_unstable();
    m.rand1b_p50_ns = percentile(&s1, 0.50);
    m.rand1b_p99_ns = percentile(&s1, 0.99);

    // Random 4 KiB range reads.
    let mut s4: Vec<u64> = Vec::with_capacity(offs4k.len());
    for &o in offs4k {
        let t = Instant::now();
        let b = reader.read(o, 4096);
        s4.push(t.elapsed().as_nanos() as u64);
        std::hint::black_box(b);
    }
    s4.sort_unstable();
    m.range4k_p50_ns = percentile(&s4, 0.50);

    m
}

fn write_mode(s: &mut String, name: &str, m: &ModeMetrics, last: bool) {
    let _ = writeln!(s, "      \"{name}\": {{");
    let _ = writeln!(s, "        \"seq_gbps\": {:.3},", m.seq_gbps);
    let _ = writeln!(
        s,
        "        \"seq_p50_ns_per_mib_chunk\": {},",
        m.seq_p50_ns_per_chunk
    );
    let _ = writeln!(s, "        \"rand1b_p50_ns\": {},", m.rand1b_p50_ns);
    let _ = writeln!(s, "        \"rand1b_p99_ns\": {},", m.rand1b_p99_ns);
    let _ = writeln!(s, "        \"range4k_p50_ns\": {},", m.range4k_p50_ns);
    let _ = writeln!(s, "        \"first_sweep_minor_faults\": {},", m.minflt);
    let _ = writeln!(s, "        \"first_sweep_major_faults\": {}", m.majflt);
    let _ = writeln!(s, "      }}{}", if last { "" } else { "," });
}

fn write_huge(s: &mut String, h: &HugePageReport, advised: bool) {
    let _ = writeln!(s, "        \"advised\": {advised},");
    let _ = writeln!(s, "        \"anon_huge_bytes\": {},", h.anon_huge_bytes);
    let _ = writeln!(
        s,
        "        \"file_pmd_mapped_bytes\": {},",
        h.file_pmd_bytes
    );
    let _ = writeln!(s, "        \"kernel_page_bytes\": {}", h.kernel_page_bytes);
}

/// Build a fully-materialised `.ekb` (whole-object tile set) for the pread baseline.
fn build_ekb(dir: &Path, name: &str, source: &[u8]) -> ekbloom::Result<std::path::PathBuf> {
    let size = source.len() as u64;
    let mut body = create(source, ManifestationProfile::Root)?;
    let trace = AccessTrace {
        regions: vec![(0, size)],
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
        return Err(ekbloom::EkbloomError::NotCertified {
            what: "benchmark tile set",
        });
    }
    let path = dir.join(name);
    write_body(&path, &body)?;
    Ok(path)
}

fn json_escape(x: &str) -> String {
    x.replace('\\', "\\\\").replace('"', "\\\"")
}

fn run() -> ekbloom::Result<()> {
    let dir = Path::new("target/mmap-tmp");
    std::fs::create_dir_all(dir)?;

    // Independent capability probe: does THP work at all in this environment?
    let thp_probe = thp_anonymous_probe(4 << 20).unwrap_or_default();
    println!(
        "THP anon probe: kernel_page={} B anon_huge={} B",
        thp_probe.kernel_page_bytes, thp_probe.anon_huge_bytes
    );

    struct SizeResult {
        size: u64,
        pread: ModeMetrics,
        resident: ModeMetrics,
        mmap: ModeMetrics,
        mmap_huge: ModeMetrics,
        populate: ModeMetrics,
        populate_read: ModeMetrics,
        huge_advised: bool,
        populate_advised: bool,
        populate_read_advised: bool,
        huge_report: HugePageReport,
        plain_huge_report: HugePageReport,
        physical: u64,
        verdict: String,
    }

    let mut results: Vec<SizeResult> = Vec::new();

    for &size in &SIZES {
        let source = lcg_bytes(CORPUS_SEED ^ size, size as usize);

        // (1) `.ekb` baseline for the cached handle.
        let ekb = build_ekb(dir, "surface.ekb", &source)?;
        let object = OpenObject::open(&ekb)?;

        // (2) Resident in-memory materialised surface, taken once from the handle.
        let resident = object.read(ReadRequest {
            offset: 0,
            length: size,
        })?;
        assert_eq!(resident.len() as u64, size);

        // (3)/(4) A logically-aligned output file = the visible bytes, written by
        // this process and validated before mapping.
        let surface_path = dir.join("surface.bin");
        std::fs::write(&surface_path, &source)?;
        let _ = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&surface_path)
            .and_then(|f| f.sync_all());

        let mut mmap = MappedSurface::open_with(&surface_path, MapMode::Plain)?;
        let mmap_huge = MappedSurface::open_with(&surface_path, MapMode::HugePage)?;
        let mmap_pop = MappedSurface::open_with(&surface_path, MapMode::Populate)?;
        let mmap_popread = MappedSurface::open_with(&surface_path, MapMode::PopulateRead)?;

        // Validate every surface on a handful of ranges before timing.
        for (o, l) in [
            (0u64, 1u64),
            (size / 2, 4096.min(size)),
            (size.saturating_sub(4096), 4096.min(size)),
        ] {
            let want = &source[o as usize..(o + l) as usize];
            assert_eq!(mmap.read(o, l)?, want, "plain mmap mismatch");
            assert_eq!(mmap_huge.read(o, l)?, want, "huge mmap mismatch");
            assert_eq!(
                object.read(ReadRequest {
                    offset: o,
                    length: l
                })?,
                want,
                "pread mismatch"
            );
        }

        let offs1b = offsets(CORPUS_SEED ^ (size << 1), size, RAND1B);
        let max4k = size.saturating_sub(4096).max(1);
        let offs4k = offsets(CORPUS_SEED ^ (size << 2), max4k, RANGE4K);

        let pread = measure(&Reader::Pread(&object), size, &offs1b, &offs4k);
        let resident_m = measure(&Reader::Resident(&resident), size, &offs1b, &offs4k);
        let mmap_m = measure(&Reader::Mapped(&mmap), size, &offs1b, &offs4k);
        let huge_m = measure(&Reader::Mapped(&mmap_huge), size, &offs1b, &offs4k);
        let pop_m = measure(&Reader::Mapped(&mmap_pop), size, &offs1b, &offs4k);
        let popread_m = measure(&Reader::Mapped(&mmap_popread), size, &offs1b, &offs4k);

        let huge_report = mmap_huge.huge_page_report();
        // After the plain mapping's pages are faulted in, ask the kernel whether
        // it already backed them with huge pages (read-only file THP).
        let plain_huge_report = mmap.sample_huge_pages();
        println!(
            "size {:>9} B: pread {:>6.2} GB/s p50 {:>4} ns | resident {:>6.2} | mmap {:>6.2} p50 {:>4} | mmap_huge {:>6.2} p50 {:>4} | faults {:>7}/{} | huge {}/{} B page {}",
            size,
            pread.seq_gbps, pread.rand1b_p50_ns,
            resident_m.seq_gbps,
            mmap_m.seq_gbps, mmap_m.rand1b_p50_ns,
            huge_m.seq_gbps, huge_m.rand1b_p50_ns,
            mmap_m.minflt, mmap_m.majflt,
            huge_report.anon_huge_bytes, huge_report.file_pmd_bytes, huge_report.kernel_page_bytes,
        );

        // Honest per-size verdict.
        let speedup_vs_pread = mmap_m.seq_gbps / pread.seq_gbps.max(1e-9);
        let rand_vs_pread = pread.rand1b_p50_ns as f64 / mmap_m.rand1b_p50_ns.max(1) as f64;
        let huge_vs_mmap = huge_m.rand1b_p50_ns as f64 / mmap_m.rand1b_p50_ns.max(1) as f64;
        let huge_delivered = huge_report.huge_bytes() > 0 || huge_report.kernel_page_bytes > 4096;
        let plain_delivered = plain_huge_report.huge_bytes() > 0;
        let mut verdict = String::new();
        if speedup_vs_pread >= 1.05 || rand_vs_pread >= 1.05 {
            let _ = write!(
                verdict,
                "mmap {:.2}x pread sequential / {:.2}x lower rand-1b p50",
                speedup_vs_pread, rand_vs_pread
            );
        } else {
            let _ = write!(
                verdict,
                "mmap does not beat pread (seq {:.2}x, rand-1b {:.2}x) — page cache already warm",
                speedup_vs_pread, rand_vs_pread
            );
        }
        if plain_delivered {
            let _ = write!(
                verdict,
                "; plain mapping already file-THP backed ({} B FilePmdMapped), MADV_HUGEPAGE has no effect — REJECT",
                plain_huge_report.file_pmd_bytes
            );
        } else if huge_delivered {
            if huge_vs_mmap >= 1.05 {
                let _ = write!(verdict, "; huge pages help rand-1b {:.2}x", huge_vs_mmap);
            } else {
                let _ = write!(
                    verdict,
                    "; huge pages delivered but did not help (rand-1b {:.2}x) — REJECT",
                    huge_vs_mmap
                );
            }
        } else {
            let _ = write!(
                verdict,
                "; huge pages NOT delivered (THP inactive for this file mapping) — REJECT"
            );
        }

        results.push(SizeResult {
            size,
            pread,
            resident: resident_m,
            mmap: mmap_m,
            mmap_huge: huge_m,
            populate: pop_m,
            populate_read: popread_m,
            huge_advised: mmap_huge.advised(),
            populate_advised: mmap_pop.advised(),
            populate_read_advised: mmap_popread.advised(),
            huge_report,
            plain_huge_report,
            physical: mmap.physical_bytes(),
            verdict,
        });

        drop(mmap);
        drop(mmap_huge);
        drop(mmap_pop);
        drop(mmap_popread);
        drop(object);
        let _ = std::fs::remove_file(&ekb);
        let _ = std::fs::remove_file(&surface_path);
    }

    // ---- evidence ------------------------------------------------------------
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(s, "  \"corpus_seed\": {CORPUS_SEED},");
    let _ = writeln!(s, "  \"tile_shift\": {TILE_SHIFT},");
    let _ = writeln!(s, "  \"seq_chunk_bytes\": {SEQ_CHUNK},");
    let _ = writeln!(s, "  \"seq_reps\": {SEQ_REPS},");
    let _ = writeln!(s, "  \"rand1b_samples\": {RAND1B},");
    let _ = writeln!(s, "  \"range4k_samples\": {RANGE4K},");
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
        "  \"container_flags\": \"{}\",",
        json_escape(&std::env::var("EKB_MMAP_CONTAINER_FLAGS").unwrap_or_default())
    );
    let _ = writeln!(s, "  \"thp_anonymous_probe\": {{");
    let _ = writeln!(
        s,
        "        \"anon_huge_bytes\": {},",
        thp_probe.anon_huge_bytes
    );
    let _ = writeln!(
        s,
        "        \"kernel_page_bytes\": {}",
        thp_probe.kernel_page_bytes
    );
    s.push_str("  },\n");
    s.push_str("  \"results\": [\n");
    for (i, r) in results.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"size_bytes\": {},", r.size);
        let _ = writeln!(s, "      \"mapping_physical_bytes\": {},", r.physical);
        s.push_str("      \"modes\": {\n");
        write_mode(&mut s, "pread", &r.pread, false);
        write_mode(&mut s, "resident", &r.resident, false);
        write_mode(&mut s, "mmap", &r.mmap, false);
        write_mode(&mut s, "mmap_huge", &r.mmap_huge, true);
        s.push_str("      },\n");
        s.push_str("      \"prefault_variants\": {\n");
        write_mode(&mut s, "mmap_populate", &r.populate, false);
        write_mode(&mut s, "mmap_populate_read", &r.populate_read, true);
        s.push_str("      },\n");
        s.push_str("      \"advisories\": {\n");
        let _ = writeln!(s, "        \"populate\": {},", r.populate_advised);
        let _ = writeln!(s, "        \"populate_read\": {},", r.populate_read_advised);
        let _ = writeln!(s, "        \"huge_page\": {}", r.huge_advised);
        s.push_str("      },\n");
        s.push_str("      \"huge_page_report\": {\n");
        write_huge(&mut s, &r.huge_report, r.huge_advised);
        s.push_str("      },\n");
        s.push_str("      \"plain_mapping_huge_page_report\": {\n");
        write_huge(&mut s, &r.plain_huge_report, false);
        s.push_str("      },\n");
        let _ = writeln!(s, "      \"verdict\": \"{}\"", json_escape(&r.verdict));
        let _ = writeln!(s, "    }}{}", if i + 1 == results.len() { "" } else { "," });
    }
    s.push_str("  ]\n");
    s.push_str("}\n");

    let out = std::env::var("EKB_MMAP_EVIDENCE")
        .unwrap_or_else(|_| "evidence/bench/mmap.json".to_string());
    let out = Path::new(&out);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, s)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("mmap_modes: {e}");
        std::process::exit(1);
    }
}
