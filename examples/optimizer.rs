//! Joint-planner measurement harness.
//!
//! On a **heterogeneous** corpus (a low-entropy skewed half, a uniform half, and
//! one heavily-accessed hot region) this compares, at **equal physical budget**:
//!
//! * the **joint** optimiser ([`OptimizedPlan::optimize`]) — machine, segment /
//!   region geometry, restart geometry, tile size and hot-region subset chosen
//!   together;
//! * the **greedy** staged selection — machine by core size, then hot tiles
//!   most-accessed-first (the current `manifest_hot_tiles`/region-adapt rule);
//! * **machine selection alone** — machine by core size, no tiles.
//!
//! All three decide from the same **held-out** trace and are scored on a separate
//! **evaluation** trace (never passed to the optimiser), with real wall-clock
//! latency. Writes `evidence/bench/optimizer.json`; run by
//! `sh scripts/run-optimizer.sh`.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use ekbloom::{
    create_machine, select_machine, AccessTrace, CourtConfig, DirectoryKind, Dist, EkbloomBody,
    Engine, ManifestationBudget, ManifestationProfile, OptimizedPlan, PlanReceipt, ReadRequest,
    TileSet, Workload, WorkloadKind,
};

/// Corpus size (4 MiB, well inside the 8 GiB container cap).
const N: usize = 4 * 1024 * 1024;
/// Trace granularity for the spread samples.
const TRACE_REGION: u64 = 16 * 1024;
/// The representative read length used for scoring and measurement.
const READ: u64 = 4096;
/// Total timed reads per measurement (sample sizes are proportional to counts).
const MAX_READS: usize = 3000;
/// Count on the single hot region.
const HOT_COUNT: u64 = 3000;
/// Count on each spread region.
const SPREAD_COUNT: u64 = 20;

fn lcg(seed: u64) -> impl FnMut() -> u8 {
    let mut s = seed;
    move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (s >> 33) as u8
    }
}

/// A heterogeneous corpus: a low-entropy skewed half followed by a uniform half.
fn heterogeneous(seed: u64) -> Vec<u8> {
    let mut next = lcg(seed);
    let half = N / 2;
    (0..N)
        .map(|i| {
            let byte = next();
            if i < half {
                byte & 0x07
            } else {
                byte
            }
        })
        .collect()
}

/// A skewed trace: every `TRACE_REGION` spread sample plus one hot region in the
/// far half.
fn workload_trace(n: u64, salt: u64) -> AccessTrace {
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    let mut pos = 0u64;
    while pos + TRACE_REGION <= n {
        regions.push((pos, TRACE_REGION));
        counts.push(SPREAD_COUNT + (pos.wrapping_add(salt) % 4));
        pos += TRACE_REGION;
    }
    let hot = ((n / 4) * 3).min(n.saturating_sub(TRACE_REGION));
    regions.push((hot, TRACE_REGION.min(n - hot)));
    counts.push(HOT_COUNT);
    AccessTrace { regions, counts }
}

fn cfg() -> CourtConfig {
    CourtConfig {
        reps: 3,
        interleave: true,
        seed: 0x0E71_0000_0000_BEEF,
    }
}

/// Proportional, bounded sample plan: `(offset, length, reps)`.
fn sample_plan(trace: &AccessTrace, n: u64, cap: usize) -> Vec<(u64, u64, u64)> {
    let total: u128 = trace.counts.iter().map(|&c| u128::from(c)).sum();
    if total == 0 {
        return Vec::new();
    }
    let mut plan = Vec::new();
    for (idx, &(start, len)) in trace.regions.iter().enumerate() {
        if start >= n || len == 0 {
            continue;
        }
        let count = trace.counts.get(idx).copied().unwrap_or(0);
        let reps = ((cap as u128 * u128::from(count)) / total).max(1) as u64;
        plan.push((start, len.min(READ).min(n - start).max(1), reps));
    }
    plan
}

