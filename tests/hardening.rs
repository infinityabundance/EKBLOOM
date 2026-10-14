//! Adversarial-input hardening regressions.
//!
//! One regression per adversarial class the untrusted boundary must survive:
//! malformed declared lengths, allocation exhaustion near/past the
//! [`ekbloom::ResourcePolicy`] ceiling, dictionary substitution, authentication
//! downgrade/rollback, and truncation of a backing file under an active reader.
//! Every case asserts a **typed** [`EkbloomError`] (never a panic, never an OOM,
//! never a wrong byte) and that the work is bounded in time. See
//! `docs/HARDENING.md`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ekbloom::blocks::BlockIndex;
use ekbloom::checkpoint::CheckpointSet;
use ekbloom::format::{self, SectionKind};
use ekbloom::history::History;
use ekbloom::machine::MACHINE_NL2;
use ekbloom::machine_pack::{PackContext, PACK};
use ekbloom::machine_seg::{SegContext, SEG, SEG_HEADER_BYTES};
use ekbloom::tiles::TileSet;
use ekbloom::writer::{write_body, write_body_authenticated};
use ekbloom::{
    create, create_dictionary, frame_dictionary_embedded, frame_dictionary_reference, grow, read,
    reader, train_dictionary, Dictionary, EkbloomError, ManifestationProfile, MappedSurface,
    OrMachine, ReadRequest, ResourcePolicy, DEFAULT_MAX_SINGLE_ALLOCATION, DICT,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

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

fn text_like(seed: usize, len: usize) -> Vec<u8> {
    let parts = [
        "id=",
        "user",
        "kind=",
        "alpha",
        "beta",
        "gamma",
        "region=",
        "us-east-1",
        "|",
    ];
    let mut out = Vec::with_capacity(len + 8);
    let mut k = seed as u64;
    while out.len() < len {
        k = k
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(parts[((k >> 33) as usize) % parts.len()].as_bytes());
    }
    out.truncate(len);
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-hardening-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A valid `.ekb` image (`Root`) and its logical source.
fn valid_image(src_len: usize) -> (Vec<u8>, Vec<u8>) {
    let dir = temp_dir("image");
    let src = lcg(0x4A2D_E0C0, src_len);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    let path = dir.join("v.ekb");
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (bytes, src)
}

/// Assert `f` returns within a generous wall-clock bound (proves termination).
fn within<T>(bound: Duration, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    assert!(
        elapsed < bound,
        "adversarial input took {elapsed:?}, over the {bound:?} bound"
    );
    out
}

/// Genesis header offset of the core length (little-endian u64).
const HDR_CORE_LEN: usize = 56;

// ---------------------------------------------------------------------------
// 1. Malformed declared lengths.
// ---------------------------------------------------------------------------

#[test]
fn malformed_declared_lengths_are_typed_errors() {
    let bound = Duration::from_secs(5);

    // (a) A history header declaring `u64::MAX` positions and no state bytes.
    let mut history = vec![0u8; 64];
    history[8..16].copy_from_slice(&u64::MAX.to_le_bytes()); // logical_len
    history[56..64].copy_from_slice(&u64::MAX.to_le_bytes()); // count == logical_len + 1
    let e = within(bound, || History::from_bytes(&history)).unwrap_err();
    assert!(
        matches!(
            e,
            EkbloomError::Overflow { .. } | EkbloomError::Truncated { .. }
        ),
        "history: {e}"
    );

    // (b) A compact checkpoint header declaring `u64::MAX` records.
    let mut ckpt = Vec::new();
    ckpt.extend_from_slice(b"EKBCKPT\0");
    ckpt.push(2); // version
    ckpt.push(1); // format: packed
    ckpt.extend_from_slice(&1u32.to_le_bytes()); // interval
    ckpt.extend_from_slice(&u64::MAX.to_le_bytes()); // count
    let e = within(bound, || CheckpointSet::from_bytes(&ckpt)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::Overflow { .. }),
        "checkpoint: {e}"
    );

    // (c) A block index prefix declaring an astronomical `logical_len` (the entry
    //     count is `logical_len / block_bytes + 1`, which overflows for a
    //     `block_bytes` of 1 and `logical_len == u64::MAX`).
    let mut blk = Vec::new();
    blk.extend_from_slice(b"EKBKBLKS");
    blk.push(1); // version
    blk.push(2); // format: full (u32 state ++ u64 cursor)
    blk.extend_from_slice(&MACHINE_NL2.to_le_bytes());
    blk.extend_from_slice(&1u32.to_le_bytes()); // block_bytes = 1
    blk.extend_from_slice(&u64::MAX.to_le_bytes()); // logical_len
    blk.resize(104, 0); // BLOCK_INDEX_PREFIX_LEN, reserved bytes zero
    let e = within(bound, || BlockIndex::from_bytes(&blk)).unwrap_err();
    assert!(matches!(e, EkbloomError::Overflow { .. }), "blocks: {e}");

    // (d) A valid image with its genesis `core_len` set to `u64::MAX`.
    let (mut img, _src) = valid_image(1024);
    img[HDR_CORE_LEN..HDR_CORE_LEN + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let e = within(bound, || reader::open_bytes(&img)).unwrap_err();
    assert!(
        matches!(
            e,
            EkbloomError::Overflow { .. } | EkbloomError::Truncated { .. }
        ),
        "core_len: {e}"
    );

    // (e) A valid image with a section header's declared `length` set to
    //     `u64::MAX` (the payload extent overflows).
    let (mut img, _src) = valid_image(1024);
    let scan = format::scan(&img).unwrap();
    let section = scan.commits.last().unwrap().sections.last().unwrap();
    let ho = section.header_offset as usize;
    img[ho + 16..ho + 24].copy_from_slice(&u64::MAX.to_le_bytes());
    let e = within(bound, || reader::open_bytes(&img)).unwrap_err();
    assert!(
        matches!(
            e,
            EkbloomError::Overflow { .. } | EkbloomError::Truncated { .. }
        ),
        "section length: {e}"
    );

    // (f) Arbitrary bytes to the raw tile/block/dictionary parsers must not panic.
    let junk = lcg(0xBAD0_0001, 4096);
    within(bound, || {
        let _ = TileSet::from_bytes(&junk);
        let _ = BlockIndex::from_bytes(&junk);
        let _ = History::from_bytes(&junk);
        let _ = Dictionary::from_bytes(&junk);
        let _ = reader::open_bytes(&junk);
    });
}

// ---------------------------------------------------------------------------
// 2. Allocation exhaustion near/past the resource-policy ceiling.
// ---------------------------------------------------------------------------

/// A machine-5 constant core (`k == 1`, empty payload) declaring `n` symbols.
fn forged_constant_pack_core(n: u64) -> Vec<u8> {
    let mut core = vec![1u8, 1u8, 0xAB]; // bits = 1, k = 1, constant symbol
    core.extend_from_slice(&n.to_le_bytes());
    core
}

/// A machine-6 fixed container with one constant segment covering `n` symbols,
/// with the directory BLAKE3 recomputed so the (self-consistent) forgery parses.
fn forged_constant_seg_core(n: u64) -> Vec<u8> {
    const H_LOGICAL_LEN: usize = 24;
    const H_DIRECTORY_HASH: usize = 56;
    const E_LOGICAL_LEN: usize = 8;
    let mut core = SEG.encode(&[7u8; 4]).unwrap();
    core[H_LOGICAL_LEN..H_LOGICAL_LEN + 8].copy_from_slice(&n.to_le_bytes());
    let dir_off = SEG_HEADER_BYTES;
    core[dir_off + E_LOGICAL_LEN..dir_off + E_LOGICAL_LEN + 8].copy_from_slice(&n.to_le_bytes());
    let hash = blake3::hash(&core[dir_off..]);
    core[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32].copy_from_slice(hash.as_bytes());
    core
}

#[test]
fn allocation_exhaustion_is_refused_not_attempted() {
    let bound = Duration::from_secs(5);

    // Machine 5: a ~11-byte core declaring `u64::MAX` symbols is refused before
    // any allocation.
    let core5 = forged_constant_pack_core(u64::MAX);
    assert_eq!(
        PackContext::from_core(&core5).unwrap().logical_len(),
        u64::MAX
    );
    let e = within(bound, || PACK.decode(&core5, u64::MAX)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::AllocationTooLarge { requested, limit }
            if requested == u64::MAX && limit == DEFAULT_MAX_SINGLE_ALLOCATION),
        "pack: {e}"
    );

    // Just past the ceiling (the ceiling itself is legal, one byte over is not).
    let one_past = DEFAULT_MAX_SINGLE_ALLOCATION + 1;
    let core5p = forged_constant_pack_core(one_past);
    let e = within(bound, || PACK.decode(&core5p, one_past)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::AllocationTooLarge { requested, .. } if requested == one_past),
        "pack near-limit: {e}"
    );

    // Machine 6 fixed container.
    let core6 = forged_constant_seg_core(u64::MAX);
    assert_eq!(
        SegContext::from_core(&core6).unwrap().logical_len(),
        u64::MAX
    );
    let e = within(bound, || SEG.decode(&core6, u64::MAX)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::AllocationTooLarge { .. }),
        "seg: {e}"
    );

    // Machine 2/4: the caller-supplied declared length is refused up front.
    let src = lcg(0x00A0_7001, 300);
    let c2 = ekbloom::RANS0.encode(&src).unwrap();
    let e = within(bound, || ekbloom::RANS0.decode(&c2, u64::MAX)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::AllocationTooLarge { .. }),
        "m2: {e}"
    );
    let c4 = ekbloom::encode_with_k(&src, 3).unwrap();
    let e = within(bound, || ekbloom::RANS0I.decode(&c4, u64::MAX)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::AllocationTooLarge { .. }),
        "m4: {e}"
    );

    // The policy mechanism: the ceiling is inclusive, one past is refused, and
    // the default remains the documented 1 GiB.
    assert_eq!(DEFAULT_MAX_SINGLE_ALLOCATION, 1 << 30);
    let policy = ResourcePolicy::default_policy();
    assert!(ekbloom::checked_capacity(DEFAULT_MAX_SINGLE_ALLOCATION, policy).is_ok());
    assert!(matches!(
        ekbloom::checked_capacity(one_past, policy),
        Err(EkbloomError::AllocationTooLarge { .. })
    ));

    // A legitimate bounded decode inside the cap still works (no over-refusal).
    assert_eq!(
        PACK.decode(&PACK.encode(&src).unwrap(), src.len() as u64)
            .unwrap(),
        src
    );
}

