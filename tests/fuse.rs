//! Best-effort end-to-end FUSE smoke test for the read-only `OrFs` projection.
//!
//! This test needs `/dev/fuse` plus the privileges to mount (see
//! `scripts/container-fuse.sh`). When `/dev/fuse` is unavailable — as in the
//! ordinary verification container — it reports that it is skipping and returns,
//! so it never fakes a pass. Inside the FUSE container it performs a real mount,
//! exercises the namespace, and unmounts.

#![cfg(all(feature = "fuse", target_os = "linux"))]

use ekbloom::{mount, mount_with, ManifestationProfile, MountOptions, OrFs, ReadMode, ReadRequest};
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

/// Deterministic bytes (shared LCG with the other harnesses).
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

/// A fully-materialised object (a certified whole-object path bloom) is served
/// from its logically-aligned backing surface in the default [`ReadMode::Auto`],
/// and from the decoder in [`ReadMode::Decode`]. Both must return byte-exact
/// bytes — full reads and arbitrary windows — matching the source.
#[test]
fn backing_surface_and_decode_paths_are_byte_exact() {
    if !fuse_available() {
        eprintln!(
            "SKIP: /dev/fuse is not available; run scripts/container-fuse.sh for a real mount"
        );
        return;
    }

    let root = temp_dir("readmode-root");
    let auto_mnt = temp_dir("readmode-auto");
    let decode_mnt = temp_dir("readmode-decode");
    let src = lcg(0xEB10_F05E, 64 * 1024);
    {
        let fs = OrFs::open(&root).unwrap();
        fs.create(
            "data/hot.bin",
            &src,
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();
    }
    let logical = "data/hot.bin";

    // The default mode attaches the `.ekb` path-bloom hot array as a surface.
    let fs = OrFs::open(&root).unwrap();
    let auto = mount_with(fs, &auto_mnt, false, &MountOptions::default()).unwrap();
    // The decode mode forces the engine/decoder path.
    let fs = OrFs::open(&root).unwrap();
    let decode = mount_with(
        fs,
        &decode_mnt,
        false,
        &MountOptions {
            read_mode: ReadMode::Decode,
            ..MountOptions::default()
        },
    )
    .unwrap();

    let via_auto = auto_mnt.join(logical);
    let via_decode = decode_mnt.join(logical);

    // Full reads through both mounts equal the source exactly.
    assert_eq!(std::fs::read(&via_auto).unwrap(), src, "auto full read");
    assert_eq!(std::fs::read(&via_decode).unwrap(), src, "decode full read");

    // Arbitrary windows, including misaligned and tail-crossing ones, agree with
    // the source through both mounts.
    use std::os::unix::fs::FileExt;
    let f_auto = std::fs::File::open(&via_auto).unwrap();
    let f_decode = std::fs::File::open(&via_decode).unwrap();
    for &(off, len) in &[
        (0u64, 1u64),
        (1, 4095),
        (4095, 2),
        (1234, 777),
        (32 * 1024 - 8, 16),
        (src.len() as u64 - 3, 8),
        (0, src.len() as u64),
    ] {
        // The kernel clamps a read past EOF to the file length; mirror that.
        let read_len = ((src.len() as u64) - off).min(len);
        let want = &src[off as usize..(off + read_len) as usize];
        let mut a = vec![0u8; read_len as usize];
        let mut d = vec![0u8; read_len as usize];
        f_auto.read_exact_at(&mut a, off).unwrap();
        f_decode.read_exact_at(&mut d, off).unwrap();
        assert_eq!(a, want, "auto window ({off},{len})");
        assert_eq!(d, want, "decode window ({off},{len})");
    }

    // Close the FUSE file descriptors before unmounting; an open fd on the mount
    // would make the unmount busy.
    drop(f_auto);
    drop(f_decode);
    drop(auto);
    drop(decode);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&auto_mnt).ok();
    std::fs::remove_dir_all(&decode_mnt).ok();
}

/// `ReadMode::Auto` attaches a monolithic backing surface only for a fully
/// materialised, logically-aligned object. An object without one exposes **no**
/// surface (`aligned_surface` is `None`), so the read must fall back to the
/// decoder. Both the present- and absent-surface cases must decode byte-exactly,
/// including windows that cross a 4096-byte region boundary.
///
/// Unlike the mount tests above, this needs no `/dev/fuse`: it drives the public
/// `OrFs` discovery/read API directly, so it runs in the ordinary gate.
#[test]
fn auto_uses_a_surface_only_when_present_and_the_decoder_is_byte_exact() {
    let root = temp_dir("surface-fallback");
    let src = lcg(0x5A1F_ACE0, 8 * 1024 + 5);
    {
        let fs = OrFs::open(&root).unwrap();
        // `Root` manifests no whole-object output surface: reads must decode.
        fs.create("data/plain.bin", &src, ManifestationProfile::Root)
            .unwrap();
        // `Path` materialises the whole output as one aligned surface.
        fs.create(
            "data/hot.bin",
            &src,
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();
    }
    let fs = OrFs::open(&root).unwrap();

    assert!(
        fs.aligned_surface("data/plain.bin").unwrap().is_none(),
        "an un-materialised object must expose no backing surface (decode fallback)"
    );
    let surface = fs
        .aligned_surface("data/hot.bin")
        .unwrap()
        .expect("a path-bloom object exposes an aligned surface");
    assert_eq!(surface.logical_len, src.len() as u64);

    // Decoder path over both objects, byte-exact including boundary-crossing reads.
    for &(off, len) in &[
        (0u64, 1u64),
        (1, 4095),
        (4095, 2),
        (4096, 4096),
        (src.len() as u64 - 3, 3),
        (0, src.len() as u64),
    ] {
        let want = &src[off as usize..(off + len) as usize];
        assert_eq!(
            fs.read(
                "data/plain.bin",
                ReadRequest {
                    offset: off,
                    length: len
                }
            )
            .unwrap(),
            want,
            "plain decode ({off},{len})"
        );
        assert_eq!(
            fs.read(
                "data/hot.bin",
                ReadRequest {
                    offset: off,
                    length: len
                }
            )
            .unwrap(),
            want,
            "hot decode ({off},{len})"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}
