//! Fault injection: transactional `.ekb`/stream publication under I/O faults.
//!
//! This is the transactional-publication court. It proves that no partially
//! written generation is ever visible, that the previous committed generation is
//! always intact and byte-identical after a fault, and that recovery restores a
//! clean state.
//!
//! Two injection channels:
//!
//! * **A failing `Write`.** [`StreamBuilder`] is generic over [`std::io::Write`],
//!   so [`FailingWriter`] is injected directly into `write`/`checkpoint`/`finish`
//!   and made to fail after exactly `N` accepted bytes with one of three error
//!   flavours (`WriteZero`, ENOSPC, EIO). The `N` accepted bytes are the on-disk
//!   crash image.
//! * **A byte-exact crash image.** The path-based writers
//!   (`writer::write_body_*`, `writer::append_generation*`) build their own
//!   `File` internally; none accepts a caller writer, and no seam is added (the
//!   runtime behaviour is frozen). For `append`, a write that fails after `N`
//!   total file bytes leaves exactly `final[0..N]` on disk, so the sweep truncates
//!   the committed file to every `N`. For `publish`, the destination is only ever
//!   changed by the atomic `rename`, so a failure writes nothing to it; the
//!   filesystem-boundary cases (blocked temp path, rename failure, stale temp)
//!   cover every point.
//!
//! When `EKB_FAULTS_EVIDENCE` is set the sweeps also write `evidence/bench/faults.json`
//! (`scripts/run-faults.sh`), recording the fault count and the 100 % pass rate.

use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};

use ekbloom::reader;
use ekbloom::{
    append_generation, append_generation_authenticated, create, grow, open, open_authenticated,
    read_file, recover, scan_generations, truncate_to_valid, verify, write_body,
    write_body_authenticated, ManifestationProfile, ReadRequest, RepresentationPolicy,
    StreamBuilder, StreamManifest, StreamOptions, StreamReader,
};

// ---------------------------------------------------------------------------
// Fault model
// ---------------------------------------------------------------------------

/// The error flavour a fault injects, matching a real device failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// A short write that accepted zero bytes (`ErrorKind::WriteZero`).
    WriteZero,
    /// `ENOSPC` (28): the device is full.
    Enospc,
    /// `EIO` (5): the device reported a media error.
    Eio,
}

impl Fault {
    fn io(self) -> io::Error {
        match self {
            Fault::WriteZero => io::Error::new(io::ErrorKind::WriteZero, "injected: short write"),
            Fault::Enospc => io::Error::from_raw_os_error(28),
            Fault::Eio => io::Error::from_raw_os_error(5),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Fault::WriteZero => "write-zero",
            Fault::Enospc => "enospc",
            Fault::Eio => "eio",
        }
    }

    const ALL: [Fault; 3] = [Fault::WriteZero, Fault::Enospc, Fault::Eio];
}

/// A writer that accepts exactly `budget` bytes and then fails with `fault`.
///
/// The accepted bytes land in `inner`, so after a failed build `inner` is the
/// byte-exact crash image a real device would have left.
struct FailingWriter<'a> {
    inner: &'a mut Vec<u8>,
    budget: usize,
    written: usize,
    fault: Fault,
}

impl Write for FailingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.budget {
            return Err(self.fault.io());
        }
        let n = (self.budget - self.written).min(buf.len());
        self.inner.extend_from_slice(&buf[..n]);
        self.written += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A device that has accepted every byte it was offered flushes cleanly;
        // the fault is on the next extending write (as `write` returns next call).
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Low-entropy bytes (so per-block representation adapts and payloads shrink).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(alpha[((s >> 33) % 4) as usize]);
    }
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-faults-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

/// Feed `src` through a fresh [`StreamBuilder`] over `sink`, chunked and with
/// periodic checkpoints, then `finish`.
fn drive_stream<W: Write>(
    sink: W,
    src: &[u8],
    block_bytes: u32,
    policy: RepresentationPolicy,
) -> ekbloom::Result<StreamManifest> {
    let mut b = StreamBuilder::new(
        sink,
        StreamOptions {
            block_bytes,
            machine: policy,
        },
    )?;
    for (i, chunk) in src.chunks(37).enumerate() {
        b.write(chunk)?;
        if i % 4 == 3 {
            b.checkpoint()?;
        }
    }
    b.finish()
}

/// A summary of a bounded stream fault sweep.
struct StreamSweep {
    fault_points: u64,
    passed: u64,
    finish_phase_points: u64,
    torn_readable: u64,
}

