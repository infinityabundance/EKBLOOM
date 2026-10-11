//! Opt-in authenticated mode: keyed BLAKE3 MAC over the `.ekb` prefix.
//!
//! These tests exercise `write_body_authenticated`/`open_authenticated` (and the
//! CLI `--key-hex` path): a genuine round-trip, tamper detection that unkeyed
//! checks miss, wrong-key and missing-`AUTH` rejection, and the guarantee that the
//! default (unkeyed) behaviour is unchanged. All temporary state lives under a
//! unique `std::env::temp_dir()` subdirectory that is removed afterwards.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use ekbloom::format::{self, ManifestRecord, SectionKind};
use ekbloom::{create, grow, read, reader, verify, writer, ManifestationProfile, ReadRequest};

const KEY: [u8; 32] = [0x11; 32];
const KEY2: [u8; 32] = [0x22; 32];

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-auth-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

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

fn full(src: &[u8]) -> ReadRequest {
    ReadRequest {
        offset: 0,
        length: src.len() as u64,
    }
}

/// Number of `AUTH` sections across every committed generation.
fn auth_sections(bytes: &[u8]) -> usize {
    format::scan(bytes)
        .unwrap()
        .commits
        .iter()
        .map(|c| {
            c.sections
                .iter()
                .filter(|s| s.kind == SectionKind::Auth)
                .count()
        })
        .sum()
}

/// Recompute the footer's trailing CRC-32 after patching its content.
fn refresh_footer_checksum(out: &mut [u8], footer_start: usize) {
    let footer = &out[footer_start..footer_start + format::FOOTER_LEN];
    let mut buf = Vec::with_capacity(format::FOOTER_LEN - 4);
    buf.extend_from_slice(&footer[..56]);
    buf.extend_from_slice(&footer[60..]);
    let crc = crc32fast::hash(&buf);
    out[footer_start + 56..footer_start + 60].copy_from_slice(&crc.to_le_bytes());
}

/// Forge one core byte into a *different, internally consistent* unkeyed object:
/// flip the byte, then rewrite the manifest `core_hash`, the manifest section
/// CRC-32, and the footer `manifest_hash`/checksum so every unkeyed check passes.
/// The `AUTH` section is left untouched, so its MAC no longer matches.
fn forge_core_byte(bytes: &[u8], index: usize) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let core_off = format::CORE_OFFSET as usize;
    out[core_off + index] ^= 0xFF;

    let scan = format::scan(&out).unwrap();
    let commit = scan.commits.last().unwrap();
    let core_len = scan.header.core_len as usize;
    let new_core_hash = *blake3::hash(&out[core_off..core_off + core_len]).as_bytes();

    let section = commit
        .sections
        .iter()
        .rev()
        .find(|s| s.kind == SectionKind::Manifest)
        .unwrap();
    let old = format::section_payload(&out, section);
    let mut record = ManifestRecord::decode(old).unwrap();
    record.core_hash = new_core_hash;
    let payload = record.encode();
    assert_eq!(
        payload.len(),
        old.len(),
        "manifest length must be preserved"
    );

    let po = section.payload_offset as usize;
    out[po..po + payload.len()].copy_from_slice(&payload);
    let ho = section.header_offset as usize;
    out[ho + 32..ho + 36].copy_from_slice(&crc32fast::hash(&payload).to_le_bytes());

    let footer_start = commit.end as usize - format::FOOTER_LEN;
    let mut hash16 = [0u8; 16];
    hash16.copy_from_slice(&blake3::hash(&payload).as_bytes()[..16]);
    out[footer_start + 32..footer_start + 48].copy_from_slice(&hash16);
    refresh_footer_checksum(&mut out, footer_start);
    out
}

/// Remove the trailing `AUTH` section, yielding the equivalent unkeyed file.
fn strip_auth(bytes: &[u8]) -> Vec<u8> {
    let scan = format::scan(bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth = commit
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Auth)
        .unwrap();
    let auth_start = auth.header_offset as usize;
    let footer_start = commit.end as usize - format::FOOTER_LEN;
    assert!(auth_start < footer_start);
    let mut out = Vec::with_capacity(bytes.len() - (footer_start - auth_start));
    out.extend_from_slice(&bytes[..auth_start]);
    out.extend_from_slice(&bytes[footer_start..]);
    out
}

