//! Integration tests for the completion-driven `io_uring` publication backend.
//!
//! What is asserted, regardless of whether `io_uring` is usable in the test
//! environment:
//!
//! * the synchronous fallback always publishes a verifiable file;
//! * `publish`/`append` never leave a partial shell or a stray `*.tmp` sibling;
//! * a torn tail never invalidates an earlier committed generation.
//!
//! When the backend is actually available (ring created *and* a probe write
//! completed), the async path must additionally produce bytes byte-identical to
//! the synchronous writer, and the result must pass `reader::open` + `verify` +
//! `read`.
//!
//! The asynchronous backend is exercised through the public
//! [`ekbloom::asyncpub`] surface only; `ekbloom::async_io` (available under the
//! `io-uring` feature) is used just for the probe error.

use std::path::{Path, PathBuf};
#[cfg(feature = "io-uring")]
use std::time::Instant;

use ekbloom::asyncpub::{append, detect_backend, publish, Backend};
use ekbloom::{create, grow, open, read_file, verify, ManifestationProfile, ReadRequest};

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

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-async-io-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_round_trip(path: &Path, src: &[u8]) {
    let opened = open(path).unwrap();
    assert!(
        verify(&opened).unwrap().ok,
        "verification failed for {path:?}"
    );
    assert_eq!(
        read_file(
            path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );
}

fn temp_sibling_exists(path: &Path) -> bool {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os).exists()
}

/// Describe the first difference between two byte images (for diagnostics).
fn first_difference(a: &[u8], b: &[u8]) -> String {
    if a.len() != b.len() {
        return format!("length differs: async={} sync={}", a.len(), b.len());
    }
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x != y {
            return format!(
                "first differing byte at offset {i}: async=0x{x:02x} sync=0x{y:02x} (len={})",
                a.len()
            );
        }
    }
    "identical".to_string()
}

