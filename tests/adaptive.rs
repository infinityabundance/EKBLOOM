//! Integration tests for the adaptive / generative shells (O7), the parallel
//! different-shell reader proof (O8), the per-depth metrics (§10) and the
//! memory-tier curve (O9).
//!
//! All corpora are small and the sweep iteration budget is clamped so the whole
//! file stays fast. The heavy measurements live in `examples/sweep.rs`.

use std::thread;

use ekbloom::sweep::{depth_metrics, memory_tiers, SweepConfig};
use ekbloom::{
    adapt_to_workload, create, manifest, read, read_shell, verify_deep, ManifestationProfile,
    ReadRequest, ShellChoice, Workload, WorkloadKind,
};

/// Deterministic LCG corpus.
fn lcg(seed: u64, len: usize) -> Vec<u8> {
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

fn small_cfg() -> SweepConfig {
    SweepConfig {
        iterations: 4,
        seed: 0x0A11_7E57_0000_0001,
    }
}

/// A `HotSet` workload whose reads concentrate on `[0, hot_prefix)`.
fn hot_set_workload(hot_prefix: u64, reads: usize) -> Workload {
    let mut s = 0x0A11_0000_0000_0007u64;
    let sample = (0..reads)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s % hot_prefix, 1u64)
        })
        .collect();
    Workload {
        kind: WorkloadKind::HotSet,
        hot_prefix: Some(hot_prefix),
        sample,
    }
}

// ---------------------------------------------------------------------------
// (§10) per-depth metrics.
// ---------------------------------------------------------------------------

#[test]
fn depth_metrics_are_expected_and_monotone() {
    let src = lcg(0x0DEF_0000_0001, 64 * 1024);
    let m = depth_metrics(&src, &small_cfg()).unwrap();
    assert_eq!(m.source_len, src.len() as u64);
    assert_eq!(m.points.len(), 6);
    let depths: Vec<u16> = m.points.iter().map(|p| p.depth).collect();
    assert_eq!(depths, vec![1, 2, 4, 8, 16, 32]);

    let mut prev = u64::MAX;
    for p in &m.points {
        // Exact, uniform semantics at every depth.
        assert_eq!(p.key_semantics, "block ordinal j");
        assert_eq!(p.successor_semantics, "cold[j] = x_{D*(j+1)}");
        assert_eq!(p.certification, "exact-by-construction");
        // Deeper is cheaper at identical speed.
        assert!(
            p.physical_bytes > 0 && p.physical_bytes <= prev,
            "D={} physical {} not <= previous {}",
            p.depth,
            p.physical_bytes,
            prev
        );
        prev = p.physical_bytes;
        assert!(p.build_ns > 0, "D={} build time missing", p.depth);
        assert!(p.seq_bytes_per_s.is_finite() && p.seq_bytes_per_s > 0.0);
        assert!(p.random_1b_ns_p50.is_finite() && p.random_1b_ns_p50 > 0.0);
        assert!(!p.cache_note.is_empty());
    }
    // The physical bytes shrink as the cold successor array shrinks: 4*ceil(N/D).
    // The core, payload prefix and certificate are identical across depths, so the
    // D=1 minus D=32 difference isolates exactly that term.
    let n = src.len() as u64;
    let expect_delta = 4 * n - 4 * n.div_ceil(32);
    assert_eq!(
        m.points[0].physical_bytes - m.points[5].physical_bytes,
        expect_delta,
        "cold-array term does not match 4*N - 4*ceil(N/32)"
    );
}

// ---------------------------------------------------------------------------
// (O9) memory tiers.
// ---------------------------------------------------------------------------

#[test]
fn memory_tiers_are_finite_and_sized() {
    let tiers = [
        ("L1", 16 * 1024u64, 16 * 1024usize),
        ("L2", 128 * 1024u64, 128 * 1024usize),
    ];
    let m = memory_tiers(&tiers, &small_cfg()).unwrap();
    assert_eq!(m.tiers.len(), 2);
    for t in &m.tiers {
        assert!(t.corpus_bytes > 0);
        assert!(t.physical_bytes >= t.corpus_bytes);
        assert!(t.seq_bytes_per_s.is_finite() && t.seq_bytes_per_s > 0.0);
        assert!(t.random_1b_ns_p50.is_finite() && t.random_1b_ns_p50 > 0.0);
        assert!(t.random_4k_ns_p50.is_finite());
        assert!(t.samples > 0);
        assert!(!t.cache_note.is_empty());
    }
}