#[test]
fn authenticated_roundtrip_reads_and_verifies() {
    let dir = temp_dir("rt");
    let src = lcg(0xA11CE, 1024);
    let path = dir.join("a.ekb");
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();

    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], &format::MAGIC);
    assert_eq!(bytes.len() % 8, 0, "sections stay 8-aligned");
    assert_eq!(auth_sections(&bytes), 1, "exactly one AUTH section");

    let opened = reader::open_authenticated(&path, &KEY).unwrap();
    assert_eq!(read(&opened, full(&src)).unwrap(), src);
    assert!(verify(&opened).unwrap().ok, "authenticated object verifies");

    // The unkeyed open path still works and simply ignores the AUTH section.
    let plain = reader::open(&path).unwrap();
    assert_eq!(read(&plain, full(&src)).unwrap(), src);

    // The streaming file-backed reader also opens it.
    let fb = reader::FileBody::open(&path).unwrap();
    assert_eq!(fb.read(full(&src)).unwrap(), src);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tampered_core_opens_unkeyed_but_fails_authenticated() {
    let dir = temp_dir("tamper-core");
    let src = lcg(0xC0FFEE, 512);
    let path = dir.join("b.ekb");
    let body = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();
    let good = fs::read(&path).unwrap();
    assert!(reader::open_bytes_authenticated(&good, &KEY).is_ok());

    // A forger with write access rewrites the core and every unkeyed digest
    // consistently: the result is a different but valid unkeyed object.
    let forged = forge_core_byte(&good, 7);
    assert!(
        reader::open_bytes(&forged).is_ok(),
        "fully re-forged file still passes unkeyed structural checks"
    );
    // But the keyed MAC no longer matches, so authentication fails.
    assert!(
        matches!(
            reader::open_bytes_authenticated(&forged, &KEY),
            Err(ekbloom::EkbloomError::ChecksumMismatch { section: "auth" })
        ),
        "a re-forged core must fail the keyed MAC"
    );

    // Same verdict through the file API.
    fs::write(&path, &forged).unwrap();
    assert!(reader::open(&path).is_ok());
    assert!(reader::open_authenticated(&path, &KEY).is_err());

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tampered_section_payload_fails_authenticated() {
    let dir = temp_dir("tamper-section");
    let src = lcg(0x5EED, 256);
    let path = dir.join("c.ekb");
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();
    let mut bytes = fs::read(&path).unwrap();

    // Corrupt a checkpoint's first packed restart state (which unkeyed `open`
    // accepts: only the derived cursor is checked there) and refresh that
    // section's CRC-32 so the section still passes structural checks. The compact
    // checkpoint header is 22 bytes (magic ++ version ++ format ++ interval ++
    // count), so the first 2-byte packed state begins at `po + 22`.
    let scan = format::scan(&bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    let section = commit
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Checkpoints)
        .unwrap();
    let po = section.payload_offset as usize;
    bytes[po + 22] ^= 0x80; // first packed restart state (derived cursor left legal)
    let crc = crc32fast::hash(&bytes[po..po + section.payload_len as usize]);
    let ho = section.header_offset as usize;
    bytes[ho + 32..ho + 36].copy_from_slice(&crc.to_le_bytes());

    assert!(
        reader::open_bytes(&bytes).is_ok(),
        "unkeyed open accepts the re-checksummed section"
    );
    assert!(
        reader::open_bytes_authenticated(&bytes, &KEY).is_err(),
        "the MAC covers section payloads, so tampering is detected"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wrong_key_and_missing_or_stripped_auth_fail() {
    let dir = temp_dir("negative");
    let src = lcg(0xBEEF, 300);
    let path = dir.join("d.ekb");
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();
    let bytes = fs::read(&path).unwrap();

    // Wrong key.
    assert!(reader::open_authenticated(&path, &KEY2).is_err());

    // Stripping the AUTH section yields a valid unkeyed file that still cannot be
    // opened authenticated (missing AUTH is a rejection, never a silent downgrade).
    let stripped = strip_auth(&bytes);
    assert!(reader::open_bytes(&stripped).is_ok());
    assert!(
        reader::open_bytes_authenticated(&stripped, &KEY).is_err(),
        "a stripped AUTH section must be rejected"
    );

    // A never-authenticated (unkeyed) file has no AUTH section at all.
    let plain = dir.join("plain.ekb");
    writer::write_body(&plain, &create(&src, ManifestationProfile::Root).unwrap()).unwrap();
    assert_eq!(auth_sections(&fs::read(&plain).unwrap()), 0);
    assert!(
        reader::open_authenticated(&plain, &KEY).is_err(),
        "an unkeyed file cannot be opened authenticated"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn default_unkeyed_files_are_unchanged() {
    let dir = temp_dir("default");
    let src = lcg(0xDEF0, 400);
    let path = dir.join("e.ekb");
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    writer::write_body(&path, &body).unwrap();

    let bytes = fs::read(&path).unwrap();
    assert_eq!(auth_sections(&bytes), 0, "unkeyed write emits no AUTH");

    let opened = reader::open(&path).unwrap();
    assert_eq!(read(&opened, full(&src)).unwrap(), src);
    assert!(verify(&opened).unwrap().ok);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn authenticated_append_then_prior_tamper_is_caught() {
    let dir = temp_dir("append");
    let src = lcg(0xADD, 200);
    let path = dir.join("f.ekb");
    let body = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();

    let mut body = reader::open_authenticated(&path, &KEY).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    let gen = writer::append_generation_authenticated(&path, &body, &KEY).unwrap();
    assert_eq!(gen, body.generation);

    let opened = reader::open_authenticated(&path, &KEY).unwrap();
    assert_eq!(read(&opened, full(&src)).unwrap(), src);

    // Tamper an earlier-generation byte: the newest MAC covers the whole prefix,
    // so authenticated open must fail (no silent fallback to a bad generation).
    let mut bad = fs::read(&path).unwrap();
    bad[format::CORE_OFFSET as usize] ^= 0x01;
    fs::write(&path, &bad).unwrap();
    assert!(reader::open_authenticated(&path, &KEY).is_err());

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_authenticated_create_read_verify() {
    let dir = temp_dir("cli");
    let src = lcg(0xC11, 256);
    let input = dir.join("in.bin");
    fs::write(&input, &src).unwrap();
    let ekb = dir.join("o.ekb");
    let ekb_s = ekb.to_str().unwrap();
    let input_s = input.to_str().unwrap();
    let len = src.len().to_string();
    let key_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let wrong_hex = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

    let out = Command::new(env!("CARGO_BIN_EXE_ekbloom"))
        .args(["create", input_s, "-o", ekb_s, "--key-hex", key_hex])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "create --key-hex failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Same key: read succeeds and returns the source bytes.
    let out = Command::new(env!("CARGO_BIN_EXE_ekbloom"))
        .args([
            "read",
            ekb_s,
            "--offset",
            "0",
            "--length",
            &len,
            "--key-hex",
            key_hex,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "authenticated read failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, src);

    // Wrong key: read fails with a nonzero status.
    let out = Command::new(env!("CARGO_BIN_EXE_ekbloom"))
        .args([
            "read",
            ekb_s,
            "--offset",
            "0",
            "--length",
            &len,
            "--key-hex",
            wrong_hex,
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "wrong key must fail");

    // Malformed key: rejected without echoing the key.
    let out = Command::new(env!("CARGO_BIN_EXE_ekbloom"))
        .args([
            "read",
            ekb_s,
            "--offset",
            "0",
            "--length",
            "1",
            "--key-hex",
            "abcd",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "malformed key must fail");

    // verify --key-hex succeeds; verify without a key still works unkeyed too.
    let out = Command::new(env!("CARGO_BIN_EXE_ekbloom"))
        .args(["verify", ekb_s, "--key-hex", key_hex])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "authenticated verify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    fs::remove_dir_all(&dir).unwrap();
}
