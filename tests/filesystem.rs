//! End-to-end tests for the in-process OrFS logical namespace.
//!
//! These exercise the public API only: `open`, `create`, `read`, `list`, `stat`
//! and `inspect_physical`, checking that logical enumeration hides the concealed
//! `.ekbloom` namespace while administrative accounting reports the real physical
//! bytes and never conflates them with logical content.

use ekbloom::{FileMeta, ManifestationProfile, ObjectPhysical, OrFs, PhysicalReport, ReadRequest};
use std::path::{Path, PathBuf};

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

/// The concealed object directories (hex-named subdirectories of `.ekbloom/`),
/// sorted. Used to prove that independent objects get distinct physical homes.
fn object_dirs(root: &Path) -> Vec<String> {
    let concealed = root.join(".ekbloom");
    let mut dirs: Vec<String> = std::fs::read_dir(&concealed)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    dirs
}

/// The real `du`-style on-disk bytes of `path`: `st_blocks * 512` per inode
/// (allocated units), each inode counted once, directories included.
fn du_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).unwrap();
    let mut total = metadata.blocks().saturating_mul(512).max(metadata.len());
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            total = total.saturating_add(du_bytes(&entry.unwrap().path()));
        }
    }
    total
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

    for (name, profile) in profiles {
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
        // Each object must report its OWN profile: identical source content must
        // not let one object's manifestation overwrite another's.
        assert_eq!(
            meta.profiles,
            vec![profile.label()],
            "stat reported the wrong profile for {logical}"
        );
    }

    // Every logical path here shares identical source bytes, yet each must occupy
    // a distinct concealed directory (content does not determine physical home).
    assert_eq!(
        object_dirs(&root).len(),
        profiles.len(),
        "identically-valued objects shared a physical directory"
    );

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

