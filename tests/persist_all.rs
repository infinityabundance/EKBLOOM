//! Persistence round-trip for **every** manifested structure.
//!
//! The `.ekb` writer publishes each structure present in an [`EkbloomBody`] as a
//! committed section: the full history, checkpoint sets, every bloom kind
//! (universal/global, contextual, regional/region tables, path), certificates,
//! selective tiles, and block indexes. This test pins that contract end to end:
//!
//! for a body that manifests every structure type it
//!
//! * writes the file and asserts each section's raw payload equals the structure's
//!   own `to_bytes()` (so the writer bytes are the canonical encoding);
//! * reopens the file and asserts the reconstructed body is **equal** to the
//!   pre-write body (so the reader-parsed structure == the original structure ==
//!   `to_bytes()`);
//! * asserts `physical_bytes()` is identical before-write and after-reopen;
//! * asserts `byte_at` is byte-exact at sampled positions, through both the
//!   in-memory reader and the file-backed [`FileBody`];
//! * asserts `verify` and `verify_deep` pass after reopen (every reopened
//!   structure is checked).
//!
//! It also covers a MaxSpeed-style body and re-opens the frozen legacy fixtures.

use std::path::{Path, PathBuf};

use ekbloom::bloom_context::build_context_shell_with;
use ekbloom::format::{self, SectionKind};
use ekbloom::{
    byte_at, create_machine, machine_by_id, manifest, open, verify, verify_deep, AccessTrace,
    BlockIndex, BloomKind, BloomShell, Certificate, DirectoryKind, EkbloomBody, FileBody,
    ManifestationBudget, ManifestationProfile, ReadRequest, TileSet, MACHINE_NL2, MACHINE_RANS0,
};