// ---------------------------------------------------------------------------
// (O7) adaptive shells.
// ---------------------------------------------------------------------------

#[test]
fn adapt_to_hot_set_is_additive_and_improves() {
    let src = lcg(0x0ADD_5000_0001, 256 * 1024);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let before_gen = body.generation;
    let before_bytes = body.physical_bytes();
    let before_profiles = body.profiles.clone();

    let workload = hot_set_workload(32 * 1024, 256);
    let report = adapt_to_workload(&mut body, &workload, &small_cfg()).unwrap();

    // Additive: a new generation appended, earlier profiles retained.
    assert_eq!(report.before_generation, before_gen);
    assert_eq!(report.new_generation, before_gen + 1);
    assert_eq!(body.generation, before_gen + 1);
    assert!(report.added_physical_bytes > 0);
    assert_eq!(
        body.physical_bytes(),
        before_bytes + report.added_physical_bytes
    );
    assert_eq!(report.shell, Some(ShellChoice::Path));
    assert!(body.profiles.len() == before_profiles.len() + 1);
    assert!(body
        .profiles
        .starts_with(&before_profiles[..before_profiles.len()]));
    assert_eq!(body.profiles.last().unwrap(), "path(D=32)");

    // Measurable improvement for the observed hot-set workload.
    assert!(
        report.improved,
        "adaptive shell did not improve the hot-set workload: before {:.1} ns/read, after {:.1} ns/read",
        report.before_ns_per_read, report.after_ns_per_read
    );
    assert!(report.after_ns_per_read < report.before_ns_per_read);
    assert!(report.after_bytes_per_s > report.before_bytes_per_s);

    // The adapted body is still fully verifiable (new bloom + certificate linked).
    let v = verify_deep(&body).unwrap();
    assert!(
        v.ok,
        "adapted body failed deep verification: {:?}",
        v.checks
    );

    // The adaptive read really returns the authoritative bytes.
    for &(off, len) in &workload.sample {
        assert_eq!(
            read_shell(&body, ShellChoice::Path, off, len).unwrap(),
            read(
                &body,
                ReadRequest {
                    offset: off,
                    length: len
                }
            )
            .unwrap()
        );
    }
}

#[test]
fn adapt_is_a_no_op_when_the_read_surface_is_already_present() {
    let src = lcg(0x0ADD_5000_0002, 64 * 1024);
    let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    let before = body.clone();

    let report =
        adapt_to_workload(&mut body, &hot_set_workload(16 * 1024, 64), &small_cfg()).unwrap();

    // Nothing appended: the certified path bloom is already the read surface.
    assert_eq!(report.built, "none");
    assert_eq!(report.shell, None);
    assert_eq!(report.added_physical_bytes, 0);
    assert_eq!(report.new_generation, report.before_generation);
    assert!(!report.improved);
    assert_eq!(body, before, "no-op adaptation must not touch the body");
}

#[test]
fn adapt_preserves_earlier_generations() {
    let src = lcg(0x0ADD_5000_0003, 128 * 1024);
    let mut body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    let checkpoints_before = body.checkpoints.len();
    let blooms_before = body.blooms.len();
    let gen_before = body.generation;

    let report =
        adapt_to_workload(&mut body, &hot_set_workload(16 * 1024, 64), &small_cfg()).unwrap();
    assert!(report.improved);
    assert_eq!(body.generation, gen_before + 1);
    // Earlier structures remain (non-withdrawal).
    assert_eq!(body.checkpoints.len(), checkpoints_before);
    assert_eq!(body.blooms.len(), blooms_before + 1);
    assert!(verify_deep(&body).unwrap().ok);
}

#[test]
fn adapt_rejects_out_of_range_observations() {
    let src = lcg(0x0ADD_5000_0004, 4096);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let bad = Workload {
        kind: WorkloadKind::UniformRandom,
        hot_prefix: None,
        sample: vec![(4096, 1)],
    };
    assert!(adapt_to_workload(&mut body, &bad, &small_cfg()).is_err());
}

// ---------------------------------------------------------------------------
// (O8) parallel different-shell readers.
// ---------------------------------------------------------------------------

