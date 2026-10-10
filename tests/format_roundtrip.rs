//! End-to-end `.ekb` wire-format tests: write -> reopen -> verify -> read, plus
//! additive generations and malformed-input rejection.
//!
//! Every test uses a unique subdirectory of `std::env::temp_dir()` and removes it
//! afterwards.

use std::path::PathBuf;

use ekbloom::format::{self, HEADER_LEN};
use ekbloom::{
    append_generation, create, grow, inspect_file, open, read_file, verify, write_body,
    EkbloomError, ManifestationProfile, ReadRequest,
};

const MAGIC: [u8; 8] = *b"EKBLOOM\0";
const MARKER: [u8; 8] = *b"EKBCOMM\0";

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
        "ekbloom-format-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cases() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        (0u8..=255).collect(),
        lcg(0x1234_5678, 63),
        lcg(0x9ABC_DEF0, 64),
        lcg(0x0FED_CBA9, 65),
        lcg(0x1111_2222, 777),
        lcg(0x3333_4444, 1024),
        lcg(0x5555_6666, 1025),
    ]
}

fn profiles() -> Vec<ManifestationProfile> {
    vec![
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 1024 },
        ManifestationProfile::Dense { interval: 64 },
        ManifestationProfile::Full,
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::MaxSpeed,
    ]
}

fn ranges(n: u64) -> Vec<(u64, u64)> {
    let mut out = vec![(0, n), (0, 0), (n, 0)];
    if n > 0 {
        out.push((n - 1, 1));
        out.push((n / 2, n - n / 2));
        out.push((0, n.min(7)));
    }
    out
}

