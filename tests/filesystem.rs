//! End-to-end tests for the in-process OrFS logical namespace.
//!
//! These exercise the public API only: `open`, `create`, `read`, `list`, `stat`
//! and `inspect_physical`, checking that logical enumeration hides the concealed
//! `.ekbloom` namespace while administrative accounting reports the real physical
//! bytes and never conflates them with logical content.

use ekbloom::{FileMeta, ManifestationProfile, ObjectPhysical, OrFs, PhysicalReport, ReadRequest};
use std::path::PathBuf;

/// Deterministic pseudorandom bytes from a small LCG (no new dependencies).
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

/// A batch of valid random ranges over `n` bytes (including zero-length reads).
fn random_ranges(n: u64) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..32 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let offset = if n == 0 { 0 } else { s % n };
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let remaining = n - offset;
        let length = if remaining == 0 {
            0
        } else {
            s % (remaining + 1)
        };
        ranges.push((offset, length));
    }
    ranges
}

/// A unique temporary root, removed by the caller.
fn temp_root(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-fs-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn create_read_and_stat_across_profiles() {
    let root = temp_root("rt");
    let fs = OrFs::open(&root).unwrap();
    let src = lcg(0x1234_5678, 2000);
    let profiles = [
        ("root.bin", ManifestationProfile::Root),
        ("sparse.bin", ManifestationProfile::Sparse { interval: 256 }),
        ("full.bin", ManifestationProfile::Full),
        ("path.bin", ManifestationProfile::Path { depth: 4 }),
        (
            "regional.bin",
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 512,
            },
        ),
        ("max.bin", ManifestationProfile::MaxSpeed),
    ];
    for (name, profile) in profiles {
        let logical = format!("data/{name}");
        fs.create(&logical, &src, profile).unwrap();
    }

    for (name, _) in profiles {
        let logical = format!("data/{name}");
        let full = fs
            .read(
                &logical,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64,
                },
            )
            .unwrap();
        assert_eq!(full, src, "full read mismatch for {logical}");

        for (offset, length) in random_ranges(src.len() as u64) {
            let got = fs.read(&logical, ReadRequest { offset, length }).unwrap();
            let want = src[offset as usize..(offset + length) as usize].to_vec();
            assert_eq!(
                got, want,
                "range ({offset},{length}) mismatch for {logical}"
            );
        }

        let meta: FileMeta = fs.stat(&logical).unwrap();
        assert_eq!(meta.logical_len, src.len() as u64);
        assert_eq!(meta.visible_bytes, src.len() as u64);
        assert!(meta.physical_bytes >= meta.visible_bytes);
        assert_eq!(meta.object_id, fs.stat(&logical).unwrap().object_id);
    }

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn list_hides_the_concealed_namespace() {
    let root = temp_root("list");
    let fs = OrFs::open(&root).unwrap();
    fs.create(
        "data/example.bin",
        b"hello concealed world",
        ManifestationProfile::Full,
    )
    .unwrap();

    let entries = fs.list("data").unwrap();
    assert_eq!(entries, vec!["example.bin".to_string()]);
    assert!(entries.iter().all(|name| !name.starts_with(".ekbloom")));

    // The concealed directory is physically present even though it is hidden.
    assert!(root.join(".ekbloom").is_dir());

    // The root listing also hides the concealed namespace.
    let root_entries = fs.list("").unwrap();
    assert!(root_entries
        .iter()
        .all(|name| !name.starts_with(".ekbloom")));
    assert!(root_entries.contains(&"data".to_string()));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn physical_accounting_reports_real_bytes() {
    let root = temp_root("phys");
    let fs = OrFs::open(&root).unwrap();
    let n = 1500usize;
    let plain = lcg(0x1111_1111, n);
    let full = lcg(0x2222_2222, n);
    let path = lcg(0x3333_3333, n);

    fs.create("data/plain.bin", &plain, ManifestationProfile::Root)
        .unwrap();
    fs.create("data/full.bin", &full, ManifestationProfile::Full)
        .unwrap();
    fs.create(
        "data/path.bin",
        &path,
        ManifestationProfile::Path { depth: 4 },
    )
    .unwrap();

    let report: PhysicalReport = fs.inspect_physical().unwrap();

    // Physical accounting is at least the sum of each object's real bytes.
    let mut summed = 0u64;
    for name in ["data/plain.bin", "data/full.bin", "data/path.bin"] {
        summed = summed.saturating_add(fs.stat(name).unwrap().physical_bytes);
    }
    assert!(
        report.total_bytes >= summed,
        "total {} < per-object sum {}",
        report.total_bytes,
        summed
    );

    // Path/Full manifest real concealed bytes beyond the visible representation.
    assert!(report.concealed_bytes > 0);

    // The report distinguishes logical from physical: the visible bytes equal the
    // logical content, and the concealed excess is exactly the physical bytes
    // beyond that visible content.
    assert_eq!(report.logical_bytes, 3 * n as u64);
    assert_eq!(
        report.concealed_bytes,
        report.total_bytes - report.logical_bytes
    );
    assert!(report.total_bytes > report.logical_bytes);

    // Every object is accounted for, and physical never under-reports visible.
    assert_eq!(report.objects.len(), 3);
    let path_obj: &ObjectPhysical = report
        .objects
        .iter()
        .find(|o| o.logical_path == "data/path.bin")
        .unwrap();
    assert_eq!(path_obj.visible_bytes, n as u64);
    assert!(path_obj.physical_bytes > path_obj.visible_bytes);

    // This quota view is logical, not host physical allocation.
    assert!(fs.is_logical_quota());

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn reopen_rediscovers_objects() {
    let root = temp_root("reopen");
    let src = lcg(0x0F0F_0F0F, 900);
    {
        let fs = OrFs::open(&root).unwrap();
        fs.create("data/a.bin", &src, ManifestationProfile::Full)
            .unwrap();
        fs.create(
            "data/b.bin",
            &lcg(0x1E1E_1E1E, 300),
            ManifestationProfile::Sparse { interval: 256 },
        )
        .unwrap();
    }

    let fs = OrFs::open(&root).unwrap();
    let mut names = fs.list("data").unwrap();
    names.sort();
    assert_eq!(names, vec!["a.bin".to_string(), "b.bin".to_string()]);
    assert_eq!(
        fs.read(
            "data/a.bin",
            ReadRequest {
                offset: 0,
                length: src.len() as u64,
            },
        )
        .unwrap(),
        src
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn duplicate_logical_path_is_rejected() {
    let root = temp_root("dup");
    let fs = OrFs::open(&root).unwrap();
    fs.create("data/example.bin", b"first", ManifestationProfile::Root)
        .unwrap();

    assert!(fs
        .create("data/example.bin", b"second", ManifestationProfile::Full)
        .is_err());

    // The original object is untouched.
    assert_eq!(
        fs.read(
            "data/example.bin",
            ReadRequest {
                offset: 0,
                length: 5,
            },
        )
        .unwrap(),
        b"first".to_vec()
    );

    std::fs::remove_dir_all(&root).unwrap();
}
