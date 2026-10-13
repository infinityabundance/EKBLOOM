//! Explicit wire-format version tests.
//!
//! Pins the versioning contract of the `.ekb` format:
//!
//! - an unknown `format_major`/`format_minor` is rejected with
//!   [`EkbloomError::UnsupportedVersion`];
//! - an unknown per-machine `core_version` is rejected with
//!   [`EkbloomError::UnsupportedCoreVersion`];
//! - an unknown manifest record version is rejected with
//!   [`EkbloomError::UnsupportedManifestVersion`];
//! - every known version reads byte-exactly and round-trips write -> open ->
//!   verify for machines 1..8, with a byte-identical machine core;
//! - a crafted file claiming a *future* version is refused (never best-effort).
//!
//! A legacy file (written before the `core_version` field existed) leaves the
//! formerly-reserved header bytes zero, which decodes as
//! [`format::CORE_VERSION_LEGACY`] and must still read.

use std::path::{Path, PathBuf};

use ekbloom::format::{
    self, ManifestComponent, ManifestRecord, CORE_VERSION_CURRENT, CORE_VERSION_LEGACY,
    FORMAT_MAJOR, FORMAT_MINOR, HEADER_LEN, MANIFEST_RECORD_VERSION, SUPPORTED_CORE_VERSIONS,
};
use ekbloom::{
    create_machine, machine_by_id, open, read_file, verify, write_body, EkbloomError,
    ManifestationProfile, ReadRequest,
};

/// Header field offsets (little-endian; see `docs/CORE-LAYOUT.md`).
const OFF_FORMAT_MAJOR: usize = 8;
const OFF_FORMAT_MINOR: usize = 10;
const OFF_CORE_MACHINE_ID: usize = 64;
const OFF_CORE_VERSION: usize = 68;

