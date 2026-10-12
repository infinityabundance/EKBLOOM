//! Integrity -> authenticity coverage of the opt-in `AUTH` keyed-BLAKE3 MAC.
//!
//! The MAC covers the exact bytes `file[0 .. auth_header_offset)` — the genesis
//! header, the core, all alignment padding, and **every** section written before
//! the `AUTH` section (history, checkpoints/packed, every bloom shell including
//! regional, certificates, tiles, blocks, and the manifest). These tests pin that
//! coverage by tampering **each** structure kind (with its section CRC-32
//! recomputed so the structural scan still passes) and asserting that
//! `reader::open_require_auth` / the strict authenticated `verify` reject it.
//!
//! They also pin the strict mode: an unkeyed file with no `AUTH` is rejected, a
//! wrong key is rejected, stripping `AUTH` is rejected, and honest files pass.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use ekbloom::format::{self, SectionKind};
use ekbloom::{
    create, manifest, read, reader, verify, verify_file_require_auth, writer, AccessTrace,
    BlockIndex, CheckpointSet, DirectoryKind, EkbloomBody, EkbloomError, ManifestationBudget,
    ManifestationProfile, ReadRequest, TileSet,
};

const KEY: [u8; 32] = [0x11; 32];
const KEY2: [u8; 32] = [0x22; 32];

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-authcov-{tag}-{}-{}",
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

/// A body carrying a persisted tile set over the first `hot` bytes.
fn with_tiles(mut body: EkbloomBody, hot: u64) -> EkbloomBody {
    let n = body.logical_len;
    let regions: Vec<(u64, u64)> = (0..hot.div_ceil(4096)).map(|k| (k * 4096, 4096)).collect();
    let counts: Vec<u64> = (0..regions.len() as u64).map(|k| 10_000 - k).collect();
    let report = TileSet::manifest_hot_tiles(
        &mut body,
        &AccessTrace { regions, counts },
        12,
        DirectoryKind::Flat,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();
    assert!(report.certified);
    let _ = n;
    body
}

/// A body carrying an independently restartable block index.
fn with_blocks(mut body: EkbloomBody) -> EkbloomBody {
    let index = BlockIndex::build(&body.core, body.logical_len, 64).unwrap();
    body.blocks.push(index);
    assert!(verify(&body).unwrap().ok, "block body must verify");
    body
}

/// A body carrying a packed checkpoint set (in addition to whatever the profile
/// produced).
fn with_checkpoint(mut body: EkbloomBody, interval: u32) -> EkbloomBody {
    let cp = CheckpointSet::build(&body.core, body.logical_len, interval).unwrap();
    body.checkpoints.push(cp);
    assert!(verify(&body).unwrap().ok, "checkpoint body must verify");
    body
}

/// A body carrying a regional bloom shell.
fn with_regional(body: EkbloomBody) -> EkbloomBody {
    let mut body = body;
    manifest(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 512,
        },
    )
    .unwrap();
    assert!(verify(&body).unwrap().ok, "regional body must verify");
    body
}

/// Flip one payload byte of the first section of `kind` in the newest commit and
/// recompute that section's CRC-32, so the structural scan still accepts the
/// file and the *only* check that can catch the tamper is the keyed MAC.
///
/// Returns the tampered bytes. Panics if there is no such section.
fn tamper_section(bytes: &[u8], kind: SectionKind) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let scan = format::scan(&out).unwrap();
    let commit = scan.commits.last().unwrap();
    let section = commit
        .sections
        .iter()
        .find(|s| s.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} section to tamper"));
    assert!(section.payload_len > 0, "{kind:?} payload is empty");
    let po = section.payload_offset as usize;
    let plen = section.payload_len as usize;
    out[po + plen / 2] ^= 0xFF;
    let crc = crc32fast::hash(&out[po..po + plen]);
    let ho = section.header_offset as usize;
    out[ho + 32..ho + 36].copy_from_slice(&crc.to_le_bytes());
    // The tampered file must still scan: that isolates the MAC as the detector.
    assert!(
        format::scan(&out).is_ok(),
        "re-checksummed {kind:?} tamper must still scan"
    );
    out
}