/// Regression (reviewer scenario): two objects with identical source bytes at
/// distinct logical paths and distinct profiles must never share a concealed
/// directory, and each must keep its own manifestation.
#[test]
fn identical_source_distinct_paths_do_not_share_physical_directory() {
    let root = temp_root("identity");
    let fs = OrFs::open(&root).unwrap();
    let s = lcg(0xABCD_1234_5678_9012, 2048);
    let full = ReadRequest {
        offset: 0,
        length: s.len() as u64,
    };

    fs.create("data/a.bin", &s, ManifestationProfile::Root)
        .unwrap();
    fs.create("data/b.bin", &s, ManifestationProfile::Path { depth: 32 })
        .unwrap();

    // Both read back byte-exact == S.
    assert_eq!(fs.read("data/a.bin", full).unwrap(), s);
    assert_eq!(fs.read("data/b.bin", full).unwrap(), s);

    // Each reports its own profile.
    assert_eq!(
        fs.stat("data/a.bin").unwrap().profiles,
        vec!["root".to_string()]
    );
    assert_eq!(
        fs.stat("data/b.bin").unwrap().profiles,
        vec!["path(D=32)".to_string()]
    );

    // Distinct physical directories under `.ekbloom/`.
    let dirs = object_dirs(&root);
    assert_eq!(
        dirs.len(),
        2,
        "objects shared a concealed directory: {dirs:?}"
    );

    // Distinct per-object bytes: the path manifestation is strictly larger than
    // the root-only one, and reads never perturb either object.
    let a = fs.stat("data/a.bin").unwrap();
    let b = fs.stat("data/b.bin").unwrap();
    assert!(
        b.physical_bytes > a.physical_bytes,
        "path manifestation {} not larger than root {}",
        b.physical_bytes,
        a.physical_bytes
    );
    assert_eq!(fs.read("data/b.bin", full).unwrap(), s);
    assert_eq!(fs.read("data/a.bin", full).unwrap(), s);
    assert_eq!(fs.stat("data/a.bin").unwrap(), a);
    assert_eq!(fs.stat("data/b.bin").unwrap(), b);

    // Physical accounting covers both independent objects.
    let report = fs.inspect_physical().unwrap();
    assert_eq!(report.objects.len(), 2);
    assert_eq!(report.logical_bytes, 2 * s.len() as u64);
    let summed = a.physical_bytes.saturating_add(b.physical_bytes);
    assert!(
        report.total_bytes >= summed,
        "total {} < sum of both objects {}",
        report.total_bytes,
        summed
    );
    assert_eq!(
        dirs,
        object_dirs(&root),
        "directory set changed after reads"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

/// Content-identity stress: many logical paths from the SAME source with many
/// DIFFERENT profiles must stay fully isolated (no cross-contamination).
#[test]
fn same_source_many_profiles_stay_isolated() {
    let root = temp_root("stress");
    let fs = OrFs::open(&root).unwrap();
    let s = lcg(0x5151_2626_3737_4848, 1024);
    let full = ReadRequest {
        offset: 0,
        length: s.len() as u64,
    };
    let cases = [
        ("data/root.bin", ManifestationProfile::Root),
        ("data/p4.bin", ManifestationProfile::Path { depth: 4 }),
        ("data/p16.bin", ManifestationProfile::Path { depth: 16 }),
        ("data/p32.bin", ManifestationProfile::Path { depth: 32 }),
        ("data/full.bin", ManifestationProfile::Full),
        (
            "data/sparse.bin",
            ManifestationProfile::Sparse { interval: 64 },
        ),
    ];
    for (path, profile) in cases {
        fs.create(path, &s, profile).unwrap();
    }

    for (path, profile) in cases {
        assert_eq!(fs.read(path, full).unwrap(), s, "read mismatch for {path}");
        assert_eq!(
            fs.stat(path).unwrap().profiles,
            vec![profile.label()],
            "profile cross-contamination for {path}"
        );
    }

    assert_eq!(
        object_dirs(&root).len(),
        cases.len(),
        "identically-valued objects shared physical directories"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

/// The authoritative payload is stored **exactly once**: the visible core of each
/// object lives only in the single authoritative `.ekb` store, and every other
/// concealed name is a tiny pointer view (not a duplicated payload).
#[test]
fn authoritative_payload_is_stored_exactly_once() {
    let root = temp_root("dedup");
    let fs = OrFs::open(&root).unwrap();
    let src = lcg(0x0BAD_F00D_1234_5678, 4096);
    fs.create("data/obj.bin", &src, ManifestationProfile::MaxSpeed)
        .unwrap();

    let dirs = object_dirs(&root);
    assert_eq!(dirs.len(), 1);
    let obj = root.join(".ekbloom").join(&dirs[0]);
    let authoritative = obj.join("manifests");
    assert!(
        authoritative.is_file(),
        "one authoritative `.ekb` per object"
    );

    // The core bytes occur only in the authoritative file, never in a sidecar.
    let bytes = std::fs::read(&authoritative).unwrap();
    let header = ekbloom::format::GenesisHeader::decode(&bytes).unwrap();
    let start = ekbloom::format::CORE_OFFSET as usize;
    let core = &bytes[start..start + header.core_len as usize];
    assert!(core.len() > 256);

    let mut holders = 0usize;
    for entry in std::fs::read_dir(&obj).unwrap() {
        let path = entry.unwrap().path();
        let contents = std::fs::read(&path).unwrap();
        if contents.len() >= core.len() && contents.windows(core.len()).any(|w| w == core) {
            holders += 1;
            assert_eq!(path.file_name().unwrap(), "manifests");
        }
    }
    assert_eq!(holders, 1, "the core payload must be stored exactly once");

    // Every sidecar is a tiny pointer view, so the object directory is only
    // slightly larger than the single authoritative store.
    let sidecars: u64 = std::fs::read_dir(&obj)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            if e.file_name() == "manifests" {
                0
            } else {
                e.metadata().unwrap().len()
            }
        })
        .sum();
    assert!(sidecars < 1024, "sidecars carry no payload: {sidecars} B");

    std::fs::remove_dir_all(&root).unwrap();
}

/// `inspect_physical` reports the real on-disk allocation of the concealed
/// namespace: an independent `du`-style walk (allocated blocks per inode,
/// directories counted once) of `.ekbloom/` plus the logical pointer artifacts
/// agrees exactly, and each object's `stat().physical_bytes` matches a walk of
/// its own concealed directory.
#[test]
fn inspect_physical_matches_du_style_bytes() {
    let root = temp_root("du");
    let fs = OrFs::open(&root).unwrap();
    fs.create(
        "data/root.bin",
        &lcg(0x1111_2222, 1024),
        ManifestationProfile::Root,
    )
    .unwrap();
    fs.create(
        "data/path.bin",
        &lcg(0x3333_4444, 2500),
        ManifestationProfile::Path { depth: 4 },
    )
    .unwrap();

    let report = fs.inspect_physical().unwrap();
    let mut expected = du_bytes(&root.join(".ekbloom"));
    for name in ["data/root.bin", "data/path.bin"] {
        expected = expected.saturating_add(du_bytes(&root.join(name)));
    }
    assert_eq!(
        report.total_bytes, expected,
        "inspect_physical total must equal the real du-style byte count"
    );
    assert!(report.concealed_bytes > 0);
    assert_eq!(report.logical_bytes, 1024 + 2500);

    std::fs::remove_dir_all(&root).unwrap();
}