/// Measure the evaluation trace against a plan, asserting byte-exactness first.
fn measure(
    engine: &Engine,
    body: &EkbloomBody,
    source: &[u8],
    trace: &AccessTrace,
) -> ekbloom::Result<Dist> {
    let n = source.len() as u64;
    let plan = sample_plan(trace, n, MAX_READS);

    for &(off, len, _) in &plan {
        let req = ReadRequest {
            offset: off,
            length: len,
        };
        let got = engine.read(body, req)?;
        assert_eq!(
            &got,
            &source[off as usize..(off + len) as usize],
            "not byte-exact"
        );
    }
    for &(off, len, _) in &plan {
        let _ = engine.read(
            body,
            ReadRequest {
                offset: off,
                length: len,
            },
        )?;
    }

    let mut samples: Vec<u64> = Vec::new();
    for &(off, len, reps) in &plan {
        for _ in 0..reps {
            let req = ReadRequest {
                offset: off,
                length: len,
            };
            let t0 = Instant::now();
            let _ = engine.read(body, req)?;
            let dt = t0.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            samples.push(dt);
            if samples.len() >= MAX_READS {
                break;
            }
        }
        if samples.len() >= MAX_READS {
            break;
        }
    }
    Ok(Dist::from_samples(&samples, 0x0E71_0000_0000_D157))
}

struct Outcome {
    machine: u32,
    region_bytes: u64,
    committed_bytes: u64,
    dist: Dist,
    certified: bool,
    note: String,
}

struct BudgetResult {
    limit: u64,
    joint: Outcome,
    greedy: Outcome,
    machine_only: Outcome,
    improves: bool,
    receipts: Vec<PlanReceipt>,
}

fn machine_only(source: &[u8], limit: u64) -> ekbloom::Result<(EkbloomBody, Engine)> {
    let machine = select_machine(source)?;
    let body = create_machine(
        source,
        ManifestationProfile::Sparse { interval: 64 },
        machine,
    )?;
    let budget = ManifestationBudget {
        maximum_physical_bytes: limit,
    };
    let workload = Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, READ)],
    };
    let engine = Engine::plan(&body, &budget, &workload)?;
    Ok((body, engine))
}

fn greedy(
    source: &[u8],
    trace: &AccessTrace,
    limit: u64,
) -> ekbloom::Result<(EkbloomBody, Engine)> {
    let machine = select_machine(source)?;
    let mut body = create_machine(
        source,
        ManifestationProfile::Sparse { interval: 64 },
        machine,
    )?;
    let budget = ManifestationBudget {
        maximum_physical_bytes: limit,
    };
    TileSet::manifest_hot_tiles(&mut body, trace, 12, DirectoryKind::TwoLevelRadix, &budget)?;
    let workload = Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, READ)],
    };
    let engine = Engine::plan(&body, &budget, &workload)?;
    Ok((body, engine))
}

fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn finite(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

fn write_outcome(s: &mut String, name: &str, o: &Outcome) {
    s.push_str(&format!("      \"{name}\": {{\n"));
    let _ = writeln!(s, "        \"machine\": {},", o.machine);
    let _ = writeln!(s, "        \"region_bytes\": {},", o.region_bytes);
    let _ = writeln!(s, "        \"committed_bytes\": {},", o.committed_bytes);
    let _ = writeln!(s, "        \"p50_ns\": {:.1},", finite(o.dist.p50));
    let _ = writeln!(s, "        \"p99_ns\": {:.1},", finite(o.dist.p99));
    let _ = writeln!(s, "        \"p999_ns\": {:.1},", finite(o.dist.p999));
    let _ = writeln!(s, "        \"certified\": {},", o.certified);
    let _ = writeln!(s, "        \"note\": \"{}\"", o.note);
    s.push_str("      },\n");
}

fn write_json(
    path: &Path,
    source: &[u8],
    held_out: &AccessTrace,
    evaluation: &AccessTrace,
    results: &[BudgetResult],
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/optimizer.rs (sh scripts/run-optimizer.sh)\","
    );
    let _ = writeln!(s, "  \"source_len\": {},", source.len());
    let _ = writeln!(
        s,
        "  \"source_hash\": \"{}\",",
        hex32(blake3::hash(source).as_bytes())
    );
    let _ = writeln!(s, "  \"read_bytes\": {READ},");
    let _ = writeln!(
        s,
        "  \"held_out_trace_fingerprint\": \"{}\",",
        hex32(&ekbloom::trace_fingerprint(held_out))
    );
    let _ = writeln!(
        s,
        "  \"evaluation_trace_fingerprint\": \"{}\",",
        hex32(&ekbloom::trace_fingerprint(evaluation))
    );
    let _ = writeln!(s, "  \"max_reads\": {MAX_READS},");
    s.push_str("  \"budgets\": [\n");
    for (i, r) in results.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"budget_bytes\": {},", r.limit);
        write_outcome(&mut s, "joint", &r.joint);
        write_outcome(&mut s, "greedy", &r.greedy);
        write_outcome(&mut s, "machine_only", &r.machine_only);
        let _ = writeln!(s, "      \"joint_improves_frontier\": {},", r.improves);
        s.push_str("      \"joint_receipts\": [\n");
        for (j, receipt) in r.receipts.iter().enumerate() {
            s.push_str("        {\n");
            let _ = writeln!(s, "          \"candidate\": \"{}\",", receipt.candidate);
            let _ = writeln!(
                s,
                "          \"committed_bytes\": {},",
                receipt.committed_bytes
            );
            let _ = writeln!(
                s,
                "          \"expected_p99_ns\": {:.1},",
                finite(receipt.expected_latency.p99)
            );
            let _ = writeln!(s, "          \"reason\": \"{}\"", receipt.reason);
            s.push_str(if j + 1 == r.receipts.len() {
                "        }\n"
            } else {
                "        },\n"
            });
        }
        s.push_str("      ]\n");
        s.push_str(if i + 1 == results.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    s.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, s)?;
    Ok(())
}

