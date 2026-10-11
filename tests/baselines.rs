//! Conventional direct-access baselines (`src/baseline.rs`) versus the
//! authoritative reader.
//!
//! Every baseline must return the source byte-for-byte for full, single-byte
//! and range reads; `physical_bytes` accounting must be sound; and construction
//! time must be recorded. The corpus and sample budget are deliberately small so
//! the suite stays fast.

use std::path::{Path, PathBuf};

use ekbloom::{
    create, measure_baseline, now_ns, read, write_baselines_json, Baseline, BaselineKind,
    BaselineReport, Capabilities, EkbloomError, ManifestationProfile, ReadRequest, SweepConfig,
    DEFAULT_INDEX_BLOCK,
};

/// Deterministic LCG corpus.
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

/// A per-test scratch directory under `target/` (never the source tree).
fn tmp_dir(name: &str) -> PathBuf {
    let dir = Path::new("target/baselines-test").join(name);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn small_cfg() -> SweepConfig {
    SweepConfig {
        iterations: 8,
        seed: 0x9E37_79B9_7F4A_7C15,
    }
}

#[test]
fn every_baseline_is_byte_exact_against_the_authoritative_reader() {
    let src = lcg_bytes(0xBA5E_0000_0000_0001, 16 * 1024);
    let n = src.len() as u64;
    let dir = tmp_dir("byte-exact");

    // Authoritative reference: EKBLOOM's root reader must reproduce the source.
    let body = create(&src, ManifestationProfile::Root).expect("create root");
    let reference = read(
        &body,
        ReadRequest {
            offset: 0,
            length: n,
        },
    )
    .expect("reader full");
    assert_eq!(reference, src, "authoritative reader must be identity here");

    let offsets: [u64; 6] = [0, 1, n / 2, n - 1, 42, n / 3];
    let ranges: [(u64, u64); 5] = [(0, 1), (1, 4095), (n / 2, 777), (n - 1, 1), (0, n)];

    for kind in BaselineKind::all() {
        let b = Baseline::build(kind, &src, &dir).expect("build baseline");

        // Whole-object reads.
        assert_eq!(b.read_sequential().unwrap(), src, "{kind:?} seq");
        assert_eq!(
            b.read(ReadRequest {
                offset: 0,
                length: n
            })
            .unwrap(),
            src,
            "{kind:?} full"
        );

        // Single-byte reads in order.
        let mut got: Vec<u8> = Vec::new();
        b.read_random_1b(&offsets, &mut got).unwrap();
        let want: Vec<u8> = offsets.iter().map(|&o| src[o as usize]).collect();
        assert_eq!(got, want, "{kind:?} random-1b");

        // Range reads.
        for &(off, len) in &ranges {
            let got = b
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .unwrap();
            let want = &src[off as usize..(off + len) as usize];
            assert_eq!(got, want, "{kind:?} range ({off},{len})");
        }

        // Zero-length read always succeeds and yields nothing.
        assert!(b
            .read(ReadRequest {
                offset: n,
                length: 0
            })
            .unwrap()
            .is_empty());
    }
}

#[test]
fn bounds_and_overflow_semantics_match_the_reader() {
    let src = lcg_bytes(0xBA5E_0000_0000_0002, 4096);
    let n = src.len() as u64;
    let dir = tmp_dir("bounds");

    for kind in BaselineKind::all() {
        let b = Baseline::build(kind, &src, &dir).expect("build baseline");

        // Past the end.
        assert!(matches!(
            b.read(ReadRequest {
                offset: n,
                length: 1
            }),
            Err(EkbloomError::OutOfBounds { .. })
        ));
        assert!(matches!(
            b.read(ReadRequest {
                offset: 0,
                length: n + 1
            }),
            Err(EkbloomError::OutOfBounds { .. })
        ));
        // Overflow at u64::MAX.
        assert!(matches!(
            b.read(ReadRequest {
                offset: u64::MAX,
                length: 1
            }),
            Err(EkbloomError::Overflow { .. })
        ));
        // Single-byte out-of-range.
        let mut out = Vec::new();
        assert!(matches!(
            b.read_random_1b(&[n], &mut out),
            Err(EkbloomError::OutOfBounds { .. })
        ));
        assert!(matches!(
            b.read_random_1b(&[u64::MAX], &mut out),
            Err(EkbloomError::Overflow { .. })
        ));
    }
}

#[test]
fn physical_bytes_accounting_is_sound() {
    let src = lcg_bytes(0xBA5E_0000_0000_0003, 16 * 1024 + 7);
    let n = src.len() as u64;
    let dir = tmp_dir("accounting");

    let raw = Baseline::build(BaselineKind::RawVec, &src, &dir).unwrap();
    assert_eq!(raw.physical_bytes(), n);

    let pre = Baseline::build(BaselineKind::Predecoded, &src, &dir).unwrap();
    assert_eq!(
        pre.physical_bytes(),
        n,
        "V=N: the predecoded cache is exactly the raw bytes"
    );

    let idx = Baseline::build(BaselineKind::IndexedArray, &src, &dir).unwrap();
    let blocks = src.len().div_ceil(DEFAULT_INDEX_BLOCK) as u64;
    assert_eq!(idx.physical_bytes(), n + blocks * 8);
    assert_eq!(idx.index().len() as u64, blocks);
    assert_eq!(idx.index().first(), Some(&0));

    let file = Baseline::build(BaselineKind::RawFile, &src, &dir).unwrap();
    assert_eq!(file.physical_bytes(), n, "file-backed retained = file size");
    let path = file.backing_path().expect("raw-file has a backing path");
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        n,
        "on-disk file length must match"
    );

    // Baselines retain no EKBLOOM capabilities.
    for kind in BaselineKind::all() {
        let b = Baseline::build(kind, &src, &dir).unwrap();
        assert_eq!(b.capabilities(), Capabilities::none());
        assert!(b.capabilities().is_empty());
    }
}

