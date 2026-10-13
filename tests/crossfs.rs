//! Cross-filesystem determinism.
//!
//! The same `.ekb` and the same stream container are written on every available
//! filesystem, then reopened from each: `physical_bytes()`, sampled `byte_at`
//! values and the file BLAKE3 must be identical, byte for byte. The in-test
//! default set is the temporary directory (overlay) plus `/dev/shm` (tmpfs) when
//! present; `scripts/run-crossfs.sh` widens it (adding the ext4 bind mount and any
//! loopback xfs/btrfs) by exporting `EKB_CROSSFS_TARGETS`. When
//! `EKB_CROSSFS_EVIDENCE` is set the discovered set and the result are written to
//! `evidence/bench/crossfs.json`.

use std::path::{Path, PathBuf};

use ekbloom::{
    create, grow, open, read_file, verify, write_body, ManifestationProfile, ReadRequest,
};

/// One filesystem target: a `label` (typically the `fstype`) and a directory.
struct Target {
    label: String,
    dir: PathBuf,
}

/// The result of building and re-reading the two artifacts on one filesystem.
struct Artifacts {
    label: String,
    dir: PathBuf,
    ekb_physical_bytes: u64,
    ekb_byte_at: Vec<u8>,
    ekb_blake3: [u8; 32],
    ekb_len: u64,
    stream_container_bytes: u64,
    stream_read_at: Vec<u8>,
    stream_blake3: [u8; 32],
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

fn mixed(seed: u64, len: usize) -> Vec<u8> {
    let half = len / 2;
    let mut out = lcg(seed, half);
    out.extend(std::iter::repeat_n(0x5Au8, len - half));
    out
}

fn writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".ekb-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The filesystem targets: `EKB_CROSSFS_TARGETS` (`label<TAB>path` per line) when
/// set, else the temporary directory plus `/dev/shm` when it is writable.
fn targets() -> Vec<Target> {
    if let Ok(spec) = std::env::var("EKB_CROSSFS_TARGETS") {
        return spec
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(label, path)| Target {
                label: label.to_string(),
                dir: PathBuf::from(path),
            })
            .collect();
    }
    let mut out = vec![Target {
        label: "temp".to_string(),
        dir: std::env::temp_dir(),
    }];
    let shm = Path::new("/dev/shm");
    if writable(shm) {
        out.push(Target {
            label: "tmpfs".to_string(),
            dir: shm.to_path_buf(),
        });
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Fixed byte-at sample offsets for a logical length.
fn samples(len: usize) -> Vec<u64> {
    if len == 0 {
        return vec![];
    }
    let mut v = vec![0u64, (len / 3) as u64, (len / 2) as u64, (len - 1) as u64];
    v.sort_unstable();
    v.dedup();
    v
}

/// Build both artifacts under `target.dir` and re-read them, collecting the
/// quantities that must be filesystem-independent.
fn build_on(target: &Target, src: &[u8]) -> Artifacts {
    let dir = target
        .dir
        .join(format!("ekbloom-crossfs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // ---- `.ekb`: a root body grown with sparse checkpoints ----
    let ekb_path = dir.join("object.bin");
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    write_body(&ekb_path, &body).unwrap();

    let ekb_bytes = std::fs::read(&ekb_path).unwrap();
    let opened = open(&ekb_path).unwrap();
    assert!(verify(&opened).unwrap().ok);
    assert_eq!(
        read_file(
            &ekb_path,
            ReadRequest {
                offset: 0,
                length: src.len() as u64,
            }
        )
        .unwrap(),
        src
    );
    let ekb_byte_at: Vec<u8> = samples(src.len())
        .into_iter()
        .map(|off| ekbloom::byte_at(&opened, off).unwrap())
        .collect();

    // ---- stream container: odd, pseudorandom chunk sizes ----
    let stream_path = dir.join("container.bin");
    {
        let file = std::fs::File::create(&stream_path).unwrap();
        let mut builder = ekbloom::StreamBuilder::new(
            file,
            ekbloom::StreamOptions {
                block_bytes: 4096,
                machine: ekbloom::RepresentationPolicy::Auto,
            },
        )
        .unwrap();
        let mut off = 0usize;
        let mut rng = 0x5EED_C0DE_0000_0001u64;
        while off < src.len() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let take = (1 + (rng >> 33) as usize % 131).min(src.len() - off);
            builder.write(&src[off..off + take]).unwrap();
            off += take;
        }
        builder.finish().unwrap();
    }
    let stream_bytes = std::fs::read(&stream_path).unwrap();
    let reader = ekbloom::StreamReader::open(std::fs::File::open(&stream_path).unwrap()).unwrap();
    assert_eq!(reader.len(), src.len() as u64);
    let stream_read_at: Vec<u8> = samples(src.len())
        .into_iter()
        .map(|off| {
            let mut one = [0u8; 1];
            reader.read_at(off, &mut one).unwrap();
            one[0]
        })
        .collect();

    let artifacts = Artifacts {
        label: target.label.clone(),
        dir,
        ekb_physical_bytes: opened.physical_bytes(),
        ekb_byte_at,
        ekb_blake3: *blake3::hash(&ekb_bytes).as_bytes(),
        ekb_len: ekb_bytes.len() as u64,
        stream_container_bytes: stream_bytes.len() as u64,
        stream_read_at,
        stream_blake3: *blake3::hash(&stream_bytes).as_bytes(),
    };
    let _ = std::fs::remove_dir_all(&artifacts.dir);
    artifacts
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn same_bytes_and_reads_across_filesystems() {
    let targets = targets();
    assert!(
        !targets.is_empty(),
        "no writable filesystem target was found"
    );

    let src = mixed(0x00C0_FFEE_2026, 64 * 1024);
    let results: Vec<Artifacts> = targets.iter().map(|t| build_on(t, &src)).collect();

    let first = &results[0];
    for r in &results[1..] {
        assert_eq!(
            r.ekb_physical_bytes, first.ekb_physical_bytes,
            "physical_bytes differs on {}",
            r.label
        );
        assert_eq!(
            r.ekb_byte_at, first.ekb_byte_at,
            "byte_at differs on {}",
            r.label
        );
        assert_eq!(
            r.ekb_blake3, first.ekb_blake3,
            ".ekb BLAKE3 differs on {}",
            r.label
        );
        assert_eq!(
            r.ekb_len, first.ekb_len,
            ".ekb length differs on {}",
            r.label
        );
        assert_eq!(
            r.stream_container_bytes, first.stream_container_bytes,
            "container size differs on {}",
            r.label
        );
        assert_eq!(
            r.stream_read_at, first.stream_read_at,
            "stream read_at differs on {}",
            r.label
        );
        assert_eq!(
            r.stream_blake3, first.stream_blake3,
            "container BLAKE3 differs on {}",
            r.label
        );
    }

    if let Ok(out) = std::env::var("EKB_CROSSFS_EVIDENCE") {
        let path = Path::new(&out);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let note = std::env::var("EKB_CROSSFS_LOOPBACK_NOTE")
            .unwrap_or_else(|_| "loopback xfs/btrfs not attempted".to_string());
        let mut filesystems = String::new();
        for (i, r) in results.iter().enumerate() {
            if i > 0 {
                filesystems.push_str(",\n");
            }
            filesystems.push_str(&format!(
                concat!(
                    "    {{ \"label\": \"{}\", \"ekb_physical_bytes\": {}, ",
                    "\"ekb_len\": {}, \"ekb_blake3\": \"{}\", ",
                    "\"stream_container_bytes\": {}, \"stream_blake3\": \"{}\" }}"
                ),
                json_escape(&r.label),
                r.ekb_physical_bytes,
                r.ekb_len,
                hex(&r.ekb_blake3),
                r.stream_container_bytes,
                hex(&r.stream_blake3),
            ));
        }
        let json = format!(
            concat!(
                "{{\n",
                "  \"generated_by\": \"tests/crossfs.rs (sh scripts/run-crossfs.sh)\",\n",
                "  \"filesystem_count\": {count},\n",
                "  \"file_blake3_identical\": true,\n",
                "  \"physical_bytes_identical\": true,\n",
                "  \"byte_at_samples_identical\": true,\n",
                "  \"stream_container_identical\": true,\n",
                "  \"loopback_note\": \"{note}\",\n",
                "  \"arch_note\": \"x86_64 vs aarch64 determinism is covered separately by scripts/container-aarch64.sh (Q2): identical\",\n",
                "  \"filesystems\": [\n{fs}\n  ],\n",
                "  \"verdict\": \"the same .ekb and stream container are byte-identical (BLAKE3) across every filesystem, and physical_bytes()/byte_at agree exactly\"\n",
                "}}\n"
            ),
            count = results.len(),
            note = json_escape(&note),
            fs = filesystems,
        );
        std::fs::write(path, json).unwrap();
    }
}
