//! **Repeated, interleaved multirun court** — distributions and confidence
//! intervals instead of single sweeps.
//!
//! A single sweep reports one number per contender and cannot say whether a
//! difference is real or run noise. This harness upgrades the evidence:
//!
//! * every contender is measured across `reps` **fresh repetitions** (default 15,
//!   up to 31) in a **seed-randomised interleaved order**, so warm-up and drift hit
//!   every contender equally and never a single sweep is reported;
//! * each contender is reported as a **full distribution** — min/p25/p50/p75/p90/
//!   p99/p99.9/max, mean and sample stddev — with **bootstrap 95% CIs for p50 and
//!   p99** ([`ekbloom::Dist`]);
//! * the concurrency arm sweeps **1/2/4/8/16/32/64 readers** with a publishing
//!   writer and reports p50/p99/p99.9 per level, so tail-scaling is visible;
//! * a full **configuration manifest** (`config-manifest.json`) records the CPU
//!   model + flags, kernel, container image id, cgroup/`--cpus`/`--memory` limits,
//!   governor, SMT, page size, RNG seeds, corpus hashes/sizes and the exact command
//!   lines.
//!
//! It writes `evidence/bench/multirun.json` and `evidence/bench/config-manifest.json`
//! and is run inside the pinned, memory-capped container by
//! `sh scripts/run-multirun.sh`. The corpora are the container's **own** real
//! text/ELF files (loaded by `tests/common/e2e.rs`), so nothing large is committed.
//!
//! **Honest limit:** this is a **single host** (one machine, one kernel, one
//! container image). The CIs bound *run-to-run* noise, not host-to-host variation;
//! `docs/BENCHMARKING.md` says so explicitly.

#[path = "../tests/common/corpora.rs"]
mod bench;
#[path = "../tests/common/e2e.rs"]
mod common;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ekbloom::baseline::{Baseline, BaselineKind};
use ekbloom::reader::FileBody;
use ekbloom::writer::write_body;
use ekbloom::{
    collect_config_manifest, create, create_auto, lcg_bytes, manifest, shuffle_order,
    spread_offsets, write_config_manifest_json, AccessTrace, ConfigManifest, CorpusDescriptor,
    CourtConfig, Dist, EkbloomBody, Engine, ManifestationBudget, ManifestationProfile,
    OptimizedPlan, PlannerReader, Publication, ReadRequest, ResidencyCache,
};

/// Reader levels for the concurrency sweep.
const READER_LEVELS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];

/// Resolved multirun configuration (all env-overridable and bounded).
struct Cfg {
    reps: usize,
    cap: usize,
    seed: u64,
    byte_samples: usize,
    conc_reps: usize,
    conc_byte_reads: usize,
    conc_4k_reads: usize,
}

fn env_usize(key: &str, default: usize, lo: usize, hi: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(lo, hi))
        .unwrap_or(default)
}

impl Cfg {
    fn from_env() -> Cfg {
        Cfg {
            reps: env_usize("EKB_MULTIRUN_REPS", 15, 3, 31),
            cap: env_usize("EKB_MULTIRUN_CAP", 1024 * 1024, 64 * 1024, 4 * 1024 * 1024),
            seed: std::env::var("EKB_MULTIRUN_SEED")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0xE8B1_0000_0000_0001),
            byte_samples: env_usize("EKB_MULTIRUN_BYTE_SAMPLES", 256, 32, 4096),
            conc_reps: env_usize("EKB_MULTIRUN_CONC_REPS", 3, 1, 15),
            conc_byte_reads: env_usize("EKB_MULTIRUN_CONC_BYTE_READS", 1000, 128, 40_000),
            conc_4k_reads: env_usize("EKB_MULTIRUN_CONC_4K_READS", 500, 64, 20_000),
        }
    }
}

// ---------------------------------------------------------------------------
// Corpora arm: ekbloom vs raw-vec vs block-indexed LZ4, byte_at latency.
// ---------------------------------------------------------------------------

/// One measurable in-memory contender.
enum Contender {
    Raw(Vec<u8>),
    Ek {
        engine: Engine,
        body: Box<EkbloomBody>,
    },
    Lz4(bench::Store),
}

impl Contender {
    fn byte_at(&mut self, offset: u64) -> u8 {
        match self {
            Contender::Raw(bytes) => bytes[offset as usize],
            Contender::Ek { engine, body } => engine.byte_at(body.as_ref(), offset).unwrap_or(0),
            Contender::Lz4(store) => store.byte_at(offset),
        }
    }
}