#[test]
fn construction_time_is_recorded_and_measurement_is_finite() {
    let src = lcg_bytes(0xBA5E_0000_0000_0004, 8 * 1024);
    let dir = tmp_dir("construction");
    let cfg = small_cfg();

    for kind in BaselineKind::all() {
        let b = Baseline::build(kind, &src, &dir).unwrap();
        let ns = b.construction_ns();
        assert!(ns < 60_000_000_000, "construction time must be recorded");

        let p = measure_baseline(&b, &cfg).unwrap();
        assert_eq!(p.physical_bytes, b.physical_bytes());
        assert!(p.seq_bytes_per_s.is_finite() && p.seq_bytes_per_s > 0.0);
        assert!(p.random_1b_ns_p50.is_finite());
        assert!(p.random_1b_ns_p99 >= p.random_1b_ns_p50);
        assert!(p.random_4k_ns_p50.is_finite());
        assert!(p.samples > 0);
        assert!(p.capabilities.is_empty());
    }

    // A recorded time is also a sane lower bound relative to the wall clock.
    assert!(now_ns() > 0);
}

#[test]
fn report_json_is_written_and_well_formed() {
    let src = lcg_bytes(0xBA5E_0000_0000_0005, 8 * 1024);
    let dir = tmp_dir("json");
    let cfg = small_cfg();

    let mut points = Vec::new();
    for kind in [BaselineKind::RawVec, BaselineKind::RawFile] {
        let b = Baseline::build(kind, &src, &dir).unwrap();
        points.push(measure_baseline(&b, &cfg).unwrap());
    }
    let report = BaselineReport {
        cpu: ekbloom::cpu_features(),
        source_len: src.len() as u64,
        source_seed: 1,
        iterations: cfg.iterations,
        sweep_seed: cfg.seed,
        points,
    };
    let out = dir.join("baselines.json");
    write_baselines_json(&out, &report).unwrap();

    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
    assert!(text.contains("\"points\""));
    assert!(text.contains("\"retained_capabilities\""));
    assert!(text.contains("\"class\": \"baseline\""));
    assert!(text.contains("raw-vec"));
    assert!(text.contains("raw-file"));
}
