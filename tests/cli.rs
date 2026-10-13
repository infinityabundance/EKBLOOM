//! End-to-end tests for the `ekbloom` CLI binary.
//!
//! These spawn the compiled binary via [`env!("CARGO_BIN_EXE_ekbloom")`] and use
//! only temporary files, cleaned up afterwards. They exercise the documented
//! command surface (`create`/`inspect`/`verify`/`read`/`manifest`/`grow`/
//! `certify`/`recover`/`bench`) plus the two failure paths required by the
//! acceptance criteria (unknown profile and missing file).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// The compiled CLI binary under test.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ekbloom")
}

/// A unique temporary directory for one test.
fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-cli-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the CLI with `args` and capture its output.
fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to spawn ekbloom")
}

/// Convenience: a command's stdout as a string.
fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Convenience: a command's stderr as a string.
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn end_to_end_command_surface() {
    let dir = temp_dir("e2e");
    let src: Vec<u8> = (0..300u32)
        .map(|i| (i.wrapping_mul(7).wrapping_add(3)) as u8)
        .collect();
    let input = dir.join("input.bin");
    fs::write(&input, &src).unwrap();
    let ekb = dir.join("obj.ekb");
    let input_s = input.to_str().unwrap();
    let ekb_s = ekb.to_str().unwrap();

    // create
    let out = run(&["create", input_s, "-o", ekb_s, "--profile", "root"]);
    assert!(out.status.success(), "create failed: {}", stderr(&out));
    assert!(ekb.exists(), "create did not produce the output file");

    // inspect --json
    let out = run(&["inspect", ekb_s, "--json"]);
    assert!(
        out.status.success(),
        "inspect --json failed: {}",
        stderr(&out)
    );
    let json = stdout(&out);
    for key in [
        "\"logical_len\"",
        "\"visible_bytes\"",
        "\"physical_bytes\"",
        "\"concealed_bytes\"",
        "\"expansion_ratio\"",
        "\"generation\"",
        "\"profiles\"",
        "\"components\"",
        "\"name\"",
        "\"kind\"",
        "\"bytes\"",
    ] {
        assert!(json.contains(key), "inspect json missing {key}: {json}");
    }

    // verify
    let out = run(&["verify", ekb_s]);
    assert!(out.status.success(), "verify failed: {}", stderr(&out));
    assert!(stdout(&out).contains("OK"), "verify did not report OK");

    // read the whole object to stdout
    let length = src.len().to_string();
    let out = run(&["read", ekb_s, "--offset", "0", "--length", length.as_str()]);
    assert!(out.status.success(), "read failed: {}", stderr(&out));
    assert_eq!(out.stdout, src, "whole-object read mismatch");

    // read a sub-range to a file
    let sub = dir.join("sub.bin");
    let sub_s = sub.to_str().unwrap();
    let out = run(&[
        "read", ekb_s, "--offset", "10", "--length", "50", "-o", sub_s,
    ]);
    assert!(
        out.status.success(),
        "sub-range read failed: {}",
        stderr(&out)
    );
    assert_eq!(fs::read(&sub).unwrap(), src[10..60].to_vec());

    // manifest a path bloom (additive generation)
    let out = run(&["manifest", ekb_s, "--profile", "path", "--depth", "4"]);
    assert!(out.status.success(), "manifest failed: {}", stderr(&out));

    // grow with a universal bloom
    let out = run(&["grow", ekb_s, "--profile", "bloom4"]);
    assert!(out.status.success(), "grow failed: {}", stderr(&out));

    // certify all constructions
    let out = run(&["certify", ekb_s]);
    assert!(out.status.success(), "certify failed: {}", stderr(&out));

    // recover
    let out = run(&["recover", ekb_s]);
    assert!(out.status.success(), "recover failed: {}", stderr(&out));

    // bench
    let out = run(&[
        "bench",
        ekb_s,
        "--workload",
        "random-1b",
        "--iterations",
        "100",
    ]);
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("p50"),
        "bench output missing p50: {}",
        stdout(&out)
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_profile_is_rejected() {
    let dir = temp_dir("badprofile");
    let input = dir.join("in.bin");
    fs::write(&input, b"hello").unwrap();
    let ekb = dir.join("out.ekb");

    let out = run(&[
        "create",
        input.to_str().unwrap(),
        "-o",
        ekb.to_str().unwrap(),
        "--profile",
        "bogus",
    ]);
    assert!(!out.status.success(), "unknown profile should fail");
    assert!(!stderr(&out).is_empty(), "failure should explain itself");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bench_reports_in_memory_and_file_backed() {
    let dir = temp_dir("bench");
    let src: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect();
    let input = dir.join("in.bin");
    fs::write(&input, &src).unwrap();
    let ekb = dir.join("o.ekb");
    let out = run(&[
        "create",
        input.to_str().unwrap(),
        "-o",
        ekb.to_str().unwrap(),
        "--profile",
        "path",
        "--depth",
        "4",
    ]);
    assert!(out.status.success(), "create failed: {}", stderr(&out));

    // Human output labels both the in-memory fast path and the file-backed path.
    let out = run(&[
        "bench",
        ekb.to_str().unwrap(),
        "--workload",
        "random-1b",
        "--iterations",
        "100",
    ]);
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("in-memory"), "missing in-memory: {text}");
    assert!(text.contains("file-backed"), "missing file-backed: {text}");
    assert!(text.contains("p50_us"), "missing p50_us: {text}");

    // JSON output carries both labelled objects.
    let out = run(&[
        "bench",
        ekb.to_str().unwrap(),
        "--workload",
        "seq",
        "--iterations",
        "5",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "bench --json failed: {}",
        stderr(&out)
    );
    let json = stdout(&out);
    assert!(
        json.contains("\"in-memory\""),
        "json missing in-memory: {json}"
    );
    assert!(
        json.contains("\"in-memory\""),
        "json missing in-memory: {json}"
    );
    assert!(
        json.contains("\"file-backed\""),
        "json missing file-backed: {json}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dict_train_and_create_with_dict_roundtrip() {
    let dir = temp_dir("dict");
    // A corpus of small, text-like records (one newline-separated line each).
    let mut corpus = Vec::new();
    for i in 0..64u32 {
        corpus.extend_from_slice(format!("id={i}|user=alice|region=us-east-1\n").as_bytes());
    }
    let corpus_path = dir.join("corpus.txt");
    fs::write(&corpus_path, &corpus).unwrap();
    let dict_path = dir.join("shared.dict");

    // Train the shared dictionary.
    let out = run(&[
        "dict",
        "train",
        corpus_path.to_str().unwrap(),
        "-o",
        dict_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "dict train failed: {}", stderr(&out));
    assert!(dict_path.exists(), "dict train produced no file");
    assert!(
        stdout(&out).contains("dictionary_bytes: 540"),
        "dict output: {}",
        stdout(&out)
    );

    // Create a self-contained machine-9 object with `--dict` (embedded).
    let record = b"id=999|user=alice|region=us-east-1";
    let input = dir.join("record.bin");
    fs::write(&input, record).unwrap();
    let ekb = dir.join("record.ekb");
    let out = run(&[
        "create",
        input.to_str().unwrap(),
        "-o",
        ekb.to_str().unwrap(),
        "--dict",
        dict_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "create --dict failed: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("machine: 9"),
        "create output: {}",
        stdout(&out)
    );

    // The embedded dictionary makes the file decodable with no registry.
    let length = record.len().to_string();
    let out = run(&[
        "read",
        ekb.to_str().unwrap(),
        "--offset",
        "0",
        "--length",
        length.as_str(),
    ]);
    assert!(out.status.success(), "read failed: {}", stderr(&out));
    assert_eq!(out.stdout, record, "dict roundtrip mismatch");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_file_is_rejected() {
    let dir = temp_dir("missing");
    let missing = dir.join("nope.ekb");

    let out = run(&["inspect", missing.to_str().unwrap()]);
    assert!(!out.status.success(), "missing file should fail");
    assert!(!stderr(&out).is_empty(), "failure should explain itself");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stream_subcommand_builds_reads_and_reports() {
    let dir = temp_dir("stream");
    let src: Vec<u8> = (0..50_000u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect();
    let input = dir.join("input.bin");
    fs::write(&input, &src).unwrap();
    let ekb = dir.join("stream.ekb");
    let input_s = input.to_str().unwrap();
    let ekb_s = ekb.to_str().unwrap();

    // Build a streaming container with 4 KiB blocks.
    let out = run(&[
        "stream",
        input_s,
        "-o",
        ekb_s,
        "--block-bytes",
        "4096",
        "--machine",
        "auto",
    ]);
    assert!(
        out.status.success(),
        "stream build failed: {}",
        stderr(&out)
    );
    assert!(ekb.exists(), "stream build produced no file");
    assert!(stdout(&out).contains("logical_len: 50000"));

    // Inspect.
    let out = run(&["stream", ekb_s, "--info"]);
    assert!(out.status.success(), "stream info failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("logical_len: 50000"), "info: {text}");
    assert!(text.contains("blocks: "), "info missing blocks: {text}");

    // Range read to a file, compared byte-for-byte.
    let range = dir.join("range.bin");
    let out = run(&[
        "stream",
        ekb_s,
        "--offset",
        "12345",
        "--length",
        "4096",
        "-o",
        range.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "stream read failed: {}", stderr(&out));
    assert_eq!(fs::read(&range).unwrap(), src[12345..12345 + 4096].to_vec());

    let _ = fs::remove_dir_all(&dir);
}
