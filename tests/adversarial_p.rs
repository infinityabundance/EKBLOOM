//! Independent adversarial/security review of round **P1–P8**.
//!
//! These tests are deliberately written *against the public API* and do not
//! reuse the round's own test helpers, so they are an independent oracle for:
//!
//! - **P1 versioning** — write→read for machines **1–9** (the shipped
//!   `tests/format_version.rs` only covers 1–8), typed rejection of a future
//!   `format_minor`/`core_version` for every machine including the shared
//!   dictionary (9), and legacy (`core_version = 0`) still reading.
//! - **P2 streaming** — byte-exact `read_at`/`read_sequential` at adversarial
//!   offsets through a **variable** (mid-stream `checkpoint`) layout, the final
//!   partial block, and typed rejection of tampered/truncated containers.
//! - **P5 dictionary** — a referenced core whose id is registered but *wrong*
//!   is not silently rewired by the reader's own manifest linkage.
//! - **P3 corruption** — an independent single-bit flip sweep over the core,
//!   the manifest payload and the commit footer.
//! - **SEG linearity** — the new `Seg::decode`/`state_log` overrides agree
//!   byte-for-byte with the *unmodified* per-symbol `reveal` reference path.

use std::io::Cursor;
use std::path::PathBuf;

use ekbloom::format::{
    self, GenesisHeader, CORE_VERSION_CURRENT, CORE_VERSION_LEGACY, FORMAT_MINOR, HEADER_LEN,
};
use ekbloom::{
    create, create_machine, machine_by_id, open, read_file, verify, EkbloomError,
    ManifestationProfile, OrMachine, ReadRequest, RepresentationPolicy, StreamBuilder,
    StreamOptions, StreamReader, MACHINE_DICT, MACHINE_SEG, SEG,
};

const OFF_FORMAT_MAJOR: usize = 8;
const OFF_FORMAT_MINOR: usize = 10;
const OFF_CORE_VERSION: usize = 68;
/// The streaming container's fixed header length (payload starts here).
const STREAM_HEADER: usize = 64;

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-advp-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
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
    let mut out = skewed(seed, half);
    out.extend_from_slice(&lcg(seed ^ 0x5EED, len - half));
    out
}

// ---------------------------------------------------------------------------
// P1 — versioning, machines 1–9.
// ---------------------------------------------------------------------------

