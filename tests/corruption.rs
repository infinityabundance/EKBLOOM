//! Targeted single-bit-flip corruption tests.
//!
//! Each test flips a *single bit* in one structure and asserts the typed,
//! located failure. See `examples/corruption_court.rs` for the statistical
//! 1000+-flips-per-structure court and `docs/CORRUPTION.md` for the model.

use std::path::PathBuf;

use ekbloom::blocks::BlockIndex;
use ekbloom::bloom_context::build_context_shell;
use ekbloom::format::{self, SectionKind};
use ekbloom::machine::MACHINE_NL2;
use ekbloom::tiles::{DirectoryKind, TileSet};
use ekbloom::writer::{write_body, write_body_authenticated};
use ekbloom::{
    create, grow, reader, verify, verify_deep, verify_deep_located, AccessTrace, BloomKind,
    EkbloomBody, EkbloomError, ManifestationBudget, ManifestationProfile, ReadRequest,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

const WORDS: [&[u8]; 6] = [
    b"the ",
    b"ekbloom ",
    b"corruption ",
    b"detection ",
    b"bit ",
    b"flip ",
];

fn corpus(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len + 32);
    while out.len() < len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(WORDS[(s >> 33) as usize % WORDS.len()]);
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if s & 7 == 0 {
            out.push((s >> 40) as u8);
        }
    }
    out.truncate(len);
    out
}

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