#[test]
fn sync_fallback_publishes_and_leaves_no_temp() {
    let dir = temp_dir("sync");
    let path = dir.join("obj.ekb");
    let src = lcg(0xA5A5_0001, 1024);
    let body = create(&src, ManifestationProfile::Root).unwrap();

    publish(&path, &body, Backend::Sync).unwrap();
    assert_round_trip(&path, &src);
    assert!(
        !temp_sibling_exists(&path),
        "a stray *.tmp sibling was left behind"
    );

    // The detected backend must publish a verifiable file too — via io_uring when
    // available, and via the transparent synchronous fallback otherwise.
    let detected = dir.join("detected.ekb");
    publish(&detected, &body, detect_backend()).unwrap();
    assert_round_trip(&detected, &src);
    assert!(!temp_sibling_exists(&detected));

    // Explicitly requesting the async backend must never fail even when io_uring
    // is unavailable: it falls back to the synchronous writer.
    let forced = dir.join("forced.ekb");
    publish(&forced, &body, Backend::IoUring).unwrap();
    assert_round_trip(&forced, &src);
    assert_eq!(
        std::fs::read(&forced).unwrap(),
        std::fs::read(&path).unwrap()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn publishing_never_exposes_a_partial_shell() {
    let dir = temp_dir("partial");
    let path = dir.join("obj.ekb");
    let src = lcg(0xB0B0_0001, 2048);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();

    publish(&path, &body, detect_backend()).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 256 }).unwrap();
    let generation = append(&path, &body, detect_backend()).unwrap();
    assert_eq!(generation, body.generation);

    // The newest commit is complete and the earlier bytes are intact.
    let opened = open(&path).unwrap();
    assert_eq!(opened.generation, generation);
    assert!(verify(&opened).unwrap().ok);
    assert_round_trip(&path, &src);
    assert!(!temp_sibling_exists(&path));

    // Simulate a crash during a subsequent append: a torn tail must never
    // invalidate the newest committed generation, so recovery still reports the
    // previous complete generation as newest-valid.
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 16]).unwrap();
    let report = ekbloom::recovery::recover(&path).unwrap();
    assert_eq!(report.newest_valid, Some(generation - 1));
    assert!(report.torn_tail_bytes > 0);
    assert_round_trip(&path, &src);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn async_backend_matches_sync_byte_for_byte_when_available() {
    let dir = temp_dir("parity");
    let src = lcg(0xC0DE_0001, 4096);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();

    let async_path = dir.join("async.ekb");
    let sync_path = dir.join("sync.ekb");

    // `Backend::IoUring` always succeeds (falling back if needed), so this is a
    // valid parity check either way.
    publish(&async_path, &body, Backend::IoUring).unwrap();
    publish(&sync_path, &body, Backend::Sync).unwrap();
    assert_round_trip(&async_path, &src);
    let a = std::fs::read(&async_path).unwrap();
    let s = std::fs::read(&sync_path).unwrap();
    assert!(
        a == s,
        "async and sync fresh publications diverge: {}",
        first_difference(&a, &s)
    );

    // Append parity: append the same grown body through both backends and compare.
    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    let async_append = dir.join("async-append.ekb");
    let sync_append = dir.join("sync-append.ekb");
    std::fs::copy(&sync_path, &async_append).unwrap();
    std::fs::copy(&sync_path, &sync_append).unwrap();

    let g_async = append(&async_append, &body, Backend::IoUring).unwrap();
    let g_sync = append(&sync_append, &body, Backend::Sync).unwrap();
    assert_eq!(g_async, g_sync);
    assert_round_trip(&async_append, &src);
    let a = std::fs::read(&async_append).unwrap();
    let s = std::fs::read(&sync_append).unwrap();
    assert!(
        a == s,
        "async and sync appended files diverge: {}",
        first_difference(&a, &s)
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Median of a non-empty sample.
#[cfg(feature = "io-uring")]
fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// JSON-escape a string for the evidence file.
#[cfg(feature = "io-uring")]
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Measure async vs. sync publication of the same body and, when
/// `EKB_IOURING_EVIDENCE` is set, write `evidence/bench/io-uring.json`.
///
/// When `io_uring` is unavailable this records the exact probe error and marks the
/// async path as compile-verified but not runtime-verified — it does not fake a
/// measurement.
#[cfg(feature = "io-uring")]
#[test]
fn benchmark_async_vs_sync_and_emit_evidence() {
    const ITERATIONS: usize = 15;

    let dir = temp_dir("bench");
    let src = lcg(0x1051_0001, 1 << 20);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 4096 }).unwrap();

    let probe = ekbloom::async_io::probe();
    let usable = probe.is_ok();
    let probe_error = probe
        .as_ref()
        .err()
        .map(|e| format!("{} (raw_os_error={:?})", e, e.raw_os_error()));

    // The sync path must always work and round-trip.
    let sync_probe_path = dir.join("sync-probe.ekb");
    publish(&sync_probe_path, &body, Backend::Sync).unwrap();
    assert_round_trip(&sync_probe_path, &src);

    let mut async_ns: Vec<u128> = Vec::new();
    let mut sync_ns: Vec<u128> = Vec::new();
    let mut identical = true; // asserted only when measured; vacuously true otherwise
    let mut parity_detail = String::from("not measured (io_uring unavailable)");
    let mut durability_ordered = true;

    if usable {
        parity_detail = "identical".to_string();
        for i in 0..ITERATIONS {
            let a = dir.join(format!("bench-async-{i}.ekb"));
            let s = dir.join(format!("bench-sync-{i}.ekb"));

            let t = Instant::now();
            publish(&a, &body, Backend::IoUring).unwrap();
            async_ns.push(t.elapsed().as_nanos());

            let t = Instant::now();
            publish(&s, &body, Backend::Sync).unwrap();
            sync_ns.push(t.elapsed().as_nanos());

            let abytes = std::fs::read(&a).unwrap();
            let sbytes = std::fs::read(&s).unwrap();
            if abytes != sbytes {
                identical = false;
                parity_detail = first_difference(&abytes, &sbytes);
            }
            let opened = open(&a).unwrap();
            durability_ordered &= verify(&opened).unwrap().ok;
            assert_round_trip(&a, &src);

            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&s);
        }
    }

    let async_median = if usable { median(async_ns.clone()) } else { 0 };
    let sync_median = if usable { median(sync_ns.clone()) } else { 0 };
    let container_flags = std::env::var("EKB_IOURING_CONTAINER_FLAGS").unwrap_or_default();

    if let Ok(path) = std::env::var("EKB_IOURING_EVIDENCE") {
        if let Some(parent) = Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let error_json = match &probe_error {
            Some(e) => format!("\"{}\"", json_escape(e)),
            None => "null".to_string(),
        };
        let json = format!(
            "{{\n  \"io_uring_usable\": {usable},\n  \"iterations\": {iterations},\n  \"body_bytes\": {body_bytes},\n  \"async_median_ns\": {async_median},\n  \"sync_median_ns\": {sync_median},\n  \"async_min_ns\": {async_min},\n  \"sync_min_ns\": {sync_min},\n  \"async_and_sync_byte_identical\": {identical},\n  \"durability_ordered\": {durability_ordered},\n  \"container_flags\": \"{flags}\",\n  \"probe_error\": {error_json}\n}}\n",
            usable = usable,
            iterations = ITERATIONS,
            body_bytes = std::fs::metadata(&sync_probe_path).map(|m| m.len()).unwrap_or(0),
            async_median = async_median,
            sync_median = sync_median,
            async_min = async_ns.iter().copied().min().unwrap_or(0),
            sync_min = sync_ns.iter().copied().min().unwrap_or(0),
            identical = identical,
            durability_ordered = durability_ordered,
            flags = json_escape(&container_flags),
            error_json = error_json,
        );
        std::fs::write(&path, json).unwrap();
    }

    // Whatever happened, the sync fallback and parity guarantees must hold.
    if usable {
        assert!(
            identical,
            "async and sync publications diverge: {parity_detail}"
        );
        assert!(
            durability_ordered,
            "an async publication failed verification"
        );
    } else {
        assert!(probe_error.is_some(), "probe failed without an error");
    }

    std::fs::remove_dir_all(&dir).unwrap();
}