#[test]
fn write_open_read_roundtrip_for_every_profile() {
    let dir = temp_dir("roundtrip");
    for (ci, src) in cases().into_iter().enumerate() {
        for profile in profiles() {
            let path = dir.join(format!(
                "{ci}-{}.ekb",
                profile.label().replace(['/', '='], "_")
            ));
            let body = create(&src, profile).unwrap();
            write_body(&path, &body).unwrap();

            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(&bytes[..8], &MAGIC, "magic missing");
            assert_eq!(
                &bytes[bytes.len() - 16..bytes.len() - 8],
                &MARKER,
                "footer marker missing"
            );
            assert!(bytes.len() >= HEADER_LEN + 64);

            let opened = open(&path).unwrap();
            assert_eq!(opened.logical_len, src.len() as u64);
            assert!(verify(&opened).unwrap().ok, "verify failed: {profile:?}");
            assert_eq!(
                read_file(
                    &path,
                    ReadRequest {
                        offset: 0,
                        length: src.len() as u64
                    }
                )
                .unwrap(),
                src,
                "whole roundtrip mismatch: len={} {profile:?}",
                src.len()
            );

            for (off, len) in ranges(src.len() as u64) {
                let got = read_file(
                    &path,
                    ReadRequest {
                        offset: off,
                        length: len,
                    },
                )
                .unwrap();
                assert_eq!(
                    got,
                    src[off as usize..(off + len) as usize].to_vec(),
                    "range ({off},{len}) mismatch: len={} {profile:?}",
                    src.len()
                );
            }

            let report = inspect_file(&path).unwrap();
            assert_eq!(report.logical_len, src.len() as u64);
            assert_eq!(report.visible_bytes, src.len() as u64);

            std::fs::remove_file(&path).unwrap();
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn append_generation_uses_newest_and_preserves_prefix() {
    let dir = temp_dir("append");
    let path = dir.join("grow.ekb");
    let src = lcg(0xABCD_EF01, 500);

    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();
    let prefix = std::fs::read(&path).unwrap();

    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    let gen = append_generation(&path, &body).unwrap();
    assert_eq!(gen, 2);

    let after = std::fs::read(&path).unwrap();
    assert!(
        after.len() > prefix.len(),
        "appending should extend the file"
    );
    assert_eq!(&after[..prefix.len()], &prefix[..], "prefix rewritten");

    let opened = open(&path).unwrap();
    assert_eq!(opened.generation, 2);
    assert_eq!(opened.checkpoints.len(), 1);
    assert!(verify(&opened).unwrap().ok);
    assert_eq!(
        read_file(
            &path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );

    let report = ekbloom::scan_generations(&path).unwrap();
    assert_eq!(report.newest_valid, Some(2));
    assert_eq!(report.generations.len(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn exact_layout_is_fixed_size_header_and_footer() {
    let dir = temp_dir("layout");
    let path = dir.join("layout.ekb");
    let src = b"layout".to_vec();
    let body = create(&src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    assert_eq!(&bytes[..8], &MAGIC);
    assert_eq!(HEADER_LEN, 80);
    // header_len field is exactly HEADER_LEN.
    assert_eq!(
        u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize,
        HEADER_LEN
    );
    // logical_bytes (offset 48) is present and equals logical_symbols (offset 40).
    let logical_symbols = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
    let logical_bytes = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
    assert_eq!(logical_symbols, src.len() as u64);
    assert_eq!(logical_bytes, logical_symbols);
    // The footer occupies the final 64 bytes and carries the marker.
    assert_eq!(
        &bytes[bytes.len() - 64 + 48..bytes.len() - 64 + 56],
        &MARKER
    );
    // Sections begin 8-aligned right after the core.
    let sections_start = format::align8((HEADER_LEN + src.len()) as u64);
    assert_eq!(sections_start % 8, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Locate the first section header offset of a freshly written root file.
fn first_section_offset(core_len: u64) -> usize {
    format::align8((HEADER_LEN as u64) + core_len) as usize
}

#[test]
fn malformed_files_are_rejected_specifically_and_never_panic() {
    let dir = temp_dir("malformed");
    let src = b"abcdef".to_vec();
    let body = create(&src, ManifestationProfile::Root).unwrap();

    // Produce a fresh valid image for each mutation.
    let write_valid = |name: &str| -> (PathBuf, Vec<u8>) {
        let path = dir.join(name);
        write_body(&path, &body).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    };

    // Bad magic.
    {
        let (path, mut bytes) = write_valid("badmagic.ekb");
        bytes[0] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(open(&path), Err(EkbloomError::BadMagic)));
    }

    // Unsupported version.
    {
        let (path, mut bytes) = write_valid("version.ekb");
        bytes[8..10].copy_from_slice(&9u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            open(&path),
            Err(EkbloomError::UnsupportedVersion { major: 9, .. })
        ));
    }

    // header_len != 64.
    {
        let (path, mut bytes) = write_valid("headerlen.ekb");
        bytes[12..16].copy_from_slice(&60u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(open(&path), Err(EkbloomError::BadSection { .. })));
    }

    let section = first_section_offset(src.len() as u64);

    // Out-of-range section offset.
    {
        let (path, mut bytes) = write_valid("offset.ekb");
        bytes[section + 8..section + 16].copy_from_slice(&(u64::MAX - 8).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            open(&path),
            Err(EkbloomError::BadSection { .. }) | Err(EkbloomError::Truncated { .. })
        ));
    }

    // Out-of-range section length (beyond end of file).
    {
        let (path, mut bytes) = write_valid("length.ekb");
        let huge = (bytes.len() as u64) + 4096;
        bytes[section + 16..section + 24].copy_from_slice(&huge.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            open(&path),
            Err(EkbloomError::Truncated { .. }) | Err(EkbloomError::Overflow { .. })
        ));
    }

    // Unknown section kind.
    {
        let (path, mut bytes) = write_valid("kind.ekb");
        bytes[section..section + 4].copy_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(open(&path), Err(EkbloomError::BadSection { .. })));
    }

    // Section payload checksum mismatch.
    {
        let (path, mut bytes) = write_valid("checksum.ekb");
        let payload = section + 40;
        bytes[payload] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            open(&path),
            Err(EkbloomError::ChecksumMismatch { .. })
        ));
    }

    // Truncated file (header only).
    {
        let (path, bytes) = write_valid("truncated.ekb");
        std::fs::write(&path, &bytes[..48]).unwrap();
        assert!(matches!(open(&path), Err(EkbloomError::Truncated { .. })));
    }

    std::fs::remove_dir_all(&dir).unwrap();
}