/// Build one body carrying every structure kind.
fn rich_body(src: &[u8]) -> EkbloomBody {
    let n = src.len() as u64;
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    grow(&mut body, ManifestationProfile::Full).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    grow(&mut body, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    grow(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
    )
    .unwrap();
    grow(&mut body, ManifestationProfile::Path { depth: 32 }).unwrap();
    body.blooms
        .push(build_context_shell(&body.core, n, 4, body.generation + 1).unwrap());
    let trace = AccessTrace {
        regions: vec![(0, n / 4)],
        counts: vec![100],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    let block = BlockIndex::build_machine(&body.core, n, 256, MACHINE_NL2).unwrap();
    body.blocks.push(block);
    body
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-corruption-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bloom_index(body: &EkbloomBody, kind: BloomKind) -> usize {
    body.blooms
        .iter()
        .position(|s| s.kind == kind)
        .expect("bloom kind present")
}

fn expect_corrupt(
    result: Result<ekbloom::verify::VerificationReport, EkbloomError>,
) -> (String, u64) {
    match result {
        Err(EkbloomError::Corrupt { section, offset }) => (section.to_string(), offset),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// The first absolute byte offset that is pure alignment padding in `bytes`.
fn first_padding(bytes: &[u8]) -> Option<u64> {
    let scan = format::scan(bytes).ok()?;
    let core_end = format::CORE_OFFSET + scan.header.core_len;
    let base = format::align8(core_end);
    if base > core_end {
        return Some(core_end);
    }
    for commit in &scan.commits {
        for section in &commit.sections {
            let end = section.payload_offset + section.payload_len;
            let aligned = format::align8(end);
            if aligned > end {
                return Some(end);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Clean body.
// ---------------------------------------------------------------------------

#[test]
fn clean_rich_body_verifies() {
    let src = corpus(0x11, 4096);
    let body = rich_body(&src);
    assert!(verify(&body).unwrap().ok, "shallow verify on a clean body");
    assert!(
        verify_deep(&body).unwrap().ok,
        "deep verify on a clean body"
    );
    assert!(verify_deep_located(&body).is_ok());
}

// ---------------------------------------------------------------------------
// Layered core (V) and history (L).
// ---------------------------------------------------------------------------

#[test]
fn core_bit_flip_is_located_to_the_core() {
    let src = corpus(0x21, 512);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    body.core[3] ^= 1 << 6;
    let (section, _) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "core");
}

#[test]
fn history_bit_flip_is_located_to_history_at_the_symbol_offset() {
    let src = corpus(0x22, 512);
    let mut body = create(&src, ManifestationProfile::Full).unwrap();
    let i = 7usize;
    body.history.as_mut().unwrap().states[i] ^= 1 << 4;
    let (section, offset) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "history");
    assert_eq!(offset, i as u64, "logical symbol offset of the divergence");
}

#[test]
fn checkpoint_bit_flip_is_located_to_the_restart_ordinal() {
    let src = corpus(0x23, 512);
    let interval = 64u32;
    let j = 3usize;
    let mut body = create(&src, ManifestationProfile::Sparse { interval }).unwrap();
    body.checkpoints[0].resume[j].state ^= 1 << 5;
    let (section, offset) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "checkpoint");
    assert_eq!(offset, j as u64 * u64::from(interval));
}

// ---------------------------------------------------------------------------
// Transition blooms T_D.
// ---------------------------------------------------------------------------

#[test]
fn global_bloom_flip_is_located_to_bloom_global() {
    let src = corpus(0x24, 512);
    let mut body = create(&src, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    let idx = bloom_index(&body, BloomKind::Universal);
    // Byte 22 is the first presence-mask byte; any bit flip changes presence.
    body.blooms[idx].payload[22] ^= 1;
    let (section, _) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "bloom-global");
}

#[test]
fn contextual_bloom_flip_is_located_to_bloom_context() {
    let src = corpus(0x25, 512);
    let n = src.len() as u64;
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    body.blooms
        .push(build_context_shell(&body.core, n, 4, 1).unwrap());
    let idx = bloom_index(&body, BloomKind::Contextual);
    body.blooms[idx].payload[20] ^= 1; // first record's `symbols[0]`
    let (section, _) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "bloom-context");
}

#[test]
fn regional_bloom_flip_is_located_to_bloom_region() {
    let src = corpus(0x26, 512);
    let mut body = create(
        &src,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
    )
    .unwrap();
    let idx = bloom_index(&body, BloomKind::Regional);
    body.blooms[idx].payload[48] ^= 1; // first record's `symbols[0]`
    let (section, _) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "bloom-region");
}

#[test]
fn path_bloom_flip_is_located_to_bloom_path_at_the_symbol_offset() {
    let src = corpus(0x27, 512);
    let mut body = create(&src, ManifestationProfile::Path { depth: 32 }).unwrap();
    let idx = bloom_index(&body, BloomKind::Path);
    let pos = 100usize;
    // Path payload is `depth u16 ++ logical_len u64 ++ nblocks u64` (18 B), then
    // the `N`-byte hot surface.
    body.blooms[idx].payload[18 + pos] ^= 1 << 3;
    let (section, offset) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "bloom-path");
    assert_eq!(offset, pos as u64);
}

// ---------------------------------------------------------------------------
// Blocks and tiles.
// ---------------------------------------------------------------------------

#[test]
fn block_restart_flip_is_located_to_blocks() {
    let src = corpus(0x28, 512);
    let n = src.len() as u64;
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    body.blocks
        .push(BlockIndex::build_machine(&body.core, n, 256, MACHINE_NL2).unwrap());
    body.blocks[0].entries[2].state ^= 1;
    let (section, _) = expect_corrupt(verify_deep_located(&body));
    assert_eq!(section, "blocks");
}

#[test]
fn tile_flip_is_rejected_at_parse_by_the_content_digest() {
    let src = corpus(0x29, 2048);
    let n = src.len() as u64;
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, n / 2)],
        counts: vec![50],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    let mut bytes = body.tiles[0].to_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 1; // flip a byte inside the tile data (not the digest)
    assert!(
        TileSet::from_bytes(&bytes).is_err(),
        "the tile set must reject a tampered data byte via its content digest"
    );
}

// ---------------------------------------------------------------------------
// Manifest, footer, and the section directory.
// ---------------------------------------------------------------------------

#[test]
fn manifest_flip_is_rejected_by_the_fast_path() {
    let src = corpus(0x2A, 2048);
    let dir = temp_dir("manifest");
    let path = dir.join("b.ekb");
    write_body(&path, &body_for(&src)).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    let manifest = scan
        .commits
        .iter()
        .flat_map(|c| &c.sections)
        .find(|s| s.kind == SectionKind::Manifest)
        .unwrap();
    bytes[manifest.payload_offset as usize] ^= 1;
    let err = reader::open_bytes(&bytes).unwrap_err();
    assert!(
        matches!(
            err,
            EkbloomError::ChecksumMismatch {
                section: "manifest"
            }
        ),
        "got {err:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn footer_flip_is_rejected_by_the_fast_path() {
    let src = corpus(0x2B, 2048);
    let dir = temp_dir("footer");
    let path = dir.join("c.ekb");
    write_body(&path, &body_for(&src)).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1; // inside the fixed 64-byte footer
    bytes[last] ^= 1;
    let err = reader::open_bytes(&bytes).unwrap_err();
    assert!(
        matches!(
            err,
            EkbloomError::ChecksumMismatch {
                section: "commit-footer"
            }
        ),
        "got {err:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn section_header_kind_flip_is_rejected_by_the_fast_path() {
    let src = corpus(0x2C, 2048);
    let dir = temp_dir("header");
    let path = dir.join("d.ekb");
    write_body(&path, &body_for(&src)).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    let first = scan.commits[0].sections[0];
    // Flip a bit in the `kind` field of the first section header.
    bytes[first.header_offset as usize] ^= 1;
    assert!(
        reader::open_bytes(&bytes).is_err(),
        "a flipped section kind must be rejected"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Multi-flip and non-interpreted (inert) bytes.
// ---------------------------------------------------------------------------

#[test]
fn multiple_flips_across_structures_are_detected() {
    let src = corpus(0x2D, 2048);
    let mut body = rich_body(&src);
    body.core[1] ^= 1;
    let hi = body.history.as_mut().unwrap();
    hi.states[5] ^= 1 << 2;
    let pi = bloom_index(&body, BloomKind::Path);
    body.blooms[pi].payload[18 + 9] ^= 1;
    body.blocks[0].entries[1].state ^= 1;

    assert!(verify_deep_located(&body).is_err(), "deep verify must fail");

    // The fast path also rejects a multi-flip body.
    let dir = temp_dir("multi");
    let path = dir.join("e.ekb");
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(reader::open_bytes(&bytes).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn padding_flip_is_not_interpreted() {
    let src = corpus(0x2E, 2048);
    let body = rich_body(&src);
    let dir = temp_dir("padding");
    let path = dir.join("f.ekb");
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let pad = first_padding(&bytes).expect("the rich body has alignment padding");

    let mut tampered = bytes.clone();
    tampered[pad as usize] ^= 1 << 3;

    // Unkeyed structural read: padding is ignored, never interpreted.
    let reopened = reader::open_bytes(&tampered).expect("padding is not structural");
    assert!(verify_deep(&reopened).unwrap().ok);

    // And the logical content is byte-identical.
    let req = ReadRequest {
        offset: 0,
        length: 256,
    };
    assert_eq!(
        reader::read(&reopened, req).unwrap(),
        reader::read(&body, req).unwrap()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn inert_section_header_metadata_flip_is_not_interpreted() {
    let src = corpus(0x2F, 2048);
    let body = rich_body(&src);
    let dir = temp_dir("inert");
    let path = dir.join("g.ekb");
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    let header = scan.commits[0].sections[0].header_offset;

    // `flags` (header+4) and `generation` (header+24) are not validated anywhere.
    for delta in [4u64, 24] {
        let mut tampered = bytes.clone();
        tampered[(header + delta) as usize] ^= 1;
        let reopened = reader::open_bytes(&tampered).expect("inert metadata is not validated");
        assert!(verify_deep(&reopened).unwrap().ok);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// AUTH: integrity is not authenticity.
// ---------------------------------------------------------------------------

#[test]
fn keyed_auth_catches_a_rewrite_that_the_unkeyed_read_ignores() {
    let src = corpus(0x31, 2048);
    let body = rich_body(&src);
    let dir = temp_dir("auth");
    let path = dir.join("h.ekb");
    let key = [0x5Au8; 32];
    write_body_authenticated(&path, &body, &key).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    // Flip a padding byte: inside the MAC-covered range, outside every CRC and
    // every semantic check.
    let pad = first_padding(&bytes).expect("the rich body has alignment padding");
    let mut tampered = bytes.clone();
    tampered[pad as usize] ^= 1;

    // The unkeyed read ignores it (corruption-detecting only)...
    assert!(reader::open_bytes(&tampered).is_ok());
    // ...but the keyed read rejects it: integrity is not authenticity.
    let err = reader::open_bytes_require_auth(&tampered, &key).unwrap_err();
    assert!(
        matches!(
            err,
            EkbloomError::ChecksumMismatch { section: "auth" }
                | EkbloomError::Corrupt {
                    section: "auth",
                    ..
                }
                | EkbloomError::CorruptGeneration { .. }
        ),
        "got {err:?}"
    );
    // The honest file still authenticates.
    assert!(reader::open_bytes_require_auth(&bytes, &key).is_ok());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A small body used by the file-level fast-path tests.
fn body_for(src: &[u8]) -> EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    grow(&mut body, ManifestationProfile::Full).unwrap();
    grow(&mut body, ManifestationProfile::Path { depth: 32 }).unwrap();
    body
}
