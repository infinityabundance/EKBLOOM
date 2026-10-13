//! Frozen wire-format compatibility fixture emitter.
//!
//! This program mints the `.ekb` files under `tests/fixtures/` from a fixed,
//! deterministic set of sources and records, per fixture, the expectations that
//! a current build must reproduce byte-for-byte (`tests/compat_fixtures.rs`).
//!
//! It is deliberately written against only the API surface common to EKBLOOM
//! 0.3.0, 0.4.0 and the current build, so the *same source file* can be compiled
//! inside a git worktree checked out at either release commit. That is how the
//! genuine old-version fixtures are produced: the old release's own
//! `create_machine` / `write_body` writes the bytes. See `scripts/regen-fixtures.sh`.
//!
//! Usage:
//!
//! ```text
//! emit_fixtures <out_dir> <name_prefix> <writer_label>
//! ```
//!
//! * `out_dir` — directory for the emitted `.ekb` files (e.g. `tests/fixtures`).
//! * `name_prefix` — file-name prefix identifying the writer (e.g. `v0.4_`).
//! * `writer_label` — release whose build wrote the files (e.g. `0.4.0`).
//!
//! It writes one compact JSON expectation object per fixture to
//! `<out_dir>/expected.<writer_label>.jsonl`. `scripts/regen-fixtures.sh` merges
//! the fragments into `<out_dir>/expected.json`. The emitter never writes
//! `expected.json` itself, so the two release runs cannot clobber each other.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::exit;

use ekbloom::{
    byte_at, create_machine, open, verify, verify_deep, write_body, EkbloomBody,
    ManifestationProfile,
};

/// Absolute offset of the `core_version` field in the 80-byte genesis header.
const OFF_CORE_VERSION: usize = 68;
const OFF_FORMAT_MAJOR: usize = 8;
const OFF_FORMAT_MINOR: usize = 10;
const OFF_CORE_MACHINE_ID: usize = 64;

/// One fixture specification: a fixed source, machine and profile.
struct Spec {
    source: &'static str,
    machine: u32,
    profile_label: &'static str,
    profile: ManifestationProfile,
    bytes: Vec<u8>,
}

/// A small deterministic ELF64 prefix (an executable header followed by filler).
fn elf_source() -> Vec<u8> {
    let mut v = Vec::with_capacity(256);
    v.extend_from_slice(b"\x7fELF\x02\x01\x01\x00");
    v.extend_from_slice(&[0u8; 8]);
    v.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    v.extend_from_slice(&0x3eu16.to_le_bytes()); // e_machine = EM_X86_64
    v.extend_from_slice(&1u32.to_le_bytes()); // e_version
    v.extend_from_slice(&0x40_0000u64.to_le_bytes()); // e_entry
    v.extend_from_slice(&64u64.to_le_bytes()); // e_phoff
    v.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    while v.len() < 256 {
        let i = v.len() as u8;
        v.push(i.wrapping_mul(31).wrapping_add(7));
    }
    v.truncate(256);
    v
}

/// A deterministic ASCII text prefix.
fn text_source() -> Vec<u8> {
    const BASE: &str = "EKBLOOM frozen compatibility fixture. The quick brown fox jumps over \
the lazy dog 0123456789. ";
    let mut v = Vec::with_capacity(512);
    while v.len() < 512 {
        v.extend_from_slice(BASE.as_bytes());
    }
    v.truncate(512);
    v
}

/// A skewed 4-symbol blob — what the compressing machines are built for.
fn skew_source() -> Vec<u8> {
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(4096);
    for _ in 0..4096 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(alpha[idx]);
    }
    out
}

/// The fixed fixture set. Adding a fixture here forces a `regen-fixtures.sh` run
/// and a `docs/MIGRATION.md` entry; removing one is never allowed.
fn specs() -> Vec<Spec> {
    vec![
        Spec {
            source: "elf",
            machine: 1,
            profile_label: "root",
            profile: ManifestationProfile::Root,
            bytes: elf_source(),
        },
        Spec {
            source: "text",
            machine: 2,
            profile_label: "root",
            profile: ManifestationProfile::Root,
            bytes: text_source(),
        },
        Spec {
            source: "skew",
            machine: 2,
            profile_label: "root",
            profile: ManifestationProfile::Root,
            bytes: skew_source(),
        },
        Spec {
            source: "skew",
            machine: 2,
            profile_label: "path",
            profile: ManifestationProfile::Path { depth: 4 },
            bytes: skew_source(),
        },
        Spec {
            source: "empty",
            machine: 1,
            profile_label: "root",
            profile: ManifestationProfile::Root,
            bytes: Vec::new(),
        },
        Spec {
            source: "one",
            machine: 1,
            profile_label: "root",
            profile: ManifestationProfile::Root,
            bytes: vec![0x5Au8],
        },
    ]
}