/// Assert that `bytes` (a tampered authenticated image) is rejected by strict
/// authenticated open with the keyed-MAC error, and that the strict verify also
/// fails.
fn assert_mac_rejects(bytes: &[u8], tag: &str) {
    match reader::open_bytes_require_auth(bytes, &KEY) {
        Err(EkbloomError::ChecksumMismatch { section: "auth" }) => {}
        other => panic!("{tag}: expected an AUTH MAC rejection, got {other:?}"),
    }
    // Round-trip through a file for the strict `verify` entry point.
    let dir = temp_dir(tag);
    let path = dir.join("t.ekb");
    fs::write(&path, bytes).unwrap();
    assert!(
        verify_file_require_auth(&path, &KEY, false).is_err(),
        "{tag}: strict verify must reject the tamper"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// One tamper case: build `body`, write it authenticated, tamper the first
/// section of `kind`, and assert the MAC catches it. Also asserts the honest file
/// passes strict open first, so a rejection is unambiguous.
fn check_tamper(tag: &str, body: &EkbloomBody, kind: SectionKind) {
    let dir = temp_dir(tag);
    let path = dir.join("o.ekb");
    writer::write_body_authenticated(&path, body, &KEY).unwrap();
    let good = fs::read(&path).unwrap();

    // Honest file: strict open + strict verify pass; read is byte-exact.
    let opened = reader::open_require_auth(&path, &KEY).unwrap();
    assert_eq!(
        read(
            &opened,
            ReadRequest {
                offset: 0,
                length: body.logical_len
            }
        )
        .unwrap()
        .len() as u64,
        body.logical_len
    );
    assert!(verify_file_require_auth(&path, &KEY, true).unwrap().ok);

    let tampered = tamper_section(&good, kind);
    assert_mac_rejects(&tampered, tag);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn auth_mac_covers_every_section_of_a_rich_body() {
    // One authenticated body carrying history, a checkpoint, a path bloom and a
    // regional bloom (MaxSpeed + Regional), plus tiles and blocks.
    let src = lcg(0x1234_5678, 96 * 1024);
    let body = create(&src, ManifestationProfile::MaxSpeed).unwrap();
    let body = with_checkpoint(body, 256);
    let body = with_regional(body);
    let body = with_tiles(body, 16 * 1024);
    let body = with_blocks(body);
    let mut body = body;
    // Add a universal transition bloom so every writable bloom kind is present.
    manifest(&mut body, ManifestationProfile::Bloom { depth: 4 }).unwrap();

    let dir = temp_dir("coverage");
    let path = dir.join("rich.ekb");
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();
    let bytes = fs::read(&path).unwrap();

    let scan = format::scan(&bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth_idx = commit
        .sections
        .iter()
        .position(|s| s.kind == SectionKind::Auth)
        .expect("an AUTH section");
    // AUTH is the final section of the commit.
    assert_eq!(auth_idx, commit.sections.len() - 1);
    let auth = commit.sections[auth_idx];
    let covered_end = auth.header_offset;

    // Every other section — of every kind present — lies wholly inside the
    // covered range `[0, auth.header_offset)`.
    let mut kinds: Vec<SectionKind> = Vec::new();
    for section in &commit.sections[..auth_idx] {
        let payload_end = section.payload_offset + section.payload_len;
        assert!(
            section.header_offset + format::SECTION_HEADER_LEN as u64 <= covered_end
                && payload_end <= covered_end,
            "{:?} is not fully covered by AUTH",
            section.kind
        );
        kinds.push(section.kind);
    }
    // The rich body exercises the newer sections the reviewer named, plus every
    // writable bloom kind (global, regional, path).
    for required in [
        SectionKind::HistoryFull,
        SectionKind::Checkpoints,
        SectionKind::BloomGlobal,
        SectionKind::BloomPath,
        SectionKind::BloomRegion,
        SectionKind::Tiles,
        SectionKind::Blocks,
        SectionKind::Manifest,
    ] {
        assert!(
            kinds.contains(&required),
            "rich body must contain a {required:?} section"
        );
    }
    // No section kind is present that is not MAC-covered: `AUTH` itself is the
    // only section after the covered range, and it is the keyed MAC.
    assert!(!kinds.contains(&SectionKind::Auth));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tamper_history_is_rejected_by_auth() {
    let src = lcg(0x4A17, 8 * 1024);
    let body = create(&src, ManifestationProfile::Full).unwrap();
    check_tamper("history", &body, SectionKind::HistoryFull);
}

#[test]
fn tamper_checkpoint_is_rejected_by_auth() {
    // `Sparse` emits a compact (packed) checkpoint set.
    let src = lcg(0x0C0, 8 * 1024);
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    check_tamper("checkpoint", &body, SectionKind::Checkpoints);
}

#[test]
fn tamper_path_bloom_is_rejected_by_auth() {
    let src = lcg(0x0B10, 8 * 1024);
    let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    check_tamper("path-bloom", &body, SectionKind::BloomPath);
}

#[test]
fn tamper_transition_bloom_is_rejected_by_auth() {
    let src = lcg(0x0B20, 8 * 1024);
    let body = create(&src, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    check_tamper("bloom", &body, SectionKind::BloomGlobal);
}

#[test]
fn tamper_regional_bloom_is_rejected_by_auth() {
    let src = lcg(0x0B30, 8 * 1024);
    let body = create(
        &src,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 512,
        },
    )
    .unwrap();
    check_tamper("regional", &body, SectionKind::BloomRegion);
}

#[test]
fn tamper_tiles_are_rejected_by_auth() {
    let src = lcg(0x711E, 64 * 1024);
    let body = with_tiles(create(&src, ManifestationProfile::Root).unwrap(), 16 * 1024);
    check_tamper("tiles", &body, SectionKind::Tiles);
}

#[test]
fn tamper_blocks_are_rejected_by_auth() {
    let src = lcg(0xB10C, 16 * 1024);
    let body = with_blocks(create(&src, ManifestationProfile::Root).unwrap());
    check_tamper("blocks", &body, SectionKind::Blocks);
}

/// The manifest is MAC-covered: a forger rewrites the core *and* every unkeyed
/// digest consistently (manifest `core_hash`, manifest CRC, footer
/// `manifest_hash`/checksum), so the file is a valid *unkeyed* object, yet the
/// keyed MAC — which covers the manifest too — rejects it.
#[test]
fn tamper_core_and_manifest_consistently_is_rejected_by_auth() {
    let dir = temp_dir("core-manifest");
    let src = lcg(0xC0DE, 2 * 1024);
    let path = dir.join("o.ekb");
    let body = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();

    let bytes = fs::read(&path).unwrap();
    let forged = forge_core_byte(&bytes, 5);
    // The forge is fully consistent for the unkeyed checks...
    assert!(reader::open_bytes(&forged).is_ok());
    // ...but the AUTH MAC, which covers the core *and* the manifest, rejects it.
    assert_mac_rejects(&forged, "core-manifest");
    fs::remove_dir_all(&dir).unwrap();
}

/// Rewrite one core byte and all unkeyed digests so the unkeyed checks pass.
fn forge_core_byte(bytes: &[u8], index: usize) -> Vec<u8> {
    use ekbloom::format::ManifestRecord;
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
    assert_eq!(payload.len(), old.len(), "manifest length is preserved");

    let po = section.payload_offset as usize;
    out[po..po + payload.len()].copy_from_slice(&payload);
    let ho = section.header_offset as usize;
    out[ho + 32..ho + 36].copy_from_slice(&crc32fast::hash(&payload).to_le_bytes());

    let footer_start = commit.end as usize - format::FOOTER_LEN;
    let mut hash16 = [0u8; 16];
    hash16.copy_from_slice(&blake3::hash(&payload).as_bytes()[..16]);
    out[footer_start + 32..footer_start + 48].copy_from_slice(&hash16);
    let footer = &out[footer_start..footer_start + format::FOOTER_LEN];
    let mut buf = Vec::with_capacity(format::FOOTER_LEN - 4);
    buf.extend_from_slice(&footer[..56]);
    buf.extend_from_slice(&footer[60..]);
    let crc = crc32fast::hash(&buf);
    out[footer_start + 56..footer_start + 60].copy_from_slice(&crc.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// Strict mode
// ---------------------------------------------------------------------------

#[test]
fn strict_mode_rejects_unkeyed_and_wrong_key_files_but_accepts_honest() {
    let dir = temp_dir("strict");
    let src = lcg(0x57B1, 4 * 1024);
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();

    // An unkeyed file (no AUTH at all) is rejected in strict mode.
    let plain = dir.join("plain.ekb");
    writer::write_body(&plain, &body).unwrap();
    assert!(
        reader::open_require_auth(&plain, &KEY).is_err(),
        "an unkeyed file must be rejected by open_require_auth"
    );
    assert!(verify_file_require_auth(&plain, &KEY, false).is_err());

    // An honest authenticated file passes strict open and strict verify.
    let auth = dir.join("auth.ekb");
    writer::write_body_authenticated(&auth, &body, &KEY).unwrap();
    assert!(reader::open_require_auth(&auth, &KEY).is_ok());
    assert!(verify_file_require_auth(&auth, &KEY, false).unwrap().ok);
    assert!(verify_file_require_auth(&auth, &KEY, true).unwrap().ok);

    // The wrong key is rejected even though the file is genuinely authenticated.
    assert!(reader::open_require_auth(&auth, &KEY2).is_err());
    assert!(verify_file_require_auth(&auth, &KEY2, false).is_err());

    // Stripping the AUTH section turns the honest file into an unkeyed one that
    // strict mode rejects.
    let bytes = fs::read(&auth).unwrap();
    let stripped = strip_auth(&bytes);
    assert!(reader::open_bytes(&stripped).is_ok());
    assert!(reader::open_bytes_require_auth(&stripped, &KEY).is_err());

    fs::remove_dir_all(&dir).unwrap();
}

/// Remove the trailing AUTH section, yielding the equivalent unkeyed file.
fn strip_auth(bytes: &[u8]) -> Vec<u8> {
    let scan = format::scan(bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth = commit
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Auth)
        .expect("an AUTH section");
    let auth_start = auth.header_offset as usize;
    let footer_start = commit.end as usize - format::FOOTER_LEN;
    let mut out = Vec::with_capacity(bytes.len() - (footer_start - auth_start));
    out.extend_from_slice(&bytes[..auth_start]);
    out.extend_from_slice(&bytes[footer_start..]);
    out
}

#[test]
fn cli_require_auth_demands_a_key_and_rejects_unauthenticated() {
    let dir = temp_dir("cli-strict");
    let src = lcg(0xC11A, 512);
    let input = dir.join("in.bin");
    fs::write(&input, &src).unwrap();
    let unkeyed = dir.join("unkeyed.ekb");
    let authed = dir.join("authed.ekb");
    let input_s = input.to_str().unwrap();
    let unkeyed_s = unkeyed.to_str().unwrap();
    let authed_s = authed.to_str().unwrap();
    let len = src.len().to_string();
    let key_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ekbloom"))
            .args(args)
            .output()
            .unwrap()
    };

    // Create an unkeyed and an authenticated object.
    assert!(run(&["create", input_s, "-o", unkeyed_s]).status.success());
    assert!(
        run(&["create", input_s, "-o", authed_s, "--key-hex", key_hex])
            .status
            .success()
    );

    // --require-auth without a key is a usage error.
    assert!(
        !run(&[
            "read",
            authed_s,
            "--offset",
            "0",
            "--length",
            &len,
            "--require-auth"
        ])
        .status
        .success(),
        "--require-auth without a key must fail"
    );

    // --require-auth on an unkeyed file fails even with a key.
    assert!(
        !run(&[
            "read",
            unkeyed_s,
            "--offset",
            "0",
            "--length",
            &len,
            "--key-hex",
            key_hex,
            "--require-auth",
        ])
        .status
        .success(),
        "--require-auth must reject an unauthenticated file"
    );

    // --require-auth on the honest authenticated file succeeds and is byte-exact.
    let out = run(&[
        "read",
        authed_s,
        "--offset",
        "0",
        "--length",
        &len,
        "--key-hex",
        key_hex,
        "--require-auth",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, src);

    // `verify --require-auth` on both files: reject unkeyed, accept authed.
    assert!(
        !run(&["verify", unkeyed_s, "--key-hex", key_hex, "--require-auth"])
            .status
            .success()
    );
    assert!(
        run(&["verify", authed_s, "--key-hex", key_hex, "--require-auth"])
            .status
            .success()
    );

    // `inspect --require-auth` likewise.
    assert!(
        !run(&["inspect", unkeyed_s, "--key-hex", key_hex, "--require-auth"])
            .status
            .success()
    );
    assert!(
        run(&["inspect", authed_s, "--key-hex", key_hex, "--require-auth"])
            .status
            .success()
    );

    fs::remove_dir_all(&dir).unwrap();
}