/// A skewed (compressible) source, so machine 2 actually shrinks it.
fn skewed(len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b" etaoinshrdlu,.ET\"'\n";
    const WEIGHTS: &[u32] = &[40, 25, 18, 15, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 3, 2, 2];
    let total: u32 = WEIGHTS.iter().sum();
    let mut s: u64 = 0x5EED_1234_ABCD_0007;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut pick = ((s >> 33) as u32) % total;
        let mut chosen = ALPHABET[0];
        for (i, &w) in WEIGHTS.iter().enumerate() {
            if pick < w {
                chosen = ALPHABET[i];
                break;
            }
            pick -= w;
        }
        out.push(chosen);
    }
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-persist-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sampled logical positions (deterministic, including boundaries).
fn sample_offsets(n: u64) -> Vec<u64> {
    let mut out = vec![0, n / 2, n.saturating_sub(1), n / 4, (3 * n) / 4];
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..16 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if n > 0 {
            out.push(s % n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Build the certificate a bloom shell would carry (mirrors `manifest`'s private
/// `make_certificate`; the receipt hash is recomputed so it is self-consistent).
fn certificate_for(shell: &BloomShell, machine_id: u32, core_hash: [u8; 32]) -> Certificate {
    let (kd, cd, method, vd) = match shell.kind {
        BloomKind::Path => (
            "block-ordinal",
            "block-index",
            "exact-by-construction",
            "every-logical-position",
        ),
        BloomKind::Contextual => (
            "x16++cD",
            "next-D-core-bytes",
            "exact-by-construction",
            "every-observed-context",
        ),
        BloomKind::Regional => (
            "x16-within-region",
            "region-local",
            "region-exhaustive",
            "every-region-state",
        ),
        BloomKind::Universal => (
            "x16",
            "none",
            "state-universe-scan",
            "observed-window-states",
        ),
    };
    let mut cert = Certificate {
        kind: shell.kind,
        depth: shell.depth,
        key_domain: kd.to_string(),
        context_def: cd.to_string(),
        generation: shell.generation,
        machine_id,
        core_hash,
        bloom_hash: shell.content_hash(),
        method: method.to_string(),
        verified_domain: vd.to_string(),
        receipt_hash: [0u8; 32],
        status: shell.status,
    };
    cert.receipt_hash = cert.compute_receipt_hash();
    cert
}

/// A body manifesting every structure type for `machine_id`.
fn kitchen_sink(src: &[u8], machine_id: u32) -> EkbloomBody {
    let machine = machine_by_id(machine_id).unwrap();
    let mut body = create_machine(src, ManifestationProfile::Root, machine_id).unwrap();

    // History, checkpoints, and the three profile-driven bloom kinds.
    manifest(&mut body, ManifestationProfile::Full).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    manifest(&mut body, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    manifest(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 256,
        },
    )
    .unwrap();
    manifest(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();

    // A contextual bloom has no profile, so append one (with its certificate).
    // (It is only built for the cursor==position machine here, where the
    // context-key theorem holds by construction.)
    if machine_id == MACHINE_NL2 {
        let shell = build_context_shell_with(
            machine,
            &body.core,
            body.logical_len,
            4,
            body.generation + 1,
        )
        .unwrap();
        let cert = certificate_for(&shell, machine_id, body.core_hash);
        body.blooms.push(shell);
        body.certificates.push(cert);
        body.generation += 1;
        body.profiles.push("contextual(D=4)".to_string());
    }

    // Selective tiles over a hot run.
    let trace = AccessTrace {
        regions: vec![(0, 4096), (8192, 4096)],
        counts: vec![1000, 900],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();

    // An independently restartable block index.
    let index = BlockIndex::build_machine(&body.core, body.logical_len, 256, machine_id).unwrap();
    body.blocks.push(index);

    body
}

/// Assert every non-manifest/auth section's raw payload equals the corresponding
/// structure's own canonical `to_bytes()`.
fn assert_section_payloads(path: &Path, body: &EkbloomBody) {
    let bytes = std::fs::read(path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    let commit = scan.commits.last().expect("a committed generation");

    let mut want: Vec<(SectionKind, Vec<u8>)> = Vec::new();
    if let Some(h) = &body.history {
        want.push((SectionKind::HistoryFull, h.to_bytes()));
    }
    for cp in &body.checkpoints {
        want.push((SectionKind::Checkpoints, cp.to_bytes()));
    }
    for shell in &body.blooms {
        let kind = match shell.kind {
            BloomKind::Universal => SectionKind::BloomGlobal,
            BloomKind::Contextual => SectionKind::BloomContext,
            BloomKind::Regional => SectionKind::BloomRegion,
            BloomKind::Path => SectionKind::BloomPath,
        };
        want.push((kind, shell.to_bytes()));
    }
    for cert in &body.certificates {
        want.push((SectionKind::Certificate, cert.to_bytes()));
    }
    for ts in &body.tiles {
        want.push((SectionKind::Tiles, ts.to_bytes()));
    }
    for b in &body.blocks {
        want.push((SectionKind::Blocks, b.to_bytes()));
    }

    let got: Vec<(SectionKind, Vec<u8>)> = commit
        .sections
        .iter()
        .filter(|s| s.kind != SectionKind::Manifest && s.kind != SectionKind::Auth)
        .map(|s| (s.kind, format::section_payload(&bytes, s).to_vec()))
        .collect();

    assert_eq!(got.len(), want.len(), "section count");
    for (i, (gk, gp)) in got.iter().enumerate() {
        assert_eq!(*gk, want[i].0, "section {i} kind");
        assert_eq!(*gp, want[i].1, "section {i} payload != to_bytes()");
    }
}

/// Assert the union of every non-manifest/auth section payload across all commits
/// equals the corresponding structure's canonical `to_bytes()` (the append path
/// spreads sections over several commits).
fn assert_all_section_payloads(path: &Path, body: &EkbloomBody) {
    let bytes = std::fs::read(path).unwrap();
    let scan = format::scan(&bytes).unwrap();

    let mut want: Vec<(SectionKind, Vec<u8>)> = Vec::new();
    if let Some(h) = &body.history {
        want.push((SectionKind::HistoryFull, h.to_bytes()));
    }
    for cp in &body.checkpoints {
        want.push((SectionKind::Checkpoints, cp.to_bytes()));
    }
    for shell in &body.blooms {
        let kind = match shell.kind {
            BloomKind::Universal => SectionKind::BloomGlobal,
            BloomKind::Contextual => SectionKind::BloomContext,
            BloomKind::Regional => SectionKind::BloomRegion,
            BloomKind::Path => SectionKind::BloomPath,
        };
        want.push((kind, shell.to_bytes()));
    }
    for cert in &body.certificates {
        want.push((SectionKind::Certificate, cert.to_bytes()));
    }
    for ts in &body.tiles {
        want.push((SectionKind::Tiles, ts.to_bytes()));
    }
    for b in &body.blocks {
        want.push((SectionKind::Blocks, b.to_bytes()));
    }

    let mut got: Vec<(SectionKind, Vec<u8>)> = Vec::new();
    for commit in &scan.commits {
        for s in &commit.sections {
            if s.kind == SectionKind::Manifest || s.kind == SectionKind::Auth {
                continue;
            }
            got.push((s.kind, format::section_payload(&bytes, s).to_vec()));
        }
    }

    assert_eq!(got.len(), want.len(), "persisted section count");
    for (i, (gk, gp)) in got.iter().enumerate() {
        assert_eq!(*gk, want[i].0, "persisted section {i} kind");
        assert_eq!(
            *gp, want[i].1,
            "persisted section {i} payload != to_bytes()"
        );
    }
}

/// Write `body`, reopen, and assert full equivalence.
fn roundtrip(dir: &Path, name: &str, src: &[u8], body: &EkbloomBody) {
    let path = dir.join(name);
    let physical = body.physical_bytes();
    let n = src.len() as u64;

    // The pre-write body verifies.
    assert!(verify(body).unwrap().ok, "{name}: pre-write verify");
    assert!(
        verify_deep(body).unwrap().ok,
        "{name}: pre-write verify_deep"
    );

    ekbloom::write_body(&path, body).unwrap();

    // The writer emitted each structure as its own canonical bytes.
    assert_section_payloads(&path, body);

    // Reopen: the reconstructed body equals the pre-write body exactly.
    let opened = open(&path).unwrap();
    assert_eq!(opened, *body, "{name}: reopened body differs");
    assert_eq!(opened.physical_bytes(), physical, "{name}: physical_bytes");
    assert_eq!(opened.logical_len, n, "{name}: logical_len");

    // Structures are present (no silent dropping).
    assert_eq!(
        opened.history.is_some(),
        body.history.is_some(),
        "{name}: history presence"
    );
    assert_eq!(opened.checkpoints.len(), body.checkpoints.len());
    assert_eq!(opened.blooms.len(), body.blooms.len());
    assert_eq!(opened.certificates.len(), body.certificates.len());
    assert_eq!(opened.tiles.len(), body.tiles.len());
    assert_eq!(opened.blocks.len(), body.blocks.len());

    // Every bloom kind the body manifested survived (no silent dropping).
    for &kind in &[
        BloomKind::Universal,
        BloomKind::Contextual,
        BloomKind::Regional,
        BloomKind::Path,
    ] {
        let want = body.blooms.iter().any(|b| b.kind == kind);
        let got = opened.blooms.iter().any(|b| b.kind == kind);
        assert_eq!(got, want, "{name}: bloom kind {kind:?} persistence");
    }

    // byte_at is byte-exact before and after reopen, through both readers.
    for &off in &sample_offsets(n) {
        let want = src[off as usize];
        assert_eq!(
            byte_at(body, off).unwrap(),
            want,
            "{name}: pre byte_at({off})"
        );
        assert_eq!(
            byte_at(&opened, off).unwrap(),
            want,
            "{name}: post byte_at({off})"
        );
    }

    // verify/verify_deep check every reopened structure.
    assert!(verify(&opened).unwrap().ok, "{name}: verify after reopen");
    assert!(
        verify_deep(&opened).unwrap().ok,
        "{name}: verify_deep after reopen"
    );

    // The file-backed reader agrees byte-for-byte at the samples.
    let file_body = FileBody::open(&path).unwrap();
    for &off in &sample_offsets(n) {
        let got = file_body
            .read(ReadRequest {
                offset: off,
                length: 1,
            })
            .unwrap();
        assert_eq!(got[0], src[off as usize], "{name}: FileBody byte_at({off})");
    }
    let whole = file_body
        .read(ReadRequest {
            offset: 0,
            length: n,
        })
        .unwrap();
    assert_eq!(whole, src, "{name}: FileBody whole object");

    std::fs::remove_file(&path).ok();
}

#[test]
fn every_structure_round_trips_on_machine1() {
    let dir = temp_dir("m1");
    let src = skewed(48 * 1024);
    let body = kitchen_sink(&src, MACHINE_NL2);
    assert!(!body.history.is_none());
    assert!(!body.checkpoints.is_empty());
    assert!(!body.blooms.is_empty());
    assert!(!body.certificates.is_empty());
    assert!(!body.tiles.is_empty());
    assert!(!body.blocks.is_empty());
    roundtrip(&dir, "k1.ekb", &src, &body);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn every_structure_round_trips_on_machine2() {
    // Machine 2 is cursor-divergent: its checkpoints store complete (state,
    // cursor) restarts and its regional records store successor cursors, so
    // persistence must preserve those too.
    let dir = temp_dir("m2");
    let src = skewed(48 * 1024);
    let body = kitchen_sink(&src, MACHINE_RANS0);
    assert_eq!(body.machine_id, MACHINE_RANS0);
    assert!(body.core.len() < src.len(), "machine 2 should compress");
    roundtrip(&dir, "k2.ekb", &src, &body);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn each_structure_individually_round_trips() {
    let dir = temp_dir("each");
    let src = skewed(24 * 1024);

    // History alone.
    let mut b = create_machine(&src, ManifestationProfile::Full, MACHINE_NL2).unwrap();
    assert!(b.history.is_some());
    roundtrip(&dir, "history.ekb", &src, &b);

    // Checkpoints alone.
    b = create_machine(
        &src,
        ManifestationProfile::Sparse { interval: 256 },
        MACHINE_NL2,
    )
    .unwrap();
    assert_eq!(b.checkpoints.len(), 1);
    roundtrip(&dir, "checkpoints.ekb", &src, &b);

    // Global/universal bloom alone.
    b = create_machine(&src, ManifestationProfile::Bloom { depth: 4 }, MACHINE_NL2).unwrap();
    assert!(b.blooms.iter().any(|s| s.kind == BloomKind::Universal));
    roundtrip(&dir, "global.ekb", &src, &b);

    // Regional (region table) alone.
    b = create_machine(
        &src,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 256,
        },
        MACHINE_NL2,
    )
    .unwrap();
    assert!(b.blooms.iter().any(|s| s.kind == BloomKind::Regional));
    roundtrip(&dir, "regional.ekb", &src, &b);

    // Path bloom alone.
    b = create_machine(&src, ManifestationProfile::Path { depth: 4 }, MACHINE_NL2).unwrap();
    assert!(b.blooms.iter().any(|s| s.kind == BloomKind::Path));
    roundtrip(&dir, "path.ekb", &src, &b);

    // Tiles alone.
    b = create_machine(&src, ManifestationProfile::Root, MACHINE_NL2).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4096)],
        counts: vec![1000],
    };
    TileSet::manifest_hot_tiles(
        &mut b,
        &trace,
        12,
        DirectoryKind::Flat,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();
    assert_eq!(b.tiles.len(), 1);
    roundtrip(&dir, "tiles.ekb", &src, &b);

    // Blocks alone.
    b = create_machine(&src, ManifestationProfile::Root, MACHINE_NL2).unwrap();
    b.blocks
        .push(BlockIndex::build_machine(&b.core, b.logical_len, 256, MACHINE_NL2).unwrap());
    roundtrip(&dir, "blocks.ekb", &src, &b);

    // A certificate linked to a manifested bloom.
    b = create_machine(&src, ManifestationProfile::Path { depth: 4 }, MACHINE_NL2).unwrap();
    assert!(!b.certificates.is_empty());
    roundtrip(&dir, "certificate.ekb", &src, &b);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn append_generation_persists_new_structures() {
    // The incremental publication path (append_generation / new_sections) must
    // emit the *new* structures too, not only the ones present at creation.
    let dir = temp_dir("append");
    let path = dir.join("append.ekb");
    let src = skewed(40 * 1024);
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_NL2).unwrap();
    ekbloom::write_body(&path, &body).unwrap();

    // Add every structure as later generations.
    manifest(&mut body, ManifestationProfile::Full).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    manifest(&mut body, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    manifest(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 256,
        },
    )
    .unwrap();
    manifest(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4096)],
        counts: vec![1000],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::Flat,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();
    body.blocks
        .push(BlockIndex::build_machine(&body.core, body.logical_len, 256, MACHINE_NL2).unwrap());

    let physical = body.physical_bytes();
    let gen = ekbloom::append_generation(&path, &body).unwrap();
    assert_eq!(gen, body.generation);

    assert_all_section_payloads(&path, &body);

    let opened = open(&path).unwrap();
    assert_eq!(opened, body, "append: reopened body differs");
    assert_eq!(opened.physical_bytes(), physical, "append: physical_bytes");
    assert!(verify(&opened).unwrap().ok);
    assert!(verify_deep(&opened).unwrap().ok);
    for &off in &sample_offsets(src.len() as u64) {
        assert_eq!(byte_at(&opened, off).unwrap(), src[off as usize]);
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn maxspeed_body_round_trips() {
    let dir = temp_dir("maxspeed");
    let src = skewed(64 * 1024);
    let body = create_machine(&src, ManifestationProfile::MaxSpeed, MACHINE_RANS0).unwrap();
    assert!(body.history.is_some());
    assert_eq!(body.checkpoints.len(), 1);
    assert!(body.blooms.iter().any(|s| s.kind == BloomKind::Path));
    roundtrip(&dir, "maxspeed.ekb", &src, &body);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn legacy_frozen_fixtures_still_open_and_verify() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("ekb") {
            continue;
        }
        count += 1;
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let body = open(&path).unwrap_or_else(|e| panic!("{name}: open: {e}"));
        assert!(verify(&body).unwrap().ok, "{name}: verify");
        assert!(verify_deep(&body).unwrap().ok, "{name}: verify_deep");
        let file_body = FileBody::open(&path).unwrap();
        assert_eq!(file_body.logical_len(), body.logical_len, "{name}");
    }
    assert!(count >= 12, "expected the frozen fixtures, found {count}");
}