/// A body manifesting every radial shell at once.
fn all_shells_body(src: &[u8]) -> ekbloom::EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    manifest(&mut body, ManifestationProfile::Full).unwrap();
    manifest(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();
    body
}

fn shell_index(shell: ShellChoice) -> u64 {
    match shell {
        ShellChoice::Path => 0,
        ShellChoice::Checkpoint => 1,
        ShellChoice::History => 2,
        ShellChoice::Machine => 3,
    }
}

#[test]
fn read_shell_matches_reader_for_every_shell() {
    let src = lcg(0x0C0E_0000_0001, 32 * 1024);
    let body = all_shells_body(&src);
    let n = src.len() as u64;
    let ranges = [(0u64, 1u64), (1, 4095), (n / 2, 777), (n - 1, 1), (0, n)];
    for shell in [
        ShellChoice::Path,
        ShellChoice::Checkpoint,
        ShellChoice::History,
        ShellChoice::Machine,
    ] {
        for &(off, len) in &ranges {
            let got = read_shell(&body, shell, off, len).unwrap();
            assert_eq!(
                got,
                read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: len
                    }
                )
                .unwrap(),
                "{} disagrees with reader at ({off},{len})",
                shell.as_str()
            );
            assert_eq!(got, src[off as usize..(off + len) as usize].to_vec());
        }
    }
}

#[test]
fn read_shell_errors_when_the_shell_is_absent() {
    let src = lcg(0x0C0E_0000_0002, 4096);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    assert!(read_shell(&body, ShellChoice::Path, 0, 1).is_err());
    assert!(read_shell(&body, ShellChoice::Checkpoint, 0, 1).is_err());
    assert!(read_shell(&body, ShellChoice::History, 0, 1).is_err());
    // The machine shell is always available.
    assert_eq!(
        read_shell(&body, ShellChoice::Machine, 0, 4).unwrap(),
        src[..4].to_vec()
    );
}

#[test]
fn parallel_different_shell_readers_are_byte_exact() {
    let src = lcg(0x0C0E_0000_0003, 128 * 1024);
    let n = src.len() as u64;
    let body = all_shells_body(&src);

    let shells = [
        ShellChoice::Path,
        ShellChoice::Checkpoint,
        ShellChoice::History,
        ShellChoice::Machine,
    ];
    let chunk = n.div_ceil(shells.len() as u64);
    // Every thread also reads the *same* shared range to prove cross-shell agreement.
    let shared = (n / 2, 4096u64.min(n));

    let results: Vec<(ShellChoice, Vec<u8>, Vec<u8>)> = thread::scope(|scope| {
        let handles: Vec<_> = shells
            .iter()
            .map(|&shell| {
                let body = &body;
                scope.spawn(move || {
                    let idx = shell_index(shell);
                    let start = idx * chunk;
                    let end = ((idx + 1) * chunk).min(n);
                    let disjoint = if start < end {
                        read_shell(body, shell, start, end - start).unwrap()
                    } else {
                        Vec::new()
                    };
                    let shared_out = if shared.1 > 0 {
                        read_shell(body, shell, shared.0, shared.1).unwrap()
                    } else {
                        Vec::new()
                    };
                    (shell, disjoint, shared_out)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut shared_reference: Option<Vec<u8>> = None;
    for (shell, disjoint, shared_out) in &results {
        // Disjoint byte-exactness against the source and the authoritative reader.
        let idx = shell_index(*shell);
        let start = idx * chunk;
        let end = ((idx + 1) * chunk).min(n);
        assert_eq!(
            *disjoint,
            src[start as usize..end as usize].to_vec(),
            "{} disjoint range mismatch",
            shell.as_str()
        );
        assert_eq!(
            *disjoint,
            read(
                &body,
                ReadRequest {
                    offset: start,
                    length: end - start
                }
            )
            .unwrap(),
            "{} vs reader",
            shell.as_str()
        );
        // Shared-range cross-shell agreement.
        let expected = read(
            &body,
            ReadRequest {
                offset: shared.0,
                length: shared.1,
            },
        )
        .unwrap();
        assert_eq!(*shared_out, expected, "{} shared range", shell.as_str());
        match &shared_reference {
            None => shared_reference = Some(shared_out.clone()),
            Some(r) => assert_eq!(r, shared_out, "shells disagreed on the shared range"),
        }
    }
    assert!(shared_reference.is_some());
}