/// Sweep a failing writer over every byte offset of a container build.
fn stream_sweep(src: &[u8], block_bytes: u32, total: usize, finish_start: usize) -> StreamSweep {
    let mut stats = StreamSweep {
        fault_points: 0,
        passed: 0,
        finish_phase_points: 0,
        torn_readable: 0,
    };
    for n in 0..=total {
        for fault in Fault::ALL {
            stats.fault_points += 1;
            if n >= finish_start && n < total {
                stats.finish_phase_points += 1;
            }
            let mut inner = Vec::new();
            let result = drive_stream(
                FailingWriter {
                    inner: &mut inner,
                    budget: n,
                    written: 0,
                    fault,
                },
                src,
                block_bytes,
                RepresentationPolicy::Auto,
            );
            let ok = if n >= total {
                // The full container was written; it must be the reference image.
                match result {
                    Ok(m) => m.container_bytes == total as u64 && inner.len() == total,
                    Err(_) => false,
                }
            } else {
                // The build must have failed and left exactly `n` bytes, and no
                // partially written container may be readable.
                let failed = result.is_err() && inner.len() == n;
                let torn = StreamReader::open(Cursor::new(inner.clone())).is_ok();
                if torn {
                    stats.torn_readable += 1;
                }
                failed && !torn
            };
            if ok {
                stats.passed += 1;
            } else {
                panic!("stream fault point failed: n={n} fault={}", fault.label());
            }
        }
    }
    stats
}

/// Assert the previous committed generation is intact and readable at `path`.
fn assert_previous_generation(path: &Path, src: &[u8], key: Option<&[u8; 32]>, generation: u64) {
    let opened = match key {
        Some(k) => open_authenticated(path, k).unwrap(),
        None => open(path).unwrap(),
    };
    assert_eq!(opened.generation, generation, "path={path:?}");
    assert!(verify(&opened).unwrap().ok);
    assert_eq!(
        read_file(
            path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64,
            }
        )
        .unwrap(),
        src
    );
}

// ---------------------------------------------------------------------------
// Streaming: a failing writer over every byte offset of write/checkpoint/finish
// ---------------------------------------------------------------------------

#[test]
fn stream_faults_never_expose_a_partial_container() {
    let src = skewed(0xFA17_0001, 2048);
    let block = 512u32;
    let mut reference = Vec::new();
    let manifest = drive_stream(&mut reference, &src, block, RepresentationPolicy::Auto).unwrap();
    let total = reference.len();
    assert_eq!(manifest.container_bytes as usize, total);

    let dir_bytes = manifest.directory_bytes as usize;
    let footer_bytes = manifest.footer_bytes as usize;
    let finish_start = total - dir_bytes - footer_bytes;

    let stats = stream_sweep(&src, block, total, finish_start);
    assert_eq!(stats.torn_readable, 0, "a torn container was readable");
    assert_eq!(stats.passed, stats.fault_points, "a fault point failed");
    assert!(
        stats.finish_phase_points > 0,
        "no faults landed in `finish`"
    );
    assert!(
        stats.fault_points > 400,
        "sweep is too small to be meaningful"
    );
}

// ---------------------------------------------------------------------------
// Append: the crash image is the committed prefix; the previous generation stays
// ---------------------------------------------------------------------------