/// A skewed 4 KiB source over a 4-symbol alphabet: skew is what the compressing
/// machines (2/4/6/7/8) are built for, and it also exercises packing (5).
fn skewed_source() -> Vec<u8> {
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

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-format-version-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a `Root` file for `machine_id` and return its path and bytes.
fn write_machine(dir: &Path, machine_id: u32, src: &[u8]) -> (PathBuf, Vec<u8>) {
    let path = dir.join(format!("m{machine_id}.ekb"));
    let body = create_machine(src, ManifestationProfile::Root, machine_id).unwrap();
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    (path, bytes)
}

fn put_u16(bytes: &mut [u8], off: usize, v: u16) {
    bytes[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn get_u16(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn get_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

#[test]
fn supported_sets_and_predicate() {
    // Every machine 1..8 supports exactly the legacy and current core versions.
    for id in 1..=8u32 {
        assert_eq!(format::supported_core_versions(id), SUPPORTED_CORE_VERSIONS);
        assert!(format::core_version_supported(id, CORE_VERSION_CURRENT));
        assert!(format::core_version_supported(id, CORE_VERSION_LEGACY));
        assert!(!format::core_version_supported(id, 2));
        assert_eq!(format::core_version_for_machine(id), CORE_VERSION_CURRENT);
    }
    // An unknown machine has no supported core version.
    assert!(format::supported_core_versions(99).is_empty());
    assert!(!format::core_version_supported(99, CORE_VERSION_LEGACY));
}

#[test]
fn unknown_format_major_and_minor_are_rejected() {
    let dir = temp_dir("fmt");
    let (_path, bytes) = write_machine(&dir, 1, b"format version probe");

    // Future major.
    let mut future_major = bytes.clone();
    put_u16(&mut future_major, OFF_FORMAT_MAJOR, FORMAT_MAJOR + 1);
    assert!(matches!(
        format::GenesisHeader::decode(&future_major),
        Err(EkbloomError::UnsupportedVersion {
            major,
            ..
        }) if major == FORMAT_MAJOR + 1
    ));
    let path = dir.join("future_major.ekb");
    std::fs::write(&path, &future_major).unwrap();
    assert!(matches!(
        open(&path),
        Err(EkbloomError::UnsupportedVersion { .. })
    ));

    // Future minor (same major line).
    let mut future_minor = bytes.clone();
    put_u16(&mut future_minor, OFF_FORMAT_MINOR, FORMAT_MINOR + 1);
    assert!(matches!(
        format::GenesisHeader::decode(&future_minor),
        Err(EkbloomError::UnsupportedVersion {
            minor,
            ..
        }) if minor == FORMAT_MINOR + 1
    ));
    let path = dir.join("future_minor.ekb");
    std::fs::write(&path, &future_minor).unwrap();
    assert!(matches!(
        open(&path),
        Err(EkbloomError::UnsupportedVersion { .. })
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unknown_core_version_is_rejected_for_every_machine() {
    let dir = temp_dir("corever");
    let src = skewed_source();
    for machine_id in 1..=8u32 {
        let (_path, bytes) = write_machine(&dir, machine_id, &src);

        // A crafted future core version.
        let mut future = bytes.clone();
        put_u16(&mut future, OFF_CORE_VERSION, 0xFFFF);
        assert!(matches!(
            format::GenesisHeader::decode(&future),
            Err(EkbloomError::UnsupportedCoreVersion {
                machine_id: m,
                core_version: 0xFFFF,
            }) if m == machine_id
        ));
        let path = dir.join(format!("bad_core_m{machine_id}.ekb"));
        std::fs::write(&path, &future).unwrap();
        assert!(matches!(
            open(&path),
            Err(EkbloomError::UnsupportedCoreVersion { machine_id: m, .. }) if m == machine_id
        ));

        // Version 2 is not a version this build knows (only 0 legacy and 1 current).
        let mut v2 = bytes.clone();
        put_u16(&mut v2, OFF_CORE_VERSION, 2);
        assert!(matches!(
            format::GenesisHeader::decode(&v2),
            Err(EkbloomError::UnsupportedCoreVersion {
                core_version: 2,
                ..
            })
        ));
        std::fs::remove_file(&path).unwrap();
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unknown_manifest_record_version_is_a_typed_error() {
    let record = ManifestRecord {
        generation: 1,
        logical_len: 8,
        object_id: [3u8; 16],
        machine_id: 1,
        core_hash: [7u8; 32],
        profiles: vec!["root".to_string()],
        components: vec![ManifestComponent {
            name: "core".to_string(),
            kind: "core".to_string(),
            bytes: 8,
        }],
    };
    let bytes = record.encode();
    assert_eq!(get_u32(&bytes, 0), MANIFEST_RECORD_VERSION);
    assert_eq!(ManifestRecord::decode(&bytes).unwrap(), record);

    let mut future = bytes.clone();
    future[0..4].copy_from_slice(&(MANIFEST_RECORD_VERSION + 1).to_le_bytes());
    assert!(matches!(
        ManifestRecord::decode(&future),
        Err(EkbloomError::UnsupportedManifestVersion { version })
            if version == MANIFEST_RECORD_VERSION + 1
    ));
}

#[test]
fn known_versions_read_byte_exactly_and_roundtrip_for_machines_1_to_8() {
    let dir = temp_dir("roundtrip");
    let src = skewed_source();
    let mut checked = 0usize;

    for machine_id in 1..=8u32 {
        let machine = machine_by_id(machine_id).unwrap();
        let expected_core = machine.encode(&src).unwrap();

        let (path, bytes) = write_machine(&dir, machine_id, &src);
        assert!(bytes.len() >= HEADER_LEN + 64);

        // The header is self-describing and carries exactly the written versions.
        let header = format::GenesisHeader::decode(&bytes).unwrap();
        assert_eq!(header.format_major, FORMAT_MAJOR);
        assert_eq!(header.format_minor, FORMAT_MINOR);
        assert_eq!(header.core_machine_id, machine_id);
        assert_eq!(header.core_version, CORE_VERSION_CURRENT);
        assert_eq!(get_u32(&bytes, OFF_CORE_MACHINE_ID), machine_id);
        assert_eq!(get_u16(&bytes, OFF_CORE_VERSION), CORE_VERSION_CURRENT);

        // The machine core is byte-identical to `machine.encode` unchanged.
        let core = &bytes[HEADER_LEN..HEADER_LEN + header.core_len as usize];
        assert_eq!(
            core,
            &expected_core[..],
            "machine {machine_id} core changed"
        );

        // Open, verify, and read the whole object and a middle range.
        let opened = open(&path).unwrap();
        assert_eq!(opened.machine_id, machine_id);
        assert_eq!(opened.logical_len, src.len() as u64);
        assert!(
            verify(&opened).unwrap().ok,
            "verify failed for machine {machine_id}"
        );
        assert_eq!(
            read_file(
                &path,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64,
                }
            )
            .unwrap(),
            src,
            "whole roundtrip mismatch for machine {machine_id}"
        );
        let mid = src.len() as u64 / 3;
        let len = 777u64;
        assert_eq!(
            read_file(
                &path,
                ReadRequest {
                    offset: mid,
                    length: len
                }
            )
            .unwrap(),
            src[mid as usize..(mid + len) as usize].to_vec(),
            "range roundtrip mismatch for machine {machine_id}"
        );

        checked += 1;
        std::fs::remove_file(&path).unwrap();
    }

    assert_eq!(checked, 8, "all eight machines must round-trip");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn legacy_zero_core_version_still_reads() {
    // A file whose formerly-reserved `core_version` bytes are zero is a legacy
    // (implicit-version) core and must read exactly as before.
    let dir = temp_dir("legacy");
    let src = skewed_source();
    let (_path, mut bytes) = write_machine(&dir, 2, &src);
    put_u16(&mut bytes, OFF_CORE_VERSION, CORE_VERSION_LEGACY);

    let header = format::GenesisHeader::decode(&bytes).unwrap();
    assert_eq!(header.core_version, CORE_VERSION_LEGACY);

    let path = dir.join("legacy.ekb");
    std::fs::write(&path, &bytes).unwrap();
    let opened = open(&path).unwrap();
    assert_eq!(opened.logical_len, src.len() as u64);
    assert!(verify(&opened).unwrap().ok);
    assert_eq!(
        read_file(
            &path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64,
            }
        )
        .unwrap(),
        src
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn crafted_future_version_file_is_rejected() {
    let dir = temp_dir("future");
    let src = b"future-version probe".to_vec();
    let (_path, bytes) = write_machine(&dir, 1, &src);

    // Claim a future format minor *and* a future core version; either must be
    // refused, and the format version is checked first.
    let mut crafted = bytes.clone();
    put_u16(&mut crafted, OFF_FORMAT_MINOR, FORMAT_MINOR + 1);
    put_u16(&mut crafted, OFF_CORE_VERSION, 42);
    let path = dir.join("crafted.ekb");
    std::fs::write(&path, &crafted).unwrap();
    assert!(matches!(
        open(&path),
        Err(EkbloomError::UnsupportedVersion { minor, .. }) if minor == FORMAT_MINOR + 1
    ));

    // With the format line valid, the future core version alone is refused.
    put_u16(&mut crafted, OFF_FORMAT_MINOR, FORMAT_MINOR);
    std::fs::write(&path, &crafted).unwrap();
    assert!(matches!(
        open(&path),
        Err(EkbloomError::UnsupportedCoreVersion {
            core_version: 42,
            ..
        })
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}
