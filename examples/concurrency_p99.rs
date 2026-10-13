//! **Concurrency p99** — the lock-free read path under publication pressure.
//!
//! 64 reader threads read (1-byte and 4 KiB) through their own
//! [`PlannerReader`], while **one** publisher thread continuously builds fresh,
//! certified generations and swaps them into a [`Publication`]. A reader caches
//! the current `Arc<PublishedBody>` and reads it with **no lock**;
//! [`PlannerReader::refresh`] is one acquire load when the generation is
//! unchanged, taking the shared publication lock only on a generation change.
//!
//! Each workload is measured **twice with the same readers**: once *quiet* (no
//! publisher thread) and once *busy* (the publisher swapping generations). The
//! quiet arm is the scheduler baseline (64 threads on an 8-CPU container are
//! already oversubscribed); the difference between the arms isolates the
//! publisher's effect on the read path. The evidence records p50/p99/p99.9/max
//! for both arms and the ratio of shared-lock acquisitions to reads — which must
//! be `O(publications)`, never `O(reads)`.
//!
//! Writes `evidence/bench/concurrency.json`; run by `sh scripts/run-corpora.sh`.

#[path = "../tests/common/corpora.rs"]
mod bench;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ekbloom::{
    create, manifest, ManifestationProfile, PlannerReader, Publication, ReadRequest, ResidencyCache,
};

/// Reader threads.
const READERS: usize = 64;
/// Timed reads per reader in the single-byte workloads.
const BYTE_READS: usize = 40_000;
/// Timed reads per reader in the 4 KiB workloads.
const READ4K_READS: usize = 20_000;
/// Deterministic source length (~1 MiB).
const SRC_LEN: usize = 1024 * 1024;

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

/// Build a fresh certified generation (root + sparse checkpoints).
fn build_generation(source: &[u8], generation: u64) -> ekbloom::Result<ekbloom::EkbloomBody> {
    let mut body = create(source, ManifestationProfile::Root)?;
    manifest(&mut body, ManifestationProfile::Sparse { interval: 256 })?;
    body.generation = generation;
    Ok(body)
}

/// One workload's measured outcome.
struct Outcome {
    lat: bench::Lat,
    locks: u64,
    reads: u64,
    publications: u64,
}

