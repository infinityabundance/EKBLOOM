//! Selective hot-region **byte tiles** vs whole-object `P_32` vs regional
//! transition tables vs raw baselines — under one corpus, one access trace and
//! one byte budget.
//!
//! The corpus is a deterministic 4 MiB object; the hot working set is a 64 KiB
//! region (1024 traced 64-byte regions with decreasing access counts). Every
//! contender is measured with the same hot/cold reads:
//!
//! * **hot** — 4 KiB contiguous reads inside the hot region;
//! * **cold** — single-byte reads far outside it.
//!
//! The tile budget is the *same* budget the whole-object `P_32` shell consumed,
//! so the comparison is "same budget": selective tiles spend bytes only where
//! hot. Output: `evidence/bench/tiles.json`, written by `sh scripts/run-tiles.sh`.

use std::fmt::Write as _;
use std::path::Path;

use ekbloom::{
    adapt_to_workload, create, manifest_hot_regions, read_ranges, verify_deep, AccessTrace,
    Baseline, BaselineKind, DirectoryKind, ManifestationBudget, ManifestationProfile, ReadRequest,
    RegionReader, SweepConfig, TileSet, Workload, WorkloadKind,
};

/// Deterministic corpus size (4 MiB).
const SRC_LEN: usize = 4 * 1024 * 1024;
/// Deterministic corpus seed.
const SRC_SEED: u64 = 0xEB10_0000_C0DE_2026;
/// Traced hot working set (64 KiB).
const HOT_BYTES: u64 = 64 * 1024;
/// Length of each traced region (bytes).
const HOT_REGION_LEN: u64 = 64;
/// Number of traced regions (`HOT_REGIONS * HOT_REGION_LEN == HOT_BYTES`).
const HOT_REGIONS: u64 = HOT_BYTES / HOT_REGION_LEN;
/// Hot read length (bytes); a multiple of every tested tile size.
const WINDOW: u64 = 4096;
/// Number of hot reads timed per contender.
const SAMPLES: usize = 512;
/// Number of cold reads timed per contender.
const COLD_SAMPLES: usize = 64;
/// Cold reads land at/after this offset.
const COLD_BASE: u64 = SRC_LEN as u64 / 2;
/// Tile shifts exercised: 1 KiB, 4 KiB, 16 KiB, 64 KiB.
const TILE_SHIFTS: [u8; 4] = [10, 12, 14, 16];

/// Deterministic LCG corpus (identical generator to the other harnesses).
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

/// `count` deterministic offsets in `[lo, hi)`.
fn lcg_offsets(seed: u64, count: usize, lo: u64, hi: u64) -> Vec<u64> {
    let span = hi.saturating_sub(lo).max(1);
    let mut s = seed;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lo + (s >> 17) % span
        })
        .collect()
}

/// Time one read per `(offset, len)` sample; returns `(p50, p99)` nanoseconds.
fn time_reads(sample: &[(u64, u64)], mut read: impl FnMut(u64, u64) -> usize) -> (f64, f64) {
    let mut durs = Vec::with_capacity(sample.len());
    for &(off, len) in sample {
        let t0 = ekbloom::now_ns();
        let n = read(off, len);
        let t1 = ekbloom::now_ns();
        assert!(n > 0, "empty read at offset {off}");
        durs.push(t1.saturating_sub(t0));
    }
    durs.sort_unstable();
    let p = ekbloom::percentiles(&durs);
    (p.p50, p.p99)
}

/// One measured contender.
struct Row {
    name: String,
    added_bytes: u64,
    directory_bytes: u64,
    hot_p50: f64,
    hot_p99: f64,
    cold_p50: f64,
    hot_read_bytes: u64,
    note: String,
}

impl Row {
    /// Hot-read bytes served per added manifestation byte.
    fn hot_bytes_per_added(&self) -> f64 {
        if self.added_bytes == 0 {
            0.0
        } else {
            self.hot_read_bytes as f64 / self.added_bytes as f64
        }
    }
}