/// A deterministic spread of sampled logical positions covering `[0, n)`.
fn sample_offsets(n: u64) -> Vec<u64> {
    if n == 0 {
        return Vec::new();
    }
    let mut v: Vec<u64> = vec![0];
    if n > 1 {
        v.push(n - 1);
    }
    for k in 1..=10u64 {
        v.push(n.saturating_mul(k) / 11);
    }
    v.retain(|&o| o < n);
    v.sort_unstable();
    v.dedup();
    v
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0x0f) as usize] as char);
    }
    s
}

fn u16_at(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("emit_fixtures: {msg}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        die("usage: emit_fixtures <out_dir> <name_prefix> <writer_label>");
    }
    let out_dir = PathBuf::from(&args[1]);
    let prefix = &args[2];
    let writer = &args[3];
    if let Err(e) = fs::create_dir_all(&out_dir) {
        die(format!("create {}: {e}", out_dir.display()));
    }

    let mut jsonl = String::new();
    let mut emitted = 0usize;

    for spec in specs() {
        let name = format!(
            "{prefix}{}_{}_m{}",
            spec.source, spec.profile_label, spec.machine
        );
        let file_name = format!("{name}.ekb");
        let path = out_dir.join(&file_name);

        let body: EkbloomBody = match create_machine(&spec.bytes, spec.profile, spec.machine) {
            Ok(b) => b,
            Err(e) => die(format!("create {name}: {e}")),
        };
        if let Err(e) = write_body(&path, &body) {
            die(format!("write {name}: {e}"));
        }

        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => die(format!("read back {name}: {e}")),
        };
        let n = spec.bytes.len() as u64;

        // Header cross-check: what the old writer put on disk.
        if &raw[0..8] != b"EKBLOOM\0" {
            die(format!("{name}: bad magic"));
        }
        let format_major = u16_at(&raw, OFF_FORMAT_MAJOR);
        let format_minor = u16_at(&raw, OFF_FORMAT_MINOR);
        let header_machine = u32_at(&raw, OFF_CORE_MACHINE_ID);
        let core_version = u16_at(&raw, OFF_CORE_VERSION);
        if header_machine != spec.machine {
            die(format!("{name}: header machine {header_machine}"));
        }

        // The old release must be able to reopen and verify what it wrote.
        let opened = match open(&path) {
            Ok(b) => b,
            Err(e) => die(format!("reopen {name}: {e}")),
        };
        if opened.logical_len != n || opened.machine_id != spec.machine {
            die(format!("{name}: reopen metadata mismatch"));
        }
        match verify(&opened) {
            Ok(r) if r.ok => {}
            other => die(format!("{name}: verify: {other:?}")),
        }
        match verify_deep(&opened) {
            Ok(r) if r.ok => {}
            other => die(format!("{name}: verify_deep: {other:?}")),
        }

        // Sample the logical content through the old read path and hash the
        // ground-truth bytes. A current build must reproduce both exactly.
        let offsets = sample_offsets(n);
        let mut sampled: Vec<u8> = Vec::with_capacity(offsets.len());
        for &off in &offsets {
            match byte_at(&opened, off) {
                Ok(b) if b == spec.bytes[off as usize] => sampled.push(b),
                Ok(b) => die(format!(
                    "{name}: byte_at({off}) = {b} != source {}",
                    spec.bytes[off as usize]
                )),
                Err(e) => die(format!("{name}: byte_at({off}): {e}")),
            }
        }
        let sample_hash = hex(blake3::hash(&sampled).as_bytes());

        let physical_bytes = body.physical_bytes();
        let file_bytes = raw.len() as u64;

        write!(
            jsonl,
            "{{\"name\":\"{name}\",\"file\":\"{file_name}\",\"writer\":\"{writer}\",\
\"source\":\"{}\",\"profile\":\"{}\",\"machine_id\":{},\
\"format_major\":{format_major},\"format_minor\":{format_minor},\
\"core_version\":{core_version},\"core_len\":{},\"logical_len\":{n},\
\"physical_bytes\":{physical_bytes},\"file_bytes\":{file_bytes},\
\"samples\":[",
            spec.source,
            spec.profile_label,
            spec.machine,
            body.core.len(),
        )
        .unwrap();
        for (i, (off, byte)) in offsets.iter().zip(sampled.iter()).enumerate() {
            if i > 0 {
                jsonl.push(',');
            }
            write!(jsonl, "[{off},{byte}]").unwrap();
        }
        writeln!(jsonl, "]\"sample_hash\":\"{sample_hash}\"}}").unwrap();

        emitted += 1;
    }

    let fragment = out_dir.join(format!("expected.{writer}.jsonl"));
    if let Err(e) = write_fragment(&fragment, &jsonl) {
        die(format!("write {}: {e}", fragment.display()));
    }
    println!(
        "emit_fixtures: wrote {emitted} fixtures ({writer}) to {}",
        out_dir.display()
    );
}

fn write_fragment(path: &Path, jsonl: &str) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(jsonl.as_bytes())?;
    f.flush()
}