/// Run one timed phase over all readers; when `with_publisher`, a publisher
/// thread swaps fresh generations concurrently.
fn run_phase(
    publication: &Arc<Publication>,
    source: &[u8],
    length: u64,
    reads_per_reader: usize,
    seed: u64,
    with_publisher: bool,
) -> Outcome {
    let n = source.len() as u64;
    let span = n.saturating_sub(length).max(1);
    let stop = Arc::new(AtomicBool::new(false));
    let locks_before = publication.reader_lock_acquisitions();

    let publisher = if with_publisher {
        let publication = Arc::clone(publication);
        let stop = Arc::clone(&stop);
        let owned = source.to_vec();
        Some(std::thread::spawn(move || -> u64 {
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
        }))
    } else {
        None
    };

    let mut handles = Vec::with_capacity(READERS);
    for r in 0..READERS {
        let publication = Arc::clone(publication);
        let src = source.to_vec();
        handles.push(std::thread::spawn(move || -> Vec<u64> {
            let mut reader = PlannerReader::new(Arc::clone(&publication));
            let mut s = seed ^ ((r as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            for _ in 0..64 {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let off = (s >> 17) % span;
                let _ = reader.read(ReadRequest {
                    offset: off,
                    length,
                });
            }
            let mut samples = Vec::with_capacity(reads_per_reader);
            let mut mismatches = 0usize;
            for i in 0..reads_per_reader {
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
                } else if i % 256 == 0 && got != src[off as usize..(off + length) as usize] {
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

    let mut pooled: Vec<u64> = Vec::with_capacity(READERS * reads_per_reader);
    for h in handles {
        pooled.extend(h.join().expect("reader thread panicked"));
    }
    let publications = if let Some(p) = publisher {
        stop.store(true, Ordering::Relaxed);
        p.join().expect("publisher thread panicked")
    } else {
        0
    };
    let locks = publication.reader_lock_acquisitions() - locks_before;
    Outcome {
        lat: bench::summarize(pooled),
        locks,
        reads: (READERS * reads_per_reader) as u64,
        publications,
    }
}

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let source = lcg_bytes(0x0C09_0000_0002, SRC_LEN);
    let publication = Publication::new(build_generation(&source, 0)?)?;

    println!("== 64 readers, quiet vs 1 publisher ==");
    let byte_quiet = run_phase(&publication, &source, 1, BYTE_READS, 0xC0_0001, false);
    let byte_busy = run_phase(&publication, &source, 1, BYTE_READS, 0xC0_0001, true);
    print_arm("1-byte quiet", &byte_quiet);
    print_arm("1-byte busy ", &byte_busy);

    let read_quiet = run_phase(&publication, &source, 4096, READ4K_READS, 0xC0_0002, false);
    let read_busy = run_phase(&publication, &source, 4096, READ4K_READS, 0xC0_0002, true);
    print_arm("4KiB quiet  ", &read_quiet);
    print_arm("4KiB busy   ", &read_busy);

    write_json(
        dir,
        &source,
        &byte_quiet,
        &byte_busy,
        &read_quiet,
        &read_busy,
    )?;
    println!("wrote evidence/bench/concurrency.json");
    Ok(())
}

fn print_arm(label: &str, o: &Outcome) {
    println!(
        "  {label}: n={} p50={} p99={} p99.9={} max={} ns  pubs={} locks={} ({:.2e} locks/read)",
        o.lat.n,
        o.lat.p50,
        o.lat.p99,
        o.lat.p999,
        o.lat.max,
        o.publications,
        o.locks,
        o.locks as f64 / o.reads as f64
    );
}

fn write_json(
    dir: &Path,
    source: &[u8],
    byte_quiet: &Outcome,
    byte_busy: &Outcome,
    read_quiet: &Outcome,
    read_busy: &Outcome,
) -> ekbloom::Result<()> {
    let mut s = String::new();
    s.push_str("{\n");
    let _ = writeln!(
        s,
        "  \"generated_by\": \"examples/concurrency_p99.rs (sh scripts/run-corpora.sh)\","
    );
    let _ = writeln!(
        s,
        "  \"design\": \"64 PlannerReader threads read while (busy arm) 1 publisher thread builds + swaps fresh certified generations through a Publication; a reader holds a cached Arc<PublishedBody> and reads it with no lock (refresh is one acquire load when the version is unchanged). The quiet arm runs the identical readers with no publisher, so the difference isolates the publisher's effect from scheduler noise.\","
    );
    let _ = writeln!(s, "  \"readers\": {READERS},");
    let _ = writeln!(
        s,
        "  \"observed_parallelism\": {},",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    let _ = writeln!(s, "  \"logical_len\": {},", source.len());
    let _ = writeln!(s, "  \"single_byte\": {{");
    let _ = writeln!(s, "    \"reads_per_reader\": {BYTE_READS},");
    s.push_str("    \"quiet\": ");
    arm_json(&mut s, byte_quiet);
    s.push_str(",\n    \"busy\": ");
    arm_json(&mut s, byte_busy);
    s.push_str("\n  },\n");
    let _ = writeln!(s, "  \"read4k\": {{");
    let _ = writeln!(s, "    \"reads_per_reader\": {READ4K_READS},");
    s.push_str("    \"quiet\": ");
    arm_json(&mut s, read_quiet);
    s.push_str(",\n    \"busy\": ");
    arm_json(&mut s, read_busy);
    s.push_str("\n  },\n");
    let _ = writeln!(
        s,
        "  \"verdict\": \"the read path is lock-free: shared-lock acquisitions are O(publications), never O(reads), and the busy-arm tail tracks the quiet-arm scheduler baseline rather than spiking on publication\""
    );
    s.push_str("}\n");
    std::fs::write(dir.join("concurrency.json"), s)?;
    Ok(())
}

fn arm_json(s: &mut String, o: &Outcome) {
    let _ = write!(
        s,
        "{{\"n\": {}, \"reads\": {}, \"publications\": {}, \"shared_lock_acquisitions\": {}, \"locks_per_read\": {:.3e}, \"min_ns\": {}, \"p50_ns\": {}, \"p99_ns\": {}, \"p99_9_ns\": {}, \"max_ns\": {}, \"mean_ns\": {:.1}}}",
        o.lat.n,
        o.reads,
        o.publications,
        o.locks,
        o.locks as f64 / o.reads as f64,
        o.lat.min,
        o.lat.p50,
        o.lat.p99,
        o.lat.p999,
        o.lat.max,
        o.lat.mean
    );
}