// ---------------------------------------------------------------------------
// 3. Dictionary substitution.
// ---------------------------------------------------------------------------

fn train(texts: &[&[u8]]) -> Dictionary {
    let dict = train_dictionary(texts).unwrap();
    let _ = ekbloom::register_dictionary(&dict);
    dict
}

#[test]
fn dictionary_substitution_is_a_typed_error() {
    let bound = Duration::from_secs(5);
    let a = train(&[
        b"user=alice|region=us-east-1|kind=alpha",
        b"user=bob|region=eu-west-1|kind=beta",
    ]);
    let b = train(&[
        b"temperature=21.5;humidity=40",
        b"temperature=19.0;humidity=55",
    ]);
    assert_ne!(a.id(), b.id(), "distinct dictionaries");

    let record = b"user=carol|region=us-west-2|kind=gamma|v=7";
    let record_core = a.encode(record).unwrap();
    let valid = frame_dictionary_reference(&a, &record_core);
    // Sanity: the honest referenced core decodes exactly.
    assert_eq!(DICT.decode(&valid, record.len() as u64).unwrap(), record);

    // (1) Forged / unregistered referenced id.
    let mut forged = valid.clone();
    forged[0] ^= 0xFF;
    let e = within(bound, || DICT.init(&forged)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::BadSection { .. }),
        "forged id: {e}"
    );

    // (2) Oversized declared `dict_len` (`u32::MAX`) over a tiny core.
    let mut oversized = valid.clone();
    oversized[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    let e = within(bound, || DICT.init(&oversized)).unwrap_err();
    assert!(
        matches!(
            e,
            EkbloomError::Truncated { .. } | EkbloomError::Overflow { .. }
        ),
        "oversized dict_len: {e}"
    );

    // (3) Embedded dictionary B relabelled as dictionary A: the recomputed
    //     content id disagrees with the frame id, so it is refused.
    let other_core = b.encode(record).unwrap();
    let mut relabelled = frame_dictionary_embedded(&b, &other_core);
    relabelled[..16].copy_from_slice(&a.id());
    let e = within(bound, || DICT.init(&relabelled)).unwrap_err();
    assert!(
        matches!(e, EkbloomError::BadSection { .. }),
        "relabelled embedded dict: {e}"
    );

    // (4) Dictionary/record mismatch inside a body: the record core is bound by
    //     the manifest `core_hash`, so a substituted core is a `CorruptGeneration`
    //     (never silently the wrong bytes).
    let dir = temp_dir("dict-mismatch");
    let src = text_like(5, 137);
    let body_a = create_dictionary(&src, ManifestationProfile::Root, &a).unwrap();
    let path = dir.join("a.ekb");
    write_body(&path, &body_a).unwrap();
    let mut img = std::fs::read(&path).unwrap();

    // A frame that references A but carries B's record, spliced over the core.
    let mismatched = frame_dictionary_reference(&a, &b.encode(&src).unwrap());
    let core_off = format::CORE_OFFSET as usize;
    let core_len = format::scan(&img).unwrap().header.core_len as usize;
    let n = mismatched.len().min(core_len);
    img[core_off..core_off + n].copy_from_slice(&mismatched[..n]);
    let e = within(bound, || reader::open_bytes(&img)).unwrap_err();
    assert!(
        matches!(
            e,
            EkbloomError::CorruptGeneration { .. }
                | EkbloomError::Corrupt { .. }
                | EkbloomError::ChecksumMismatch { .. }
                | EkbloomError::BadSection { .. }
                | EkbloomError::Truncated { .. }
        ),
        "dict/record mismatch: {e}"
    );

    // The honest body still reads back exactly (no cross-contamination).
    let opened = reader::open(&path).unwrap();
    assert_eq!(
        read(
            &opened,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. Authentication downgrade / rollback.
// ---------------------------------------------------------------------------

const KEY: [u8; 32] = [0x11; 32];
const WRONG: [u8; 32] = [0x22; 32];

fn auth_sections(bytes: &[u8]) -> usize {
    format::scan(bytes)
        .unwrap()
        .commits
        .iter()
        .map(|c| {
            c.sections
                .iter()
                .filter(|s| s.kind == SectionKind::Auth)
                .count()
        })
        .sum()
}

/// Remove the trailing `AUTH` section (the unkeyed image).
fn strip_auth(bytes: &[u8]) -> Vec<u8> {
    let scan = format::scan(bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth = commit
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Auth)
        .unwrap();
    let auth_start = auth.header_offset as usize;
    let footer_start = commit.end as usize - format::FOOTER_LEN;
    let mut out = Vec::with_capacity(bytes.len() - (footer_start - auth_start));
    out.extend_from_slice(&bytes[..auth_start]);
    out.extend_from_slice(&bytes[footer_start..]);
    out
}

#[test]
fn authentication_downgrade_and_rollback() {
    let bound = Duration::from_secs(5);
    let dir = temp_dir("auth");
    let src = lcg(0xA11CE, 1024);
    let path = dir.join("a.ekb");
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    write_body_authenticated(&path, &body, &KEY).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(auth_sections(&bytes), 1);

    // (a) Strip AUTH → strict mode rejects (no silent downgrade).
    let stripped = strip_auth(&bytes);
    assert!(
        reader::open_bytes(&stripped).is_ok(),
        "stripped image is valid unkeyed"
    );
    assert!(
        within(bound, || reader::open_bytes_require_auth(&stripped, &KEY)).is_err(),
        "stripped AUTH must be rejected in strict mode"
    );

    // (b) Flip a byte in the AUTH payload → the tag no longer verifies (here the
    //     section CRC fails, so it is treated as absent) → rejected.
    let auth = format::scan(&bytes)
        .unwrap()
        .commits
        .last()
        .unwrap()
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Auth)
        .unwrap()
        .payload_offset as usize;
    let mut flipped = bytes.clone();
    flipped[auth] ^= 0xFF;
    assert!(
        within(bound, || reader::open_bytes_require_auth(&flipped, &KEY)).is_err(),
        "a corrupted AUTH tag must be rejected"
    );

    // (c) Wrong key → rejected.
    assert!(
        within(bound, || reader::open_require_auth(&path, &WRONG)).is_err(),
        "wrong key must be rejected"
    );

    // (d) Downgrade authenticated → unkeyed: the *unkeyed* open ignores AUTH by
    //     design, but strict mode on a wholly unkeyed file is a hard rejection.
    let plain = dir.join("plain.ekb");
    write_body(&plain, &create(&src, ManifestationProfile::Root).unwrap()).unwrap();
    assert_eq!(auth_sections(&std::fs::read(&plain).unwrap()), 0);
    assert!(
        within(bound, || reader::open_require_auth(&plain, &KEY)).is_err(),
        "an unkeyed file must be rejected in strict mode"
    );

    // (e) Rollback: append a newer authenticated generation, then truncate the file
    //     back to the first generation's boundary. The older generation is a
    //     *genuine* authenticated object under the same key, so it is accepted —
    //     this is the documented residual risk (byte-level, not freshness-level).
    let mut grown = reader::open_authenticated(&path, &KEY).unwrap();
    grow(&mut grown, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    let gen2 = ekbloom::writer::append_generation_authenticated(&path, &grown, &KEY).unwrap();
    assert!(gen2 > 1);
    let bytes2 = std::fs::read(&path).unwrap();
    let gen1_end = format::scan(&bytes2).unwrap().commits[0].end as usize;
    std::fs::write(&path, &bytes2[..gen1_end]).unwrap();
    let rolled = within(bound, || reader::open_require_auth(&path, &KEY)).unwrap();
    assert!(
        rolled.generation < gen2,
        "rollback selects an older generation ({}) — the honest anti-replay limitation",
        rolled.generation
    );
    assert_eq!(
        read(
            &rolled,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src,
        "the rolled-back generation still reads its own (old) bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 5. Truncation of a backing file under an active reader / mapping.
// ---------------------------------------------------------------------------

#[test]
fn backing_file_truncation_under_an_active_reader() {
    let bound = Duration::from_secs(5);
    let dir = temp_dir("trunc");
    let src = lcg(0x7A0C_0007, 4096);
    let path = dir.join("live.ekb");
    let body = create(&src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();
    let original_len = std::fs::metadata(&path).unwrap().len();

    // Open the safe, file-backed reader, then shrink the backing file under it.
    let fb = reader::FileBody::open(&path).unwrap();
    let new_len = (original_len / 2).max(1);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(new_len)
        .unwrap();

    // Reading a logical range either fails cleanly or returns the right bytes —
    // never a panic, never a wrong byte, never an over-read.
    within(bound, || {
        match fb.read(ReadRequest {
            offset: 0,
            length: src.len() as u64,
        }) {
            Ok(got) => assert_eq!(got, src, "truncated read returned wrong bytes"),
            Err(e) => assert!(
                matches!(e, EkbloomError::Io(_) | EkbloomError::Truncated { .. }),
                "expected a typed truncation error, got {e}"
            ),
        }
    });
    // A read wholly past the shrunken end is a typed error.
    let past = within(bound, || {
        fb.read(ReadRequest {
            offset: src.len() as u64,
            length: 1,
        })
    });
    assert!(matches!(past, Err(EkbloomError::OutOfBounds { .. })));
    drop(fb);

    // A live mapping whose file shrinks: reading within the retained prefix stays
    // exact; the surface is not on the untrusted parser path, so reading past the
    // new EOF is the caller's documented responsibility (never exercised here).
    let mpath = dir.join("live.bin");
    std::fs::write(&mpath, &src).unwrap();
    let m = MappedSurface::open(&mpath).unwrap();
    let keep = (src.len() as u64 / 2).max(1);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&mpath)
        .unwrap()
        .set_len(keep)
        .unwrap();
    let got = within(bound, || m.read(0, keep)).unwrap();
    assert_eq!(
        got,
        &src[..keep as usize],
        "mapped read after shrink differs"
    );
    drop(m);

    // A file truncated to zero before opening has no surface to map.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&mpath)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(matches!(
        MappedSurface::open(&mpath),
        Err(EkbloomError::InvalidArgument { .. })
    ));

    let _ = std::fs::remove_dir_all(&dir);
}
