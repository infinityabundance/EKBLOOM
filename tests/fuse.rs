//! Best-effort end-to-end FUSE smoke test for the read-only `OrFs` projection.
//!
//! This test needs `/dev/fuse` plus the privileges to mount (see
//! `scripts/container-fuse.sh`). When `/dev/fuse` is unavailable — as in the
//! ordinary verification container — it reports that it is skipping and returns,
//! so it never fakes a pass. Inside the FUSE container it performs a real mount,
//! exercises the namespace, and unmounts.

#![cfg(all(feature = "fuse", target_os = "linux"))]

use ekbloom::{mount, ManifestationProfile, OrFs};
use std::path::PathBuf;

/// A unique temporary directory, removed by the caller.
fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-fuse-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Whether FUSE mounting is even possible in this environment.
fn fuse_available() -> bool {
    std::fs::metadata("/dev/fuse").is_ok()
}

/// Sorted names in a directory.
fn names(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

#[test]
fn read_only_mount_exposes_logical_files_and_control_file() {
    if !fuse_available() {
        eprintln!(
            "SKIP: /dev/fuse is not available; run scripts/container-fuse.sh for a real mount"
        );
        return;
    }

    let root = temp_dir("root");
    let mountpoint = temp_dir("mnt");

    let hello = b"hello from the logical namespace".to_vec();
    let other = b"a second logical object".to_vec();
    {
        let fs = OrFs::open(&root).unwrap();
        fs.create("data/hello.bin", &hello, ManifestationProfile::Full)
            .unwrap();
        fs.create(
            "data/other.bin",
            &other,
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();
    }
    let fs = OrFs::open(&root).unwrap();
    let handle = mount(fs, &mountpoint, false).expect("mount read-only OrFs");

    // 1. The root listing shows the logical directory and the control file, and
    //    never the concealed namespace.
    let root_names = names(&mountpoint);
    assert!(
        root_names.contains(&"data".to_string()),
        "root listing missing data: {root_names:?}"
    );
    assert!(
        root_names.contains(&".ekbloom-physical.json".to_string()),
        "root listing missing control file: {root_names:?}"
    );
    assert!(
        !root_names.iter().any(|n| n == ".ekbloom"),
        "concealed namespace leaked into readdir: {root_names:?}"
    );

    // 2. Logical files read back byte-exact through the mount.
    assert_eq!(
        std::fs::read(mountpoint.join("data/hello.bin")).unwrap(),
        hello
    );
    assert_eq!(
        std::fs::read(mountpoint.join("data/other.bin")).unwrap(),
        other
    );
    let data_names = names(&mountpoint.join("data"));
    assert_eq!(
        data_names,
        vec!["hello.bin".to_string(), "other.bin".to_string()]
    );

    // 3. The control file is a physical-accounting view, not logical content.
    let control = std::fs::read_to_string(mountpoint.join(".ekbloom-physical.json")).unwrap();
    assert!(control.contains("\"kind\": \"physical-accounting\""));
    assert!(control.contains("\"physical_bytes\""));
    assert!(control.contains("\"concealed_bytes\""));
    assert!(control.contains("data/hello.bin"));

    // 4. The mount is read-only: writes must be refused.
    let write_result = std::fs::write(mountpoint.join("data/new.bin"), b"nope");
    assert!(
        write_result.is_err(),
        "write unexpectedly succeeded on a read-only mount"
    );

    // 5. Unmount via drop of the handle.
    drop(handle);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&mountpoint).ok();
}