fn main() -> ekbloom::Result<()> {
    let source = heterogeneous(0x0E71_0000_0000_5EED);
    let n = source.len() as u64;
    let held_out = workload_trace(n, 1);
    let evaluation = workload_trace(n, 0x1234_5678);

    let two_budgets = [n, n + n / 2];
    let mut results = Vec::new();

    for &limit in &two_budgets {
        let budget = ManifestationBudget {
            maximum_physical_bytes: limit,
        };

        // ---- joint -----------------------------------------------------------
        let (plan, receipts) = OptimizedPlan::optimize(&source, &held_out, &budget, &cfg())?;
        let joint_body = plan.build(&source)?;
        let joint_engine = plan.engine(&joint_body)?;
        let joint_dist = measure(&joint_engine, &joint_body, &source, &evaluation)?;
        let joint = Outcome {
            machine: plan.machine_id(),
            region_bytes: plan.region_bytes(),
            committed_bytes: joint_body.physical_bytes(),
            dist: joint_dist,
            certified: joint_engine.plan_ref().all_certified(),
            note: plan.summary(),
        };

        // ---- greedy ----------------------------------------------------------
        let (g_body, g_engine) = greedy(&source, &held_out, limit)?;
        let g_dist = measure(&g_engine, &g_body, &source, &evaluation)?;
        let greedy_out = Outcome {
            machine: g_body.machine_id,
            region_bytes: g_engine.plan_ref().region_bytes(),
            committed_bytes: g_body.physical_bytes(),
            dist: g_dist,
            certified: g_engine.plan_ref().all_certified(),
            note: "machine select + greedy hot tiles (K=64, 4 KiB regions)".to_string(),
        };

        // ---- machine only ----------------------------------------------------
        let (m_body, m_engine) = machine_only(&source, limit)?;
        let m_dist = measure(&m_engine, &m_body, &source, &evaluation)?;
        let machine_out = Outcome {
            machine: m_body.machine_id,
            region_bytes: m_engine.plan_ref().region_bytes(),
            committed_bytes: m_body.physical_bytes(),
            dist: m_dist,
            certified: m_engine.plan_ref().all_certified(),
            note: "machine select only (K=64, no tiles)".to_string(),
        };

        let improves = joint.dist.p99 < greedy_out.dist.p99
            && joint.dist.p99 < machine_out.dist.p99
            && joint.committed_bytes <= limit;

        println!(
            "  budget {:>9} B: joint p99 {:>8.0} ns @ {:>9} B (m{}, R{}) | greedy p99 {:>8.0} ns @ {:>9} B | machine-only p99 {:>8.0} ns @ {:>9} B -> improves={}",
            limit,
            joint.dist.p99,
            joint.committed_bytes,
            joint.machine,
            joint.region_bytes,
            greedy_out.dist.p99,
            greedy_out.committed_bytes,
            machine_out.dist.p99,
            machine_out.committed_bytes,
            improves
        );
        println!("      joint plan: {}", plan.summary());

        results.push(BudgetResult {
            limit,
            joint,
            greedy: greedy_out,
            machine_only: machine_out,
            improves,
            receipts,
        });
    }

    let any = results.iter().any(|r| r.improves);
    println!("joint planner improved the held-out p99 frontier at equal budget: {any}");

    write_json(
        Path::new("evidence/bench/optimizer.json"),
        &source,
        &held_out,
        &evaluation,
        &results,
    )?;
    println!("wrote evidence/bench/optimizer.json");
    Ok(())
}