fn main() -> ekbloom::Result<()> {
    let source = lcg_bytes(SRC_SEED, SRC_LEN);
    let n = source.len() as u64;
    let cfg = SweepConfig {
        iterations: 8,
        seed: 0x9E37_79B9_7F4A_7C15,
    };
    let dir = Path::new("target/tiles-tmp");
    std::fs::create_dir_all(dir)?;

    let mut regions = Vec::with_capacity(HOT_REGIONS as usize);
    let mut counts = Vec::with_capacity(HOT_REGIONS as usize);
    for k in 0..HOT_REGIONS {
        regions.push((k * HOT_REGION_LEN, HOT_REGION_LEN));
        counts.push(10_000u64.saturating_sub(k * 5));
    }
    let trace = AccessTrace { regions, counts };

    // Hot reads: 4 KiB windows at 4 KiB-aligned offsets inside the hot region.
    let aligned = HOT_BYTES / WINDOW;
    let hot_offsets: Vec<u64> = (0..SAMPLES)
        .map(|i| (i as u64 % aligned) * WINDOW)
        .collect();
    let hot_sample: Vec<(u64, u64)> = hot_offsets.iter().map(|&o| (o, WINDOW)).collect();
    let cold_offsets = lcg_offsets(0xC01D_0000_0000_0001, COLD_SAMPLES, COLD_BASE, n - 1);
    let cold_sample: Vec<(u64, u64)> = cold_offsets.iter().map(|&o| (o, 1)).collect();

    let root_phys = create(&source, ManifestationProfile::Root)?.physical_bytes();

    // ---- (b) whole-object P_32 output surface --------------------------------
    let mut adaptive = create(&source, ManifestationProfile::Root)?;
    let workload = Workload {
        kind: WorkloadKind::HotSet,
        hot_prefix: Some(HOT_BYTES),
        sample: hot_sample.clone(),
    };
    let areport = adapt_to_workload(&mut adaptive, &workload, &cfg)?;
    let p32_added = areport.added_physical_bytes;
    // The tile contenders get the *same* budget the whole-object shell consumed.
    let budget_cap = root_phys.saturating_add(p32_added);
    let budget = ManifestationBudget {
        maximum_physical_bytes: budget_cap,
    };

    // The whole-object contender is read through the optimised fast path (the
    // same discipline `adapt.rs` and `docs/BASELINES.md` use): the certified path
    // bloom's `hot` array is sliced directly. Each read allocates its own output
    // buffer, exactly like the tile/regional/raw contenders below.
    let adaptive_read = |off: u64, len: u64| -> usize {
        let mut out = Vec::new();
        read_ranges(&adaptive, &[(off, len)], &mut out).expect("adaptive read");
        out.len()
    };
    for _ in 0..2 {
        for &(off, len) in &hot_sample {
            let _ = adaptive_read(off, len);
        }
    }
    let adaptive_hot = time_reads(&hot_sample, adaptive_read);
    let adaptive_cold = time_reads(&cold_sample, adaptive_read);
    let mut rows: Vec<Row> = vec![Row {
        name: "ekbloom whole-object P_32".to_string(),
        added_bytes: p32_added,
        directory_bytes: 0,
        hot_p50: adaptive_hot.0,
        hot_p99: adaptive_hot.1,
        cold_p50: adaptive_cold.0,
        hot_read_bytes: hot_sample.iter().map(|&(_, l)| l).sum(),
        note: format!(
            "existing adaptive shell (path P_32, {}); whole-object output surface",
            areport.built
        ),
    }];

    // ---- (a) selective hot tiles --------------------------------------------
    let mut tile_rows: Vec<Row> = Vec::new();
    for &shift in &TILE_SHIFTS {
        for &kind in &[DirectoryKind::Flat, DirectoryKind::TwoLevelRadix] {
            let mut body = create(&source, ManifestationProfile::Root)?;
            let report = TileSet::manifest_hot_tiles(&mut body, &trace, shift, kind, &budget)?;
            assert!(verify_deep(&body)?.ok, "tiles body must still verify");
            let tiles =
                body.tiles
                    .last()
                    .ok_or_else(|| ekbloom::EkbloomError::InvalidArgument {
                        what: "no tile set was manifested".to_string(),
                    })?;
            assert!(
                tiles.verify_against(&body.core)?,
                "tiles must certify against the machine"
            );
            // Spot-check byte-exactness on hot, boundary and cold ranges.
            for (off, len) in [
                (0u64, 1u64),
                (WINDOW - 1, 2),
                (HOT_BYTES - 3, 7),
                (COLD_BASE + 123, 5),
            ] {
                let got = tiles.read(&body.core, off, len)?;
                assert_eq!(
                    got,
                    &source[off as usize..(off + len) as usize],
                    "tile read ({off},{len}) must be byte-exact"
                );
            }

            for _ in 0..2 {
                for &(off, len) in &hot_sample {
                    let _ = tiles.read(&body.core, off, len)?;
                }
            }
            let hot = time_reads(&hot_sample, |off, len| {
                tiles
                    .read(&body.core, off, len)
                    .map(|v| v.len())
                    .unwrap_or(0)
            });
            let cold = time_reads(&cold_sample, |off, len| {
                tiles
                    .read(&body.core, off, len)
                    .map(|v| v.len())
                    .unwrap_or(0)
            });
            tile_rows.push(Row {
                name: format!(
                    "ekbloom tiles {} KiB {}",
                    1u64 << shift >> 10,
                    kind.as_str()
                ),
                added_bytes: report.added_physical_bytes,
                directory_bytes: report.directory_bytes,
                hot_p50: hot.0,
                hot_p99: hot.1,
                cold_p50: cold.0,
                hot_read_bytes: hot_sample.iter().map(|&(_, l)| l).sum(),
                note: format!(
                    "{} tiles of {} selected; {} tile B + {} dir B",
                    report.tiles_selected,
                    report.tiles_requested,
                    report.tile_bytes,
                    report.directory_bytes
                ),
            });
        }
    }
    rows.extend(tile_rows);

    // ---- (d) previous regional transition table ------------------------------
    let mut regional = create(&source, ManifestationProfile::Root)?;
    let rreport = manifest_hot_regions(&mut regional, &trace, &budget, &cfg)?;
    let rr = RegionReader::new(&regional);
    for _ in 0..2 {
        for &(off, len) in &hot_sample {
            let _ = rr.read(off, len)?;
        }
    }
    let regional_hot = time_reads(&hot_sample, |off, len| {
        rr.read(off, len).map(|v| v.len()).unwrap_or(0)
    });
    let regional_cold = time_reads(&cold_sample, |off, len| {
        rr.read(off, len).map(|v| v.len()).unwrap_or(0)
    });
    rows.push(Row {
        name: "ekbloom regional table (prev)".to_string(),
        added_bytes: rreport.added_physical_bytes,
        directory_bytes: 0,
        hot_p50: regional_hot.0,
        hot_p99: regional_hot.1,
        cold_p50: regional_cold.0,
        hot_read_bytes: hot_sample.iter().map(|&(_, l)| l).sum(),
        note: format!(
            "{} of {} regions certified/materialised at D={}",
            rreport.selected_regions,
            rreport.regions.len(),
            rreport.depth
        ),
    });

    // ---- (c) raw baselines ---------------------------------------------------
    let raw_vec = Baseline::build(BaselineKind::RawVec, &source, dir)?;
    let raw_file = Baseline::build(BaselineKind::RawFile, &source, dir)?;
    for (label, b) in [("raw-vec", &raw_vec), ("raw-file", &raw_file)] {
        let hot = time_reads(&hot_sample, |off, len| {
            b.read(ReadRequest {
                offset: off,
                length: len,
            })
            .map(|v| v.len())
            .unwrap_or(0)
        });
        let cold = time_reads(&cold_sample, |off, len| {
            b.read(ReadRequest {
                offset: off,
                length: len,
            })
            .map(|v| v.len())
            .unwrap_or(0)
        });
        rows.push(Row {
            name: (*label).to_string(),
            added_bytes: 0,
            directory_bytes: 0,
            hot_p50: hot.0,
            hot_p99: hot.1,
            cold_p50: cold.0,
            hot_read_bytes: hot_sample.iter().map(|&(_, l)| l).sum(),
            note: "conventional direct-access baseline".to_string(),
        });
    }

    // ---- report --------------------------------------------------------------
    println!(
        "corpus={n} B hot={HOT_BYTES} B ({} regions x {HOT_REGION_LEN}) window={WINDOW} \
         budget_cap={budget_cap} B",
        trace.regions.len()
    );
    println!(
        "{:<34} {:>11} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "name", "added B", "dir B", "hot p50", "hot p99", "cold p50", "hB/aB"
    );
    for r in &rows {
        println!(
            "{:<34} {:>11} {:>9} {:>9.0} {:>9.0} {:>9.0} {:>8.2}",
            r.name,
            r.added_bytes,
            r.directory_bytes,
            r.hot_p50,
            r.hot_p99,
            r.cold_p50,
            r.hot_bytes_per_added()
        );
    }

    let json = render_json(&trace, budget_cap, root_phys, p32_added, &rows);
    let out = Path::new("evidence/bench/tiles.json");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, json)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn render_json(
    trace: &AccessTrace,
    budget_cap: u64,
    root_phys: u64,
    p32_added: u64,
    rows: &[Row],
) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(s, "  \"corpus_bytes\": {SRC_LEN},");
    let _ = writeln!(s, "  \"corpus_seed\": {SRC_SEED},");
    let _ = writeln!(s, "  \"hot_bytes\": {HOT_BYTES},");
    let _ = writeln!(s, "  \"hot_regions\": {},", trace.regions.len());
    let _ = writeln!(s, "  \"hot_region_len\": {HOT_REGION_LEN},");
    let _ = writeln!(s, "  \"window_bytes\": {WINDOW},");
    let _ = writeln!(s, "  \"samples\": {SAMPLES},");
    let _ = writeln!(s, "  \"cold_samples\": {COLD_SAMPLES},");
    let _ = writeln!(s, "  \"root_physical_bytes\": {root_phys},");
    let _ = writeln!(s, "  \"whole_object_added_bytes\": {p32_added},");
    let _ = writeln!(s, "  \"budget_bytes\": {budget_cap},");
    s.push_str("  \"rows\": [\n");
    for (i, r) in rows.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&r.name));
        let _ = writeln!(s, "      \"added_physical_bytes\": {},", r.added_bytes);
        let _ = writeln!(s, "      \"directory_bytes\": {},", r.directory_bytes);
        let _ = writeln!(s, "      \"hot_ns_p50\": {:.1},", r.hot_p50);
        let _ = writeln!(s, "      \"hot_ns_p99\": {:.1},", r.hot_p99);
        let _ = writeln!(s, "      \"cold_ns_p50\": {:.1},", r.cold_p50);
        let _ = writeln!(s, "      \"hot_read_bytes\": {},", r.hot_read_bytes);
        let _ = writeln!(
            s,
            "      \"hot_bytes_per_added_byte\": {:.4},",
            r.hot_bytes_per_added()
        );
        let _ = writeln!(s, "      \"note\": \"{}\"", json_escape(&r.note));
        if i + 1 == rows.len() {
            s.push_str("    }\n");
        } else {
            s.push_str("    },\n");
        }
    }
    s.push_str("  ]\n}\n");
    s
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