#[test]
fn p1_machines_1_to_9_version_roundtrip_and_future_rejection() {
    let dir = temp_dir("p1");
    let src = skewed(0xA1, 4096);
    let mut checked = 0usize;

    for machine_id in 1..=9u32 {
        let path = dir.join(format!("m{machine_id}.ekb"));
        let body = create_machine(&src, ManifestationProfile::Root, machine_id).expect("create");
        ekbloom::write_body(&path, &body).expect("write");
        let bytes = std::fs::read(&path).expect("read");

        let header = GenesisHeader::decode(&bytes).expect("header");
        assert_eq!(header.format_major, 0);
        assert_eq!(header.format_minor, FORMAT_MINOR);
        assert_eq!(header.core_machine_id, machine_id);
        assert_eq!(header.core_version, CORE_VERSION_CURRENT);

        let opened = open(&path).expect("open");
        assert_eq!(opened.machine_id, machine_id);
        assert_eq!(opened.logical_len, src.len() as u64);
        assert!(verify(&opened).expect("verify").ok, "verify m{machine_id}");
        assert_eq!(
            read_file(
                &path,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64,
                }
            )
            .expect("read"),
            src,
            "roundtrip m{machine_id}"
        );

        // A future minor on the same major line is refused for *every* machine.
        let mut future_minor = bytes.clone();
        future_minor[OFF_FORMAT_MINOR..OFF_FORMAT_MINOR + 2]
            .copy_from_slice(&(FORMAT_MINOR + 1).to_le_bytes());
        assert!(
            matches!(
                GenesisHeader::decode(&future_minor),
                Err(EkbloomError::UnsupportedVersion { .. })
            ),
            "m{machine_id}: future minor accepted"
        );

        // A future per-machine core version is refused for every machine,
        // including the shared-dictionary machine (9).
        let mut future_core = bytes.clone();
        future_core[OFF_CORE_VERSION..OFF_CORE_VERSION + 2]
            .copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(
            matches!(
                GenesisHeader::decode(&future_core),
                Err(EkbloomError::UnsupportedCoreVersion { machine_id: m, .. }) if m == machine_id
            ),
            "m{machine_id}: future core_version accepted"
        );
        let fpath = dir.join(format!("future_m{machine_id}.ekb"));
        std::fs::write(&fpath, &future_core).expect("write");
        assert!(matches!(
            open(&fpath),
            Err(EkbloomError::UnsupportedCoreVersion { .. })
        ));

        // A legacy (formerly-reserved) core version still reads exactly.
        let mut legacy = bytes.clone();
        legacy[OFF_CORE_VERSION..OFF_CORE_VERSION + 2]
            .copy_from_slice(&CORE_VERSION_LEGACY.to_le_bytes());
        let lpath = dir.join(format!("legacy_m{machine_id}.ekb"));
        std::fs::write(&lpath, &legacy).expect("write");
        let lbody = open(&lpath).expect("legacy open");
        assert_eq!(lbody.logical_len, src.len() as u64);
        assert!(verify(&lbody).expect("legacy verify").ok);

        checked += 1;
    }

    assert_eq!(checked, 9, "all nine machines must version-round-trip");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p1_future_format_major_is_rejected_before_any_parse() {
    let dir = temp_dir("p1major");
    let src = b"future format major probe".to_vec();
    let path = dir.join("m1.ekb");
    let body = create(&src, ManifestationProfile::Root).expect("create");
    ekbloom::write_body(&path, &body).expect("write");
    let mut bytes = std::fs::read(&path).expect("read");

    bytes[OFF_FORMAT_MAJOR..OFF_FORMAT_MAJOR + 2].copy_from_slice(&1u16.to_le_bytes());
    let cpath = dir.join("maj.ekb");
    std::fs::write(&cpath, &bytes).expect("write");
    assert!(matches!(
        open(&cpath),
        Err(EkbloomError::UnsupportedVersion { major: 1, .. })
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P2 — streaming: variable layout, adversarial offsets, typed rejection.
// ---------------------------------------------------------------------------

fn build_variable(src: &[u8]) -> Vec<u8> {
    let mut sink = Vec::new();
    {
        let mut b = StreamBuilder::new(
            &mut sink,
            StreamOptions {
                block_bytes: 1024,
                machine: RepresentationPolicy::Auto,
            },
        )
        .expect("builder");
        // Irregular checkpoints make the directory layout *variable* (the
        // `O(1)` division is invalid; the reader must binary-search).
        b.write(&src[..700]).expect("w");
        b.checkpoint().expect("cp");
        b.write(&src[700..2500]).expect("w");
        b.checkpoint().expect("cp");
        b.write(&src[2500..6000]).expect("w");
        b.checkpoint().expect("cp");
        b.write(&src[6000..]).expect("w");
        b.finish().expect("finish");
    }
    sink
}

#[test]
fn p2_variable_layout_reads_are_byte_exact_at_adversarial_offsets() {
    let src = mixed(0xA2, 9000);
    let bytes = build_variable(&src);
    let reader = StreamReader::open(Cursor::new(bytes.clone())).expect("open");
    assert_eq!(reader.len(), src.len() as u64);
    assert!(!reader.entries().is_empty());

    // read_sequential == source, through odd-sized buffers over block edges.
    let mut got = Vec::new();
    let sizes = [1usize, 3, 999, 1024, 1025];
    let mut i = 0;
    loop {
        let mut buf = vec![0u8; sizes[i % sizes.len()]];
        let n = reader.read_sequential(&mut buf).expect("seq");
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
        i += 1;
    }
    assert_eq!(got, src, "sequential readback");

    // Every block boundary and boundary-1 is exact, plus a full sweep of
    // assorted spans across multiple variable blocks.
    let n = src.len();
    for e in reader.entries() {
        let s = e.logical_start as usize;
        let end = (e.logical_start + e.logical_len) as usize;
        let mut one = [0u8; 1];
        reader.read_at(e.logical_start, &mut one).expect("start");
        assert_eq!(one[0], src[s]);
        reader.read_at((end - 1) as u64, &mut one).expect("end-1");
        assert_eq!(one[0], src[end - 1]);
    }
    for &len in &[1usize, 2, 7, 1023, 1024, 1025, 4096] {
        let mut off = 0usize;
        while off + len <= n {
            let mut buf = vec![0u8; len];
            reader.read_at(off as u64, &mut buf).expect("read_at");
            assert_eq!(buf, &src[off..off + len], "len={len} off={off}");
            off += 137;
        }
    }
    // The final partial block and end-of-stream edge cases.
    let mut tail = vec![0u8; 5];
    reader.read_at((n - 5) as u64, &mut tail).expect("tail");
    assert_eq!(tail, &src[n - 5..]);
    assert!(matches!(
        reader.read_at(n as u64, &mut [0u8; 1]),
        Err(EkbloomError::OutOfBounds { .. })
    ));
    reader.read_at(n as u64, &mut []).expect("empty at end");
}

#[test]
fn p2_tampered_truncated_and_wrong_digest_are_typed_errors() {
    let src = mixed(0xB2, 6000);
    let bytes = build_variable(&src);

    // Corrupt a block payload (the very first payload byte) -> content id fails
    // on read, not silently wrong.
    let mut p = bytes.clone();
    p[STREAM_HEADER] ^= 0x01;
    let reader = StreamReader::open(Cursor::new(p)).expect("geometry still valid");
    assert!(matches!(
        reader.read_at(0, &mut [0u8; 1]),
        Err(EkbloomError::ChecksumMismatch { .. })
    ));

    // Corrupt the directory -> digest mismatch at open.
    let footer_at = bytes.len() - 160;
    let dir_off =
        u64::from_le_bytes(bytes[footer_at + 40..footer_at + 48].try_into().unwrap()) as usize;
    let mut d = bytes.clone();
    d[dir_off] ^= 0x01;
    assert!(matches!(
        StreamReader::open(Cursor::new(d)),
        Err(EkbloomError::ChecksumMismatch { .. })
    ));

    // One byte short of the minimum container size.
    assert!(matches!(
        StreamReader::open(Cursor::new(bytes[..64].to_vec())),
        Err(EkbloomError::Truncated { .. })
    ));
    // Head truncated below the footer.
    assert!(StreamReader::open(Cursor::new(bytes[..bytes.len() - 1].to_vec())).is_err());
}

// ---------------------------------------------------------------------------
// P5 — a registered-but-wrong referenced dictionary id must not be rewired
//      silently by the reader's own manifest linkage.
// ---------------------------------------------------------------------------

#[test]
fn p5_relabelled_referenced_dictionary_is_caught_by_core_linkage() {
    use ekbloom::{register_dictionary, train_dictionary};

    let dir = temp_dir("p5");
    let mut src = Vec::new();
    for _ in 0..3 {
        src.extend_from_slice(b"id=user|kind=alpha|region=us-east-1|");
    }

    let corpus: Vec<Vec<u8>> = (0..32)
        .map(|i| format!("id={i}|kind=alpha|region=us-east-1|").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");
    register_dictionary(&dict).expect("register");

    let body = ekbloom::create_dictionary(&src, ManifestationProfile::Root, &dict).expect("body");
    let path = dir.join("ref.ekb");
    ekbloom::write_body(&path, &body).expect("write");
    let bytes = std::fs::read(&path).expect("read");

    // The honest, registered reference opens and verifies.
    assert!(verify(&open(&path).expect("open")).expect("verify").ok);

    // Relabel the core's 16-byte dictionary id (bytes 80..96) to a *different*
    // value: the core changes, so the manifest `core_hash` linkage rejects it.
    let mut bad = bytes.clone();
    bad[HEADER_LEN] ^= 0xFF;
    let bpath = dir.join("bad.ekb");
    std::fs::write(&bpath, &bad).expect("write");
    assert!(open(&bpath).is_err(), "relabelled core must not open");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P3 — independent single-bit flip sweep (core, manifest payload, footer).
// ---------------------------------------------------------------------------

#[test]
fn p3_single_bit_flips_in_core_manifest_and_footer_are_detected() {
    let dir = temp_dir("p3");
    let src = skewed(0xC3, 2048);
    let body = create(&src, ManifestationProfile::Root).expect("body");
    let path = dir.join("c.ekb");
    ekbloom::write_body(&path, &body).expect("write");
    let bytes = std::fs::read(&path).expect("read");
    let header = GenesisHeader::decode(&bytes).expect("header");
    let core_end = HEADER_LEN + header.core_len as usize;

    let scan = format::scan(&bytes).expect("scan");
    let manifest = scan
        .commits
        .last()
        .and_then(|c| {
            c.sections
                .iter()
                .find(|s| s.kind == format::SectionKind::Manifest)
        })
        .expect("manifest section");
    let m_start = manifest.payload_offset as usize;
    let m_end = m_start + manifest.payload_len as usize;

    let footer_start = bytes.len() - 64;

    let mut flips = 0usize;
    // Core region: every byte is covered by the manifest core_hash.
    for pos in HEADER_LEN..core_end {
        assert!(
            flip_detected(&bytes, pos),
            "core flip at {pos} accepted silently"
        );
        flips += 1;
    }
    // Manifest payload: every byte is covered by its section frame CRC-32.
    for pos in m_start..m_end {
        assert!(
            flip_detected(&bytes, pos),
            "manifest flip at {pos} accepted silently"
        );
        flips += 1;
    }
    // Commit footer: all 64 bytes are covered (56..60 is the checksum itself).
    for pos in footer_start..bytes.len() {
        assert!(
            flip_detected(&bytes, pos),
            "footer flip at {pos} accepted silently"
        );
        flips += 1;
    }
    assert!(flips > 2000, "sweep was too small: {flips} flips");

    // Sanity: the clean image opens, so the sweep is not trivially failing.
    assert!(ekbloom::reader::open_bytes(&bytes).is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

fn flip_detected(bytes: &[u8], pos: usize) -> bool {
    let mut t = bytes.to_vec();
    t[pos] ^= 1 << 3;
    ekbloom::reader::open_bytes(&t).is_err()
}

// ---------------------------------------------------------------------------
// SEG linearity — the override must equal the unmodified reveal reference.
// ---------------------------------------------------------------------------

#[test]
fn seq_decode_and_state_log_match_the_reveal_reference() {
    let machine = machine_by_id(MACHINE_SEG).expect("machine 6");
    for &n in &[1usize, 2, 3, 17, 255, 1000, 4096, 65537] {
        let src = mixed(0x6E, n);
        let core = machine.encode(&src).expect("encode");

        // Authoritative reveal reference (the *unmodified* per-symbol path).
        let mut omega = SEG.init(&core).expect("init");
        let mut reference = Vec::with_capacity(n);
        for _ in 0..n {
            reference.push(SEG.reveal(&core, &mut omega).expect("reveal"));
        }
        assert_eq!(reference, src, "reveal reference n={n}");

        // The new whole-object override must agree byte-for-byte.
        let via_decode = machine.decode(&core, n as u64).expect("decode");
        assert_eq!(via_decode, src, "override decode n={n}");
        assert_eq!(via_decode, reference, "override vs reference n={n}");

        // state_log is [0, s_0, ..., s_{n-1}].
        let log = machine.state_log(&core, n as u64).expect("state_log");
        assert_eq!(log.len(), n + 1);
        assert_eq!(log[0], 0);
        assert!(log[1..].iter().zip(&src).all(|(&x, &b)| x == u32::from(b)));

        // A mismatched declared length is a typed error, never a short decode.
        assert!(machine.decode(&core, n as u64 + 1).is_err());
    }

    // The stream `Segmented` policy exercises the same override across blocks.
    let src = mixed(0x66, 50_000);
    let mut sink = Vec::new();
    {
        let mut b = StreamBuilder::new(
            &mut sink,
            StreamOptions {
                block_bytes: 4096,
                machine: RepresentationPolicy::Segmented,
            },
        )
        .expect("builder");
        for chunk in src.chunks(777) {
            b.write(chunk).expect("write");
        }
        b.finish().expect("finish");
    }
    let reader = StreamReader::open(Cursor::new(sink)).expect("open");
    let mut got = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = reader.read_sequential(&mut buf).expect("seq");
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, src, "segmented stream roundtrip");
    assert!(reader.entries().iter().any(|e| e.machine_id == MACHINE_SEG));

    // `machine_by_id` must still resolve the shared-dictionary machine (9).
    assert!(machine_by_id(MACHINE_DICT).is_some());
}