struct Built {
    name: &'static str,
    physical_bytes: u64,
    e_v: f64,
    contender: Contender,
}

/// Build the best body the joint optimiser admits under a budget equal to the raw
/// size, falling back to an `auto` base plus a cold block index.
fn build_ekbloom(source: &[u8], budget: u64, seed: u64) -> ekbloom::Result<(EkbloomBody, String)> {
    let n = source.len() as u64;
    let manifest_budget = ManifestationBudget {
        maximum_physical_bytes: budget,
    };
    let cfg = CourtConfig {
        reps: 1,
        interleave: true,
        seed,
    };
    let trace = AccessTrace {
        regions: vec![(0, n.max(1))],
        counts: vec![1000],
    };
    if let Ok((plan, _)) = OptimizedPlan::optimize(source, &trace, &manifest_budget, &cfg) {
        let body = plan.build(source)?;
        return Ok((
            body,
            format!(
                "optimize: machine {} / region {} B / {} committed B",
                plan.machine_id(),
                plan.region_bytes(),
                plan.committed_bytes()
            ),
        ));
    }
    let mut body = create_auto(source, ManifestationProfile::Root)?;
    let block = ekbloom::BlockIndex::build_machine(&body.core, n, 1024, body.machine_id)?;
    body.blocks.push(block);
    body.generation += 1;
    let machine_id = body.machine_id;
    Ok((
        body,
        format!("fallback: machine {machine_id} / auto base + 1024-byte block index"),
    ))
}

fn build_corpus_contenders(
    source: &[u8],
    scratch: &Path,
    seed: u64,
    tag: &str,
) -> ekbloom::Result<Vec<Built>> {
    let n = source.len() as u64;
    let (body, summary) = build_ekbloom(source, n, seed)?;
    let engine = bench::engine_for(&body)?;
    let physical = body.physical_bytes();
    println!(
        "  [{tag}] {summary} ({physical} B, E_V {:.4})",
        physical as f64 / n as f64
    );

    let store = bench::Store::lz4(&scratch.join(format!("{tag}-lz4")), source, 0)?;
    let lz4_physical = store.physical_bytes();

    let mut built = vec![
        Built {
            name: "ekbloom",
            physical_bytes: physical,
            e_v: physical as f64 / n as f64,
            contender: Contender::Ek {
                engine,
                body: Box::new(body),
            },
        },
        Built {
            name: "raw-vec",
            physical_bytes: n,
            e_v: 1.0,
            contender: Contender::Raw(source.to_vec()),
        },
        Built {
            name: "lz4",
            physical_bytes: lz4_physical,
            e_v: lz4_physical as f64 / n as f64,
            contender: Contender::Lz4(store),
        },
    ];

    // Byte-exactness spot check before any timing.
    for c in built.iter_mut() {
        for o in [0u64, n / 3, n / 2, n - 1] {
            assert_eq!(
                c.contender.byte_at(o),
                source[o as usize],
                "{} byte mismatch at {o}",
                c.name
            );
        }
    }
    Ok(built)
}

struct CorpusContender {
    name: String,
    physical_bytes: u64,
    e_v: f64,
    samples: u64,
    byte_at: Dist,
}

struct CorpusResult {
    name: String,
    kind: String,
    bytes: u64,
    blake3: String,
    budget_bytes: u64,
    contenders: Vec<CorpusContender>,
}