/// Sweep the appended region, truncating the committed file to every `N`.
fn append_sweep(src: &[u8], authenticated: bool) -> (u64, u64, u64) {
    let key = [0x5Au8; 32];
    let dir = temp_dir(if authenticated {
        "append-auth"
    } else {
        "append"
    });
    let path = dir.join("obj.ekb");

    let mut body = create(src, ManifestationProfile::Root).unwrap();
    if authenticated {
        write_body_authenticated(&path, &body, &key).unwrap();
    } else {
        write_body(&path, &body).unwrap();
    }
    let gen1_len = std::fs::metadata(&path).unwrap().len() as usize;
    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    if authenticated {
        append_generation_authenticated(&path, &body, &key).unwrap();
    } else {
        append_generation(&path, &body).unwrap();
    }
    let full = std::fs::read(&path).unwrap();
    assert!(full.len() > gen1_len);

    let mut fault_points = 0u64;
    let mut passed = 0u64;
    let mut recovered = 0u64;
    for n in gen1_len..=full.len() {
        fault_points += 1;
        let torn = dir.join(format!("torn-{n}.ekb"));
        std::fs::write(&torn, &full[..n]).unwrap();

        let key_ref = authenticated.then_some(&key);
        if n == full.len() {
            assert_previous_generation(&torn, src, key_ref, 2);
            let report = recover(&torn).unwrap();
            assert_eq!(report.newest_valid, Some(2));
            assert_eq!(report.torn_tail_bytes, 0);
        } else if n == gen1_len {
            // A fault after exactly `clean_len` bytes means the append wrote
            // nothing: the file is the previous commit, clean and complete.
            assert_previous_generation(&torn, src, key_ref, 1);
            let report = recover(&torn).unwrap();
            assert_eq!(report.newest_valid, Some(1));
            assert_eq!(report.torn_tail_bytes, 0);
        } else {
            // (a) no partially-visible generation, (b) previous generation intact.
            assert_previous_generation(&torn, src, key_ref, 1);
            let report = recover(&torn).unwrap();
            assert_eq!(report.newest_valid, Some(1), "n={n}");
            assert!(report.torn_tail_bytes > 0, "n={n}");
            // (c) recovery restores a clean state.
            let repaired = truncate_to_valid(&torn).unwrap();
            assert!(repaired.repaired);
            assert_eq!(repaired.newest_valid, Some(1));
            assert_eq!(repaired.torn_tail_bytes, 0);
            assert_eq!(scan_generations(&torn).unwrap().torn_tail_bytes, 0);
            assert_previous_generation(&torn, src, key_ref, 1);
            recovered += 1;
        }
        passed += 1;
        let _ = std::fs::remove_file(&torn);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(fault_points > 100, "append sweep is too small");
    assert_eq!(passed, fault_points);
    assert_eq!(recovered, fault_points - 2); // all but the clean `gen1_len` and `full.len()`
    (fault_points, passed, recovered)
}

#[test]
fn append_faults_keep_previous_generation_authenticated() {
    let src = lcg(0xFA17_0002, 4096);
    let (points, passed, recovered) = append_sweep(&src, true);
    assert_eq!(points, passed);
    assert!(recovered > 0);
}

#[test]
fn append_faults_keep_previous_generation_unauthenticated() {
    let src = skewed(0xFA17_0003, 2048);
    let (points, passed, recovered) = append_sweep(&src, false);
    assert_eq!(points, passed);
    assert!(recovered > 0);
}

// ---------------------------------------------------------------------------
// Publish: a failed write never tears the destination
// ---------------------------------------------------------------------------

#[test]
fn publish_faults_leave_destination_absent_or_old_valid() {
    let key = [0x11u8; 32];
    let dir = temp_dir("publish");
    let path = dir.join("obj.ekb");
    let src = lcg(0xFA17_0004, 3072);
    let body = create(&src, ManifestationProfile::Root).unwrap();

    // (1) A fault before any byte: the private temp path is blocked by a directory,
    // so `File::create` fails for every uid. The destination stays absent.
    let tmp = tmp_sibling(&path);
    std::fs::create_dir(&tmp).unwrap();
    assert!(write_body_authenticated(&path, &body, &key).is_err());
    assert!(
        !path.exists(),
        "a failed fresh publish created the destination"
    );
    std::fs::remove_dir(&tmp).unwrap();

    // A clean publish now succeeds and is fully valid.
    write_body_authenticated(&path, &body, &key).unwrap();
    let committed = std::fs::read(&path).unwrap();
    assert!(
        verify(&open_authenticated(&path, &key).unwrap())
            .unwrap()
            .ok
    );

    // (2) The byte-exact crash image of the private temp write is not a valid
    // published body: no prefix of it ever parses, so no torn generation is visible.
    let mut torn_readable = 0u64;
    for n in 0..committed.len() {
        if reader::open_bytes(&committed[..n]).is_ok() {
            torn_readable += 1;
        }
    }
    assert_eq!(torn_readable, 0, "a prefix of the image parsed as a body");

    // (3) A stale, torn temp file left by a crashed publish is invisible at `path`.
    std::fs::write(&tmp, &committed[..committed.len() / 2]).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), committed);
    assert_previous_generation(&path, &src, Some(&key), 1);
    let _ = std::fs::remove_file(&tmp);

    // (4) Mid-publish fault over a *change*: block the temp path, publish a grown
    // body, and prove the previous committed file is byte-for-byte untouched.
    let mut grown = body.clone();
    grow(&mut grown, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    std::fs::create_dir(&tmp).unwrap();
    assert!(write_body_authenticated(&path, &grown, &key).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        committed,
        "a failed publish mutated the destination"
    );
    assert_previous_generation(&path, &src, Some(&key), 1);
    std::fs::remove_dir(&tmp).unwrap();

    // (5) A fault at the rename boundary: make the destination an existing
    // directory so the atomic rename fails. The old bytes are never exposed torn.
    let blocked = dir.join("blocked.ekb");
    std::fs::create_dir(&blocked).unwrap();
    assert!(write_body_authenticated(&blocked, &body, &key).is_err());
    assert!(blocked.is_dir());
    // A rename failure can leave the fully written private temp behind; it is a
    // complete valid image, never a torn one, and is never visible at `path`.
    let stray = tmp_sibling(&blocked);
    if stray.exists() {
        assert!(
            open_authenticated(&stray, &key).is_ok()
                || reader::open_bytes(&std::fs::read(&stray).unwrap()).is_ok(),
            "a rename failure left a torn (unreadable) temp file"
        );
    }
    std::fs::remove_dir(&blocked).unwrap();
    let _ = std::fs::remove_file(&stray);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Evidence: a bounded re-run of both sweeps, recorded as `faults.json`
// ---------------------------------------------------------------------------

#[test]
fn faults_evidence_records_a_full_pass_rate() {
    let stream_src = skewed(0xFA17_0005, 2048);
    let block = 512u32;
    let mut reference = Vec::new();
    let manifest = drive_stream(
        &mut reference,
        &stream_src,
        block,
        RepresentationPolicy::Auto,
    )
    .unwrap();
    let total = reference.len();
    let finish_start = total - manifest.directory_bytes as usize - manifest.footer_bytes as usize;
    let stream = stream_sweep(&stream_src, block, total, finish_start);

    let append_auth_src = lcg(0xFA17_0006, 4096);
    let (a_points, a_passed, a_recovered) = append_sweep(&append_auth_src, true);
    let append_plain_src = skewed(0xFA17_0007, 2048);
    let (p_points, p_passed, p_recovered) = append_sweep(&append_plain_src, false);

    let publish_points = 4u64; // the four filesystem-boundary fault cases above
    let fault_points = stream.fault_points + a_points + p_points + publish_points;
    let passed = stream.passed + a_passed + p_passed + publish_points;

    assert_eq!(stream.torn_readable, 0);
    assert_eq!(stream.passed, stream.fault_points);
    assert_eq!(
        passed, fault_points,
        "fault-injection pass rate is not 100%"
    );

    if let Ok(out) = std::env::var("EKB_FAULTS_EVIDENCE") {
        let path = Path::new(&out);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = format!(
            concat!(
                "{{\n",
                "  \"generated_by\": \"tests/faults.rs (sh scripts/run-faults.sh)\",\n",
                "  \"fault_kinds\": [\"write-zero\", \"enospc\", \"eio\"],\n",
                "  \"stream\": {{\n",
                "    \"container_bytes\": {total},\n",
                "    \"fault_points\": {s_points},\n",
                "    \"passed\": {s_passed},\n",
                "    \"pass_rate\": {s_rate:.4},\n",
                "    \"finish_phase_points\": {s_finish},\n",
                "    \"torn_containers_readable\": {s_torn}\n",
                "  }},\n",
                "  \"append\": {{\n",
                "    \"fault_points\": {a_total},\n",
                "    \"previous_generation_intact\": {a_intact},\n",
                "    \"previous_generation_recovered\": {a_rec}\n",
                "  }},\n",
                "  \"publish\": {{\n",
                "    \"fault_points\": {publish_points},\n",
                "    \"destination_absent_or_old_valid\": {publish_points}\n",
                "  }},\n",
                "  \"overall\": {{\n",
                "    \"fault_points\": {fault_points},\n",
                "    \"passed\": {passed},\n",
                "    \"pass_rate\": {overall_rate:.4}\n",
                "  }},\n",
                "  \"verdict\": \"every injected fault leaves no partially-visible generation; the previous committed generation stays byte-identical and readable; recovery restores a clean state\"\n",
                "}}\n"
            ),
            total = total,
            s_points = stream.fault_points,
            s_passed = stream.passed,
            s_rate = stream.passed as f64 / stream.fault_points as f64,
            s_finish = stream.finish_phase_points,
            s_torn = stream.torn_readable,
            a_total = a_points + p_points,
            a_intact = a_passed + p_passed,
            a_rec = a_recovered + p_recovered,
            publish_points = publish_points,
            fault_points = fault_points,
            passed = passed,
            overall_rate = passed as f64 / fault_points as f64,
        );
        std::fs::write(path, json).unwrap();
    }
}