fn trial_corpus(
    name: &str,
    kind: &str,
    source: &[u8],
    scratch: &Path,
    cfg: &Cfg,
) -> ekbloom::Result<CorpusResult> {
    let n = source.len() as u64;
    let mut built = build_corpus_contenders(source, scratch, cfg.seed, name)?;
    let offsets = spread_offsets(cfg.seed ^ 0x0FF5_E700, cfg.byte_samples, n, 1);
    let mut pools: Vec<Vec<u64>> = vec![Vec::new(); built.len()];

    for rep in 0..cfg.reps {
        let order = shuffle_order(
            cfg.seed ^ (rep as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x7E57_0001,
            built.len(),
        );
        for &ci in &order {
            let c = &mut built[ci];
            for &o in &offsets {
                let t0 = Instant::now();
                let b = c.contender.byte_at(o);
                let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                std::hint::black_box(b);
                pools[ci].push(dt);
            }
        }
    }

    let contenders = built
        .iter()
        .enumerate()
        .map(|(ci, c)| CorpusContender {
            name: c.name.to_string(),
            physical_bytes: c.physical_bytes,
            e_v: c.e_v,
            samples: pools[ci].len() as u64,
            byte_at: Dist::from_samples(&pools[ci], cfg.seed ^ 0xB007_0000 ^ ci as u64),
        })
        .collect();
    Ok(CorpusResult {
        name: name.to_string(),
        kind: kind.to_string(),
        bytes: n,
        blake3: common::blake3_hex(source),
        budget_bytes: n,
        contenders,
    })
}

// ---------------------------------------------------------------------------
// File-backed cold arm: raw pread vs the `.ekb` reader, plus bytes fetched.
// ---------------------------------------------------------------------------

struct FileContender {
    name: String,
    physical_bytes: u64,
    byte_at_1b: Dist,
    read_4k: Dist,
}

struct FileBackedResult {
    bytes: u64,
    blake3: String,
    contenders: Vec<FileContender>,
    fetched_bytes_per_4k: Dist,
    encoded_bytes_per_4k: Dist,
    syscalls_per_4k: Dist,
}

fn trial_file_backed(cfg: &Cfg) -> ekbloom::Result<FileBackedResult> {
    let dir = std::env::temp_dir().join(format!("ekbloom-multirun-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    let w = 4 * 1024 * 1024usize;
    let source = lcg_bytes(cfg.seed ^ 0xF11E_4EAD, w);
    let n = source.len() as u64;

    let body = create(&source, ManifestationProfile::Path { depth: 32 })?;
    let ekb_path = dir.join("multirun.ekb");
    write_body(&ekb_path, &body)?;
    let mut fbody = FileBody::open(&ekb_path)?;
    fbody.set_bounded_windows(true);
    let rawfile = Baseline::build(BaselineKind::RawFile, &source, &dir)?;

    let byte_offsets = spread_offsets(cfg.seed ^ 0xF11E_0001, cfg.byte_samples, n, 1);
    let read_offsets = spread_offsets(cfg.seed ^ 0xF11E_0002, cfg.byte_samples.min(512), n, 4096);

    for &o in read_offsets.iter().take(4) {
        let got = fbody.read(ReadRequest {
            offset: o,
            length: 4096,
        })?;
        assert_eq!(
            got,
            &source[o as usize..(o + 4096) as usize],
            "ekb cold 4k mismatch"
        );
    }

    let mut one_pools: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut four_pools: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut fetched: Vec<u64> = Vec::new();
    let mut encoded: Vec<u64> = Vec::new();
    let mut syscalls: Vec<u64> = Vec::new();

    for rep in 0..cfg.reps {
        let order = shuffle_order(
            cfg.seed ^ (rep as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xF11E_0010,
            2,
        );
        for &ci in &order {
            for &o in &byte_offsets {
                let mut out = Vec::new();
                let t0 = Instant::now();
                if ci == 0 {
                    rawfile.read_into(
                        ReadRequest {
                            offset: o,
                            length: 1,
                        },
                        &mut out,
                    )?;
                } else {
                    fbody.read_into(
                        ReadRequest {
                            offset: o,
                            length: 1,
                        },
                        &mut out,
                    )?;
                }
                let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                std::hint::black_box(out);
                one_pools[ci].push(dt);
            }
            for &o in &read_offsets {
                let mut out = Vec::new();
                if ci == 0 {
                    let t0 = Instant::now();
                    rawfile.read_into(
                        ReadRequest {
                            offset: o,
                            length: 4096,
                        },
                        &mut out,
                    )?;
                    let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    std::hint::black_box(out);
                    four_pools[ci].push(dt);
                } else {
                    let b0 = fbody.fetched_bytes();
                    let e0 = fbody.fetched_encoded_bytes();
                    let s0 = fbody.fetched_reads();
                    let t0 = Instant::now();
                    fbody.read_into(
                        ReadRequest {
                            offset: o,
                            length: 4096,
                        },
                        &mut out,
                    )?;
                    let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                    std::hint::black_box(out);
                    four_pools[ci].push(dt);
                    fetched.push(fbody.fetched_bytes() - b0);
                    encoded.push(fbody.fetched_encoded_bytes() - e0);
                    syscalls.push(fbody.fetched_reads() - s0);
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    let contenders = vec![
        FileContender {
            name: "raw-file".to_string(),
            physical_bytes: rawfile.physical_bytes(),
            byte_at_1b: Dist::from_samples(&one_pools[0], cfg.seed ^ 0xF11E_0100),
            read_4k: Dist::from_samples(&four_pools[0], cfg.seed ^ 0xF11E_0200),
        },
        FileContender {
            name: "ekbloom-file".to_string(),
            physical_bytes: fbody.physical_len(),
            byte_at_1b: Dist::from_samples(&one_pools[1], cfg.seed ^ 0xF11E_0101),
            read_4k: Dist::from_samples(&four_pools[1], cfg.seed ^ 0xF11E_0201),
        },
    ];
    Ok(FileBackedResult {
        bytes: n,
        blake3: common::blake3_hex(&source),
        contenders,
        fetched_bytes_per_4k: Dist::from_samples(&fetched, cfg.seed ^ 0xF11E_0300),
        encoded_bytes_per_4k: Dist::from_samples(&encoded, cfg.seed ^ 0xF11E_0500),
        syscalls_per_4k: Dist::from_samples(&syscalls, cfg.seed ^ 0xF11E_0400),
    })
}

// ---------------------------------------------------------------------------
// Concurrency arm: 1/2/4/8/16/32/64 readers with a publishing writer.
// ---------------------------------------------------------------------------

/// Build a fresh certified generation (root + sparse checkpoints).
fn build_generation(source: &[u8], generation: u64) -> ekbloom::Result<EkbloomBody> {
    let mut body = create(source, ManifestationProfile::Root)?;
    manifest(&mut body, ManifestationProfile::Sparse { interval: 256 })?;
    body.generation = generation;
    Ok(body)
}

/// One busy phase: `readers` threads read while a publisher continuously swaps
/// fresh certified generations. Returns the pooled per-read samples, the
/// publications built and the O(publications) shared-lock acquisitions.
fn concurrency_phase(
    publication: &Arc<Publication>,
    source: &[u8],
    length: u64,
    readers: usize,
    reads: usize,
    seed: u64,
) -> (Vec<u64>, u64, u64) {
    let n = source.len() as u64;
    let span = n.saturating_sub(length).max(1);
    let stop = Arc::new(AtomicBool::new(false));
    let locks_before = publication.reader_lock_acquisitions();

    let publisher = {
        let publication = Arc::clone(publication);
        let stop = Arc::clone(&stop);
        let owned = source.to_vec();
        std::thread::spawn(move || -> u64 {
            let mut generation = 1u64;
            let mut built = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(body) = build_generation(&owned, generation) {
                    if publication.publish(body, ResidencyCache::default()).is_ok() {
                        built += 1;
                    }
                }
                generation += 1;
            }
            built
        })
    };

    let mut handles = Vec::with_capacity(readers);
    for r in 0..readers {
        let publication = Arc::clone(publication);
        let src = source.to_vec();
        handles.push(std::thread::spawn(move || -> Vec<u64> {
            let mut reader = PlannerReader::new(Arc::clone(&publication));
            let mut s = seed ^ ((r as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut samples = Vec::with_capacity(reads);
            let mut mismatches = 0usize;
            for _ in 0..reads {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let off = (s >> 17) % span;
                let t0 = Instant::now();
                reader.refresh();
                let got = reader
                    .read(ReadRequest {
                        offset: off,
                        length,
                    })
                    .expect("read");
                let dt = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                if length == 1 {
                    if got.first().copied() != Some(src[off as usize]) {
                        mismatches += 1;
                    }
                } else if got != src[off as usize..(off + length) as usize] {
                    mismatches += 1;
                }
                samples.push(dt);
            }
            assert_eq!(
                mismatches, 0,
                "reader decoded wrong bytes under publication"
            );
            samples
        }));
    }

    let mut pooled = Vec::with_capacity(readers * reads);
    for h in handles {
        pooled.extend(h.join().expect("reader thread panicked"));
    }
    stop.store(true, Ordering::Relaxed);
    let publications = publisher.join().expect("publisher thread panicked");
    let locks = publication.reader_lock_acquisitions() - locks_before;
    (pooled, publications, locks)
}

struct ConcResult {
    readers: usize,
    byte_at: Dist,
    reads_4k: Dist,
    publications: u64,
    locks_per_read: f64,
}

fn sweep_concurrency(cfg: &Cfg, seed: u64) -> ekbloom::Result<Vec<ConcResult>> {
    let source = lcg_bytes(seed ^ 0xC0C0_0001, 1024 * 1024);
    let publication = Publication::new(build_generation(&source, 0)?)?;
    let levels = READER_LEVELS;
    let mut byte_pools: Vec<Vec<u64>> = vec![Vec::new(); levels.len()];
    let mut read_pools: Vec<Vec<u64>> = vec![Vec::new(); levels.len()];
    let mut pubs = vec![0u64; levels.len()];
    let mut locks = vec![0u64; levels.len()];
    let mut reads_done = vec![0u64; levels.len()];

    for rep in 0..cfg.conc_reps {
        let order = shuffle_order(
            seed ^ (rep as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03) ^ 0xC0C0,
            levels.len(),
        );
        for &li in &order {
            let readers = levels[li];
            let (b, p, l) = concurrency_phase(
                &publication,
                &source,
                1,
                readers,
                cfg.conc_byte_reads,
                seed ^ 0xB17E_0000 ^ li as u64,
            );
            byte_pools[li].extend(b);
            pubs[li] += p;
            locks[li] += l;
            reads_done[li] += (readers * cfg.conc_byte_reads) as u64;

            let (r, p2, l2) = concurrency_phase(
                &publication,
                &source,
                4096,
                readers,
                cfg.conc_4k_reads,
                seed ^ 0x4B1E_0000 ^ li as u64,
            );
            read_pools[li].extend(r);
            pubs[li] += p2;
            locks[li] += l2;
            reads_done[li] += (readers * cfg.conc_4k_reads) as u64;
        }
        println!("  concurrency repetition {}/{}", rep + 1, cfg.conc_reps);
    }

    let results = levels
        .iter()
        .enumerate()
        .map(|(li, &readers)| ConcResult {
            readers,
            byte_at: Dist::from_samples(&byte_pools[li], seed ^ 0xC0C0_0000 ^ li as u64),
            reads_4k: Dist::from_samples(&read_pools[li], seed ^ 0xC0C0_1000 ^ li as u64),
            publications: pubs[li],
            locks_per_read: locks[li] as f64 / reads_done[li].max(1) as f64,
        })
        .collect();
    Ok(results)
}

// ---------------------------------------------------------------------------
// Reporting.
// ---------------------------------------------------------------------------

fn print_corpus(r: &CorpusResult) {
    println!(
        "  {} ({} B, blake3 {})",
        r.name,
        r.bytes,
        &r.blake3[..16.min(r.blake3.len())]
    );
    for c in &r.contenders {
        println!(
            "    {:<8} E_V={:.4} n={} p50={:.0} [{:.0}, {:.0}] p99={:.0} [{:.0}, {:.0}]",
            c.name,
            c.e_v,
            c.samples,
            c.byte_at.p50,
            c.byte_at.ci50_lo,
            c.byte_at.ci50_hi,
            c.byte_at.p99,
            c.byte_at.ci99_lo,
            c.byte_at.ci99_hi
        );
    }
}

fn print_file(r: &FileBackedResult) {
    println!(
        "  cold reads ({} B, blake3 {})",
        r.bytes,
        &r.blake3[..16.min(r.blake3.len())]
    );
    for c in &r.contenders {
        println!(
            "    {:<12} 1B p50={:.0} [{:.0}, {:.0}]  4k p50={:.0} [{:.0}, {:.0}]  p99={:.0}",
            c.name,
            c.byte_at_1b.p50,
            c.byte_at_1b.ci50_lo,
            c.byte_at_1b.ci50_hi,
            c.read_4k.p50,
            c.read_4k.ci50_lo,
            c.read_4k.ci50_hi,
            c.read_4k.p99
        );
    }
    println!(
        "    fetched bytes / 4k: p50={:.0} [{:.0}, {:.0}] max={:.0}  encoded/4k p50={:.0}  syscalls/4k p50={:.0}",
        r.fetched_bytes_per_4k.p50,
        r.fetched_bytes_per_4k.ci50_lo,
        r.fetched_bytes_per_4k.ci50_hi,
        r.fetched_bytes_per_4k.max,
        r.encoded_bytes_per_4k.p50,
        r.syscalls_per_4k.p50
    );
}

fn print_conc(c: &ConcResult) {
    println!(
        "    readers={:<2} byte_at p50={:.0} p99={:.0} p99.9={:.0}  4k p50={:.0} p99={:.0} p99.9={:.0}  pubs={} locks/read={:.2e}",
        c.readers,
        c.byte_at.p50,
        c.byte_at.p99,
        c.byte_at.p999,
        c.reads_4k.p50,
        c.reads_4k.p99,
        c.reads_4k.p999,
        c.publications,
        c.locks_per_read
    );
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn write_multirun_json(
    dir: &Path,
    cfg: &Cfg,
    corpora: &[CorpusResult],
    file: &FileBackedResult,
    conc: &[ConcResult],
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/multirun.rs (sh scripts/run-multirun.sh)\","
    );
    let _ = writeln!(
        s,
        "  \"method\": \"each contender is measured over `reps` fresh repetitions in a seed-randomised interleaved order; every latency/byte-count row is the pooled per-operation sample reported as a full distribution (min/p25/p50/p75/p90/p99/p99.9/max, mean, stddev) with a deterministic bootstrap 95% CI for p50 and p99; the concurrency arm sweeps 1/2/4/8/16/32/64 readers against a publishing writer; a full configuration manifest is in config-manifest.json\","
    );
    let _ = writeln!(
        s,
        "  \"config\": {{\"reps\": {}, \"interleave\": true, \"seed\": \"0x{:016X}\", \"cap_bytes\": {}, \"byte_samples\": {}, \"conc_reps\": {}, \"reader_levels\": [1, 2, 4, 8, 16, 32, 64]}},",
        cfg.reps, cfg.seed, cfg.cap, cfg.byte_samples, cfg.conc_reps
    );

    s.push_str("  \"corpora\": [\n");
    for (i, c) in corpora.iter().enumerate() {
        s.push_str("    {\n");
        let _ = writeln!(s, "      \"name\": \"{}\",", json_escape(&c.name));
        let _ = writeln!(s, "      \"kind\": \"{}\",", json_escape(&c.kind));
        let _ = writeln!(s, "      \"bytes\": {},", c.bytes);
        let _ = writeln!(s, "      \"blake3\": \"{}\",", c.blake3);
        let _ = writeln!(s, "      \"budget_bytes\": {},", c.budget_bytes);
        s.push_str("      \"contenders\": [\n");
        for (j, r) in c.contenders.iter().enumerate() {
            let _ = write!(
                s,
                "        {{\"name\": \"{}\", \"physical_bytes\": {}, \"e_v\": {:.4}, \"samples\": {}, \"byte_at\": {}}}",
                json_escape(&r.name),
                r.physical_bytes,
                r.e_v,
                r.samples,
                r.byte_at.json()
            );
            s.push_str(if j + 1 == c.contenders.len() {
                "\n"
            } else {
                ",\n"
            });
        }
        s.push_str("      ]\n    }");
        s.push_str(if i + 1 == corpora.len() { "\n" } else { ",\n" });
    }
    s.push_str("  ],\n");

    s.push_str("  \"file_backed_cold\": {\n");
    let _ = writeln!(s, "    \"bytes\": {},", file.bytes);
    let _ = writeln!(s, "    \"blake3\": \"{}\",", file.blake3);
    s.push_str("    \"contenders\": [\n");
    for (i, c) in file.contenders.iter().enumerate() {
        let _ = write!(
            s,
            "      {{\"name\": \"{}\", \"physical_bytes\": {}, \"byte_at_1b\": {}, \"read_4k\": {}}}",
            json_escape(&c.name),
            c.physical_bytes,
            c.byte_at_1b.json(),
            c.read_4k.json()
        );
        s.push_str(if i + 1 == file.contenders.len() {
            "\n"
        } else {
            ",\n"
        });
    }
    s.push_str("    ],\n");
    let _ = writeln!(
        s,
        "    \"fetched_bytes_per_4k\": {},",
        file.fetched_bytes_per_4k.json()
    );
    let _ = writeln!(
        s,
        "    \"encoded_bytes_per_4k\": {},",
        file.encoded_bytes_per_4k.json()
    );
    let _ = writeln!(
        s,
        "    \"syscalls_per_4k\": {}",
        file.syscalls_per_4k.json()
    );
    s.push_str("  },\n");

    s.push_str("  \"concurrency\": [\n");
    for (i, c) in conc.iter().enumerate() {
        let _ = write!(
            s,
            "    {{\"readers\": {}, \"byte_at\": {}, \"reads_4k\": {}, \"publications\": {}, \"locks_per_read\": {:.3e}}}",
            c.readers,
            c.byte_at.json(),
            c.reads_4k.json(),
            c.publications,
            c.locks_per_read
        );
        s.push_str(if i + 1 == conc.len() { "\n" } else { ",\n" });
    }
    s.push_str("  ],\n");

    let _ = writeln!(
        s,
        "  \"notes\": \"single host (one machine/kernel/image): the CIs bound run-to-run noise, not host-to-host variation; E_V is a deterministic ratio given corpus+budget (its sampling CI width is 0); see docs/BENCHMARKING.md\""
    );
    s.push_str("}\n");

    std::fs::write(dir.join("multirun.json"), s)?;
    Ok(())
}

fn main() -> ekbloom::Result<()> {
    let cfg = Cfg::from_env();
    println!(
        "multirun: reps={} (interleaved, seeded 0x{:016X}), cap={} B, byte_samples={}, conc_reps={}",
        cfg.reps, cfg.seed, cfg.cap, cfg.byte_samples, cfg.conc_reps
    );

    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let scratch =
        std::env::temp_dir().join(format!("ekbloom-multirun-scratch-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;

    // ---- corpora: the container's own real text/ELF data ----
    let own = common::load(common::Caps {
        text: cfg.cap,
        elf: cfg.cap,
        mixed: 0,
    });
    let mut corpora: Vec<CorpusResult> = Vec::new();
    let mut descriptors: Vec<CorpusDescriptor> = Vec::new();
    for c in own.iter() {
        if c.name != "text" && c.name != "elf" {
            continue;
        }
        println!(
            "== corpus {} ({} B, cap {}) ==",
            c.name,
            c.source.len(),
            cfg.cap
        );
        let r = trial_corpus(c.name, c.kind, &c.source, &scratch, &cfg)?;
        print_corpus(&r);
        descriptors.push(CorpusDescriptor {
            name: c.name.to_string(),
            kind: c.kind.to_string(),
            bytes: c.source.len() as u64,
            blake3: common::blake3_hex(&c.source),
            seed: "measured-from-container".to_string(),
        });
        corpora.push(r);
    }
    let _ = std::fs::remove_dir_all(&scratch);

    // ---- file-backed cold ----
    println!("== file-backed cold: raw pread vs .ekb reader (4 MiB) ==");
    let file = trial_file_backed(&cfg)?;
    print_file(&file);
    descriptors.push(CorpusDescriptor {
        name: "synthetic-lcg".to_string(),
        kind: "file-backed-cold".to_string(),
        bytes: file.bytes,
        blake3: file.blake3.clone(),
        seed: format!("0x{:016X}", cfg.seed ^ 0xF11E_4EAD),
    });

    // ---- concurrency sweep ----
    println!("== concurrency: 1/2/4/8/16/32/64 readers with a publisher ==");
    let concurrency = sweep_concurrency(&cfg, cfg.seed)?;
    for c in &concurrency {
        print_conc(c);
    }

    // ---- configuration manifest ----
    let mut manifest: ConfigManifest = collect_config_manifest();
    manifest.rng_seeds = vec![
        ("multirun_seed".to_string(), format!("0x{:016X}", cfg.seed)),
        (
            "byte_offsets_seed".to_string(),
            "multirun_seed ^ 0x0FF5E700 / ^ 0xF11E0001 / ^ 0xF11E0002".to_string(),
        ),
        (
            "concurrency_seed".to_string(),
            format!("0x{:016X}", cfg.seed ^ 0xC0C0_0001),
        ),
    ];
    if let Ok(lock) = std::fs::read("Cargo.lock") {
        manifest
            .rng_seeds
            .push(("cargo_lock_blake3".to_string(), common::blake3_hex(&lock)));
    }
    manifest.corpora = descriptors;
    write_config_manifest_json(&dir.join("config-manifest.json"), &manifest)?;

    write_multirun_json(dir, &cfg, &corpora, &file, &concurrency)?;
    println!(
        "wrote evidence/bench/multirun.json + evidence/bench/config-manifest.json ({} corpora rows)",
        corpora.len()
    );
    Ok(())
}
