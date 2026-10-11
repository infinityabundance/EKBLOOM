//! Independent adversarial hardening tests for the 0.3 additions:
//! `history_store` segment/index parsing, the `HISTORY_SEGMENTED` `.ekb`
//! section, `Bloom4Resume` unpacking, budget transactional integrity, the
//! `estimate_physical_bytes` upper-bound claim, the O7 additive-shell claim, and
//! O8 cross-shell byte-exactness.
//!
//! Every test either asserts a clean `Err` (never a panic/OOB/attacker-sized
//! allocation) for hostile bytes, or byte-for-byte equality for valid data.
//! Malformed-but-checksum-valid `.ekb` images are produced by patching the real
//! writer's output and recomputing the section CRC-32 in test code (own IEEE
//! implementation; the crate's dependencies are not visible to integration
//! tests).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ekbloom::sweep::SweepConfig;
use ekbloom::{
    adapt_to_workload, append_generation, create, create_within_budget, estimate_physical_bytes,
    manifest, manifest_within_budget, open, read, read_shell, verify, verify_deep, write_body,
    write_history_segmented, Bloom4Resume, CheckpointSet, EkbloomBody, EkbloomError, History,
    HistoryStore, ManifestationBudget, ManifestationProfile, OrMachine, ReadRequest, ShellChoice,
    StoreConfig, Workload, WorkloadKind, NL2,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ekbloom-seghard-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn join(&self, name: impl AsRef<std::path::Path>) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

fn real_core(src: &[u8]) -> Vec<u8> {
    NL2.encode(src).expect("encode source")
}

/// CRC-32 (IEEE 802.3, zlib polynomial) — matches `crc32fast::hash`, which the
/// writer uses for section checksums. Implemented locally so the test does not
/// depend on a crate-internal dependency.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn align8(x: u64) -> u64 {
    (x + 7) & !7
}

const GENESIS_LEN: u64 = 80;
const SECTION_HEADER_LEN: usize = 40;
const HS_FIXED_LEN: usize = 104;

/// `(section_header_offset, payload_offset, payload_len)` of the first section of
/// a file written by [`write_history_segmented`] (which emits `HISTORY_SEGMENTED`
/// first, at `align8(80 + core_len)`).
fn segmented_layout(bytes: &[u8], core_len: u64) -> (usize, usize, usize) {
    let base = align8(GENESIS_LEN + core_len) as usize;
    let kind = u32::from_le_bytes(bytes[base..base + 4].try_into().unwrap());
    assert_eq!(kind, 11, "first section is not history-segmented");
    let payload_off = u64::from_le_bytes(bytes[base + 8..base + 16].try_into().unwrap()) as usize;
    let payload_len = u64::from_le_bytes(bytes[base + 16..base + 24].try_into().unwrap()) as usize;
    assert_eq!(payload_off, base + SECTION_HEADER_LEN);
    (base, payload_off, payload_len)
}

/// Recompute the section CRC-32 after in-place payload mutation.
fn refix_crc(bytes: &mut [u8], base: usize, payload_off: usize, payload_len: usize) {
    let crc = crc32(&bytes[payload_off..payload_off + payload_len]);
    bytes[base + 32..base + 36].copy_from_slice(&crc.to_le_bytes());
}

fn put_u32(payload: &mut [u8], off: usize, v: u32) {
    payload[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(payload: &mut [u8], off: usize, v: u64) {
    payload[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u64(payload: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(payload[off..off + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// 1. `history_store`: corrupt / truncated segment files and out-of-range indices.
// ---------------------------------------------------------------------------

#[test]
fn history_store_corrupt_segments_error_cleanly() {
    let src = lcg(0x5150_0001, 300);
    let core = real_core(&src);
    let dir = TempDir::new("hs-corrupt");
    let seg_dir = dir.join("segs");
    let store = HistoryStore::build(
        &seg_dir,
        |p| core[p as usize],
        src.len() as u64,
        StoreConfig { segment_bytes: 64 }, // sps = 16
        4,
    )
    .unwrap();
    let n = store.logical_len();
    let sps = store.states_per_segment();
    assert_eq!(sps, 16);

    // Baseline totality.
    for i in 0..=n {
        assert!(store.state_at(i).is_ok());
    }
    assert!(store.verify_sample(|p| core[p as usize], 7).is_ok());
    assert!(store.verify_sample(|p| core[p as usize], 0).is_err());

    // Out-of-range logical positions are clean errors (never panics).
    for bad in [n + 1, n + 1000, u64::MAX] {
        assert!(store.state_at(bad).is_err(), "state_at({bad})");
    }
    assert!(store.segment_len_bytes(u64::MAX).is_err());
    assert!(store
        .segment_len_bytes(store.segment_count() as u64)
        .is_err());
    assert!(store.read_segment(store.segment_count()).is_err());

    // Truncate the final segment by one state: the last position of that segment
    // must now be a clean `Truncated`, while earlier positions remain readable.
    let last_seg = store.segment_count() - 1;
    let seg_path = seg_dir.join(format!("seg-{last_seg:08}.bin"));
    let data = std::fs::read(&seg_path).unwrap();
    std::fs::write(&seg_path, &data[..data.len() - 4]).unwrap();
    let last_pos = n; // lives in the final (ragged) segment
    assert!(
        store.state_at(last_pos).is_err(),
        "truncated final segment must error"
    );
    assert!(store.state_at(0).is_ok(), "earlier segment still readable");

    // Zero-length segment: clean error, never a panic.
    std::fs::write(&seg_path, b"").unwrap();
    assert!(store.state_at(last_pos).is_err());

    // A missing referenced segment file is a clean error, and only for the
    // positions that map into it.
    std::fs::remove_file(seg_dir.join("seg-00000000.bin")).unwrap();
    assert!(store.state_at(0).is_err());
    assert!(store.state_at(sps).is_ok(), "segment 1 is independent");

    // A state byte flipped in place: `state_at` still returns *something* (no
    // panic/OOB) but `verify_at` re-derives and catches the mismatch.
    let seg1 = seg_dir.join("seg-00000001.bin");
    let mut data = std::fs::read(&seg1).unwrap();
    data[0] ^= 0xFF;
    std::fs::write(&seg1, &data).unwrap();
    assert!(store.state_at(sps).is_ok(), "in-range read never panics");
    assert!(
        store.verify_at(|p| core[p as usize], sps).is_err(),
        "verify_at must reject a corrupted stored state"
    );
}

// ---------------------------------------------------------------------------
// 2. `HISTORY_SEGMENTED`: hostile header fields are rejected at open.
// ---------------------------------------------------------------------------

#[test]
fn segmented_header_corruption_rejected_at_open() {
    let src = lcg(0x5150_0002, 64);
    let core = real_core(&src);
    let dir = TempDir::new("seg-header");
    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        src.len() as u64,
        StoreConfig { segment_bytes: 64 },
        16,
    )
    .unwrap();
    let path = dir.join("h.ekb");
    write_history_segmented(&path, &core, &store).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let (base, poff, plen) = segmented_layout(&bytes, core.len() as u64);

    // Canary: the untouched file opens and every state is exact.
    let good = ekbloom::FileBody::open(&path).unwrap();
    for i in 0..=store.logical_len() {
        assert_eq!(
            good.segmented_state_at(i).unwrap(),
            store.state_at(i).unwrap()
        );
    }

    let cases: &[(&str, usize, u64)] = &[
        ("segment_bytes=0", 0, 0),
        ("segment_bytes=5 (not *4)", 0, 5),
        ("segment_bytes huge", 0, 0xFFFF_FFFDu32 as u64),
        ("interval invalid", 4, 7),
        ("interval zero", 4, 0),
        ("logical_len=MAX", 8, u64::MAX),
        ("state_count=0", 16, 0),
        ("state_count=MAX", 16, u64::MAX),
        ("segment_count=0", 24, 0),
        ("segment_count=MAX", 24, u64::MAX),
        ("index_offset=0", 32, 0),
        ("index_offset huge", 32, u64::MAX),
        ("states_offset=0", 40, 0),
        ("states_offset huge", 40, u64::MAX),
        ("checkpoints_offset=0", 48, 0),
        ("checkpoints_len=MAX", 56, u64::MAX),
    ];
    for (label, off, val) in cases {
        let mut b = bytes.clone();
        {
            let p = &mut b[poff..poff + plen];
            if *off == 0 && *val <= u32::MAX as u64 {
                put_u32(p, *off, *val as u32);
            } else {
                put_u64(p, *off, *val);
            }
        }
        refix_crc(&mut b, base, poff, plen);
        let r = ekbloom::reader::open_bytes(&b);
        assert!(r.is_err(), "{label} must be rejected, got Ok");
        // The streaming file-backed parser must reject it too (or at least not panic).
        let p2 = dir.join(format!(
            "m-{}.ekb",
            label.replace(['=', ' ', '(', ')'], "_")
        ));
        std::fs::write(&p2, &b).unwrap();
        let _ = ekbloom::FileBody::open(&p2);
    }

    // Forged core_hash in the segmented header: rejected at open (linkage), even
    // though the section CRC is valid.
    let mut b = bytes.clone();
    b[poff + 64] ^= 0xFF;
    refix_crc(&mut b, base, poff, plen);
    assert!(ekbloom::reader::open_bytes(&b).is_err());
}

// ---------------------------------------------------------------------------
// 3. `HISTORY_SEGMENTED`: hostile index entries never panic, never read OOB.
// ---------------------------------------------------------------------------

#[test]
fn segmented_corrupt_index_entries_error_cleanly() {
    let src = lcg(0x5150_0003, 64);
    let core = real_core(&src);
    let dir = TempDir::new("seg-index");
    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        src.len() as u64,
        StoreConfig { segment_bytes: 64 },
        16,
    )
    .unwrap();
    let path = dir.join("i.ekb");
    write_history_segmented(&path, &core, &store).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let (base, poff, plen) = segmented_layout(&bytes, core.len() as u64);

    let states_offset = get_u64(&bytes[poff..poff + plen], 40);
    let checkpoints_offset = get_u64(&bytes[poff..poff + plen], 48);
    let segment_count = get_u64(&bytes[poff..poff + plen], 24);
    assert_eq!(segment_count, 5);

    // Positive control: relocating index[0] to the state at byte offset +4 makes
    // `state_at(0)` return `x_1` (proving the in-range path and that the hostile
    // values below are genuinely out of range).
    {
        let mut b = bytes.clone();
        {
            let p = &mut b[poff..poff + plen];
            put_u64(p, HS_FIXED_LEN, states_offset + 4);
        }
        refix_crc(&mut b, base, poff, plen);
        let p2 = dir.join("control.ekb");
        std::fs::write(&p2, &b).unwrap();
        let fb = ekbloom::FileBody::open(&p2).unwrap();
        assert_eq!(
            fb.segmented_state_at(0).unwrap(),
            store.state_at(1).unwrap()
        );
    }

    let attacks: &[(&str, u64)] = &[
        ("below states region", 0),
        ("just below states", states_offset - 1),
        ("straddles checkpoints end", checkpoints_offset - 3),
        ("at checkpoints start", checkpoints_offset),
        ("far beyond file", 1 << 40),
        ("u64::MAX", u64::MAX),
        ("u64::MAX-3 (overflow)", u64::MAX - 3),
    ];
    for (label, rel) in attacks {
        let mut b = bytes.clone();
        {
            let p = &mut b[poff..poff + plen];
            put_u64(p, HS_FIXED_LEN, *rel); // index entry 0
        }
        refix_crc(&mut b, base, poff, plen);
        let p2 = dir.join(format!("idx-{}.ekb", label.replace([' ', ':'], "_")));
        std::fs::write(&p2, &b).unwrap();
        let fb = ekbloom::FileBody::open(&p2).expect("header is untouched; open must succeed");
        assert!(
            fb.segmented_state_at(0).is_err(),
            "{label}: out-of-range index must error"
        );
        // Segments whose index entry was not touched are still readable.
        assert!(
            fb.segmented_state_at(16).is_ok(),
            "{label}: unaffected segment must stay readable"
        );
    }

    // The final segment's index entry is equally bounds-checked.
    {
        let mut b = bytes.clone();
        let mut p = b[poff..poff + plen].to_vec();
        put_u64(
            &mut p,
            HS_FIXED_LEN + 8 * (segment_count as usize - 1),
            u64::MAX,
        );
        b[poff..poff + plen].copy_from_slice(&p);
        refix_crc(&mut b, base, poff, plen);
        let p2 = dir.join("last-index.ekb");
        std::fs::write(&p2, &b).unwrap();
        let fb = ekbloom::FileBody::open(&p2).unwrap();
        assert!(fb.segmented_state_at(store.logical_len()).is_err());
        assert!(fb.segmented_state_at(store.logical_len() + 1).is_err());
    }
}

#[test]
fn segmented_truncated_file_errors_cleanly() {
    let src = lcg(0x5150_0004, 96);
    let core = real_core(&src);
    let dir = TempDir::new("seg-trunc");
    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        src.len() as u64,
        StoreConfig { segment_bytes: 64 },
        16,
    )
    .unwrap();
    let path = dir.join("t.ekb");
    write_history_segmented(&path, &core, &store).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    // Every truncation, including cuts inside the segmented payload, is a clean
    // error (open or read) — never a panic/OOB.
    for cut in 0..bytes.len() {
        let b = &bytes[..cut];
        if ekbloom::reader::open_bytes(b).is_ok() {
            // A truncation that still parses must not permit an OOB state read.
            let p = dir.join(format!("cut-{cut}.ekb"));
            std::fs::write(&p, b).unwrap();
            if let Ok(fb) = ekbloom::FileBody::open(&p) {
                for i in [0u64, 1, 16, store.logical_len(), store.logical_len() + 1] {
                    let _ = fb.segmented_state_at(i);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. `Bloom4Resume` hostile and short payloads.
// ---------------------------------------------------------------------------

#[test]
fn bloom4_resume_unpack_is_total() {
    for n in 0..Bloom4Resume::PACKED_LEN {
        assert!(Bloom4Resume::unpack(&vec![0xAAu8; n]).is_err(), "len {n}");
    }
    // Every 16-byte payload is accepted and round-trips bit-exactly; the `u64`
    // cursor is preserved (no narrowing).
    for seed in [0u64, 1, 0xDEAD_BEEF_1234_5678, u64::MAX] {
        let mut buf = [0u8; Bloom4Resume::PACKED_LEN];
        buf[..8].copy_from_slice(&seed.to_le_bytes());
        buf[8..].copy_from_slice(&seed.rotate_left(17).to_le_bytes());
        let e = Bloom4Resume::unpack(&buf).unwrap();
        assert_eq!(e.pack(), buf);
        assert_eq!(e.cursor, u64::from_le_bytes(buf[8..16].try_into().unwrap()));
    }
    // Bytes beyond the first 16 are ignored (matching `unpack_block` style).
    let mut long = vec![0u8; 64];
    long[..16].copy_from_slice(&Bloom4Resume::new([1, 2, 3, 4], 7, 9).pack());
    assert_eq!(
        Bloom4Resume::unpack(&long).unwrap(),
        Bloom4Resume::new([1, 2, 3, 4], 7, 9)
    );
}

// ---------------------------------------------------------------------------
// 5. Budget transactional integrity (independent byte-for-byte assertion).
// ---------------------------------------------------------------------------

/// A strong canonical serialization of every byte-carrying field of a body, used
/// to assert that a rejected budget call leaves it *bit-for-bit* unchanged.
fn canonical_bytes(body: &EkbloomBody) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&body.generation.to_le_bytes());
    out.extend_from_slice(&body.logical_len.to_le_bytes());
    out.extend_from_slice(&body.physical_bytes().to_le_bytes());
    out.extend_from_slice(&(body.core.len() as u64).to_le_bytes());
    out.extend_from_slice(&body.core);
    out.extend_from_slice(&body.core_hash);
    out.extend_from_slice(&body.object_id);
    out.extend_from_slice(&body.machine_id.to_le_bytes());
    match &body.history {
        Some(h) => {
            out.push(1);
            out.extend_from_slice(&h.to_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&(body.checkpoints.len() as u64).to_le_bytes());
    for cp in &body.checkpoints {
        out.extend_from_slice(&cp.to_bytes());
    }
    out.extend_from_slice(&(body.blooms.len() as u64).to_le_bytes());
    for b in &body.blooms {
        out.extend_from_slice(&b.to_bytes());
    }
    out.extend_from_slice(&(body.certificates.len() as u64).to_le_bytes());
    for c in &body.certificates {
        out.extend_from_slice(&c.to_bytes());
    }
    for p in &body.profiles {
        out.extend_from_slice(p.as_bytes());
        out.push(0);
    }
    out
}

fn budget(bytes: u64) -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: bytes,
    }
}

#[test]
fn budget_rejection_is_byte_for_byte_transactional() {
    let src = lcg(0xB0D6_E700, 512);
    let starts = [
        ManifestationProfile::Root,
        ManifestationProfile::Sparse { interval: 64 },
        ManifestationProfile::Bloom { depth: 4 },
        ManifestationProfile::Full,
    ];
    let attempts = [
        ManifestationProfile::Full,
        ManifestationProfile::MaxSpeed,
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Bloom { depth: 32 },
        ManifestationProfile::Dense { interval: 16 },
        ManifestationProfile::Bloom { depth: 3 }, // unsupported depth -> build error
    ];

    for &start in &starts {
        let base = create(&src, start).unwrap();
        let base_bytes = canonical_bytes(&base);
        let base_phys = base.physical_bytes();

        // A range of budgets, including exactly-at, one-below, and wildly small.
        for delta in [0i64, -1, -8, -64, 1, 64, 4096] {
            let limit = (base_phys as i64 + delta).max(0) as u64;
            for &attempt in &attempts {
                let mut body = base.clone();
                let before = body.clone();
                let before_bytes = canonical_bytes(&body);
                let res = manifest_within_budget(&mut body, attempt, &budget(limit));
                match res {
                    Ok(()) => {
                        assert!(body.physical_bytes() <= limit);
                        assert_eq!(body.generation, before.generation + 1);
                    }
                    Err(_) => {
                        // Rejected or failed: the body must be bit-for-bit the same.
                        assert_eq!(
                            canonical_bytes(&body),
                            before_bytes,
                            "rejected {attempt:?} at limit {limit} mutated the body"
                        );
                        assert_eq!(body.physical_bytes(), base_phys);
                        assert_eq!(body.generation, before.generation);
                        assert_eq!(body, before);
                    }
                }
            }
        }
        // Sanity: the untouched clone still serializes to the original bytes.
        assert_eq!(canonical_bytes(&base), base_bytes);
    }
}

#[test]
fn create_within_budget_rejection_yields_no_body() {
    let src = lcg(0xB0D6_E701, 300);
    for profile in [
        ManifestationProfile::Root,
        ManifestationProfile::Full,
        ManifestationProfile::MaxSpeed,
        ManifestationProfile::Path { depth: 4 },
        ManifestationProfile::Bloom { depth: 4 },
    ] {
        let actual = create(&src, profile).unwrap().physical_bytes();
        // One byte below the true cost must be refused.
        let res = create_within_budget(&src, profile, &budget(actual - 1));
        match res {
            Err(EkbloomError::AllocationTooLarge { requested, limit }) => {
                assert!(requested > limit);
            }
            other => panic!("expected AllocationTooLarge for {profile:?}, got {other:?}"),
        }
        // At the exact cost it is admitted and within budget.
        let ok = create_within_budget(&src, profile, &budget(actual)).unwrap();
        assert!(ok.physical_bytes() <= actual);
    }
}

// ---------------------------------------------------------------------------
// 6. `estimate_physical_bytes` is a hard upper bound (counterexample search).
// ---------------------------------------------------------------------------

#[test]
fn estimate_never_underestimates_actual_exhaustively() {
    let lengths = [
        0usize, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255,
        256, 257, 511, 512, 513, 1023, 1024, 1025, 4095, 4096, 4097,
    ];
    let mut profiles: Vec<ManifestationProfile> = Vec::new();
    for &depth in &[1u16, 2, 4, 8, 16, 32] {
        profiles.push(ManifestationProfile::Bloom { depth });
        profiles.push(ManifestationProfile::Path { depth });
    }
    for &interval in &[1u32, 4, 16, 64, 256, 1024, 4096, 65536] {
        profiles.push(ManifestationProfile::Sparse { interval });
        profiles.push(ManifestationProfile::Dense { interval });
    }
    for &region in &[1u32, 3, 4, 5, 16, 64, 255, 256, 257, 1024, 4096, 65536] {
        profiles.push(ManifestationProfile::Regional {
            depth: 4,
            region_bytes: region,
        });
    }
    profiles.push(ManifestationProfile::Root);
    profiles.push(ManifestationProfile::Full);
    profiles.push(ManifestationProfile::MaxSpeed);

    let mut checked = 0u64;
    for &len in &lengths {
        let src = lcg(0xE571_0000 ^ len as u64, len);
        for &profile in &profiles {
            let body = match create(&src, profile) {
                Ok(b) => b,
                Err(_) => continue, // unsupported parameter combination
            };
            let estimate = estimate_physical_bytes(len as u64, profile);
            assert!(
                estimate >= body.physical_bytes(),
                "estimate {estimate} < actual {} for {profile:?} at len {len}",
                body.physical_bytes()
            );
            checked += 1;
        }
    }
    assert!(checked > 1000, "coverage too small: {checked}");
}

// ---------------------------------------------------------------------------
// 7. O7 additive claim, verified on disk (prefix invariance).
// ---------------------------------------------------------------------------

fn adapt_sample(hot_prefix: u64, reads: usize) -> Workload {
    let mut s = 0xAD47_0000_0000_0001u64;
    let sample = (0..reads)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s % hot_prefix, 1u64)
        })
        .collect();
    Workload {
        kind: WorkloadKind::HotSet,
        hot_prefix: Some(hot_prefix),
        sample,
    }
}

#[test]
fn adapt_is_additive_on_disk_and_preserves_earlier_structures() {
    let src = lcg(0x0ADD_0000_0001, 64 * 1024);
    let dir = TempDir::new("adapt-disk");
    let path = dir.join("a.ekb");

    // A body already carrying a checkpoint and a (non-path) bloom, written as gen 3.
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    manifest(&mut body, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    write_body(&path, &body).unwrap();

    let bytes_before = std::fs::read(&path).unwrap();
    let cps_before: Vec<Vec<u8>> = body.checkpoints.iter().map(|c| c.to_bytes()).collect();
    let blooms_before: Vec<Vec<u8>> = body.blooms.iter().map(|b| b.to_bytes()).collect();
    let gen_before = body.generation;

    // Adapt: appends an exact path shell as a new generation (in memory).
    let report = adapt_to_workload(
        &mut body,
        &adapt_sample(16 * 1024, 64),
        &SweepConfig {
            iterations: 4,
            seed: 1,
        },
    )
    .unwrap();
    assert!(
        report.shell.is_some(),
        "a path shell should have been appended"
    );
    assert_eq!(body.generation, gen_before + 1);

    // Earlier structures are byte-for-byte the same in memory.
    let cps_after: Vec<Vec<u8>> = body.checkpoints.iter().map(|c| c.to_bytes()).collect();
    let blooms_after: Vec<Vec<u8>> = body.blooms.iter().map(|b| b.to_bytes()).collect();
    assert_eq!(&cps_after[..cps_before.len()], &cps_before[..]);
    assert_eq!(&blooms_after[..blooms_before.len()], &blooms_before[..]);
    assert_eq!(body.blooms.len(), blooms_before.len() + 1);

    // Append the new generation to the same file: the earlier committed bytes
    // must be an exact, unchanged prefix of the new file.
    let written = append_generation(&path, &body).unwrap();
    assert_eq!(written, body.generation);
    let bytes_after = std::fs::read(&path).unwrap();
    assert!(
        bytes_after.starts_with(&bytes_before),
        "append_generation rewrote earlier committed bytes"
    );
    assert!(bytes_after.len() > bytes_before.len());

    // The reopened body verifies and the pre-existing global bloom is retained.
    let reopened = open(&path).unwrap();
    assert_eq!(reopened.generation, body.generation);
    assert!(
        reopened
            .blooms
            .iter()
            .any(|b| b.kind == ekbloom::BloomKind::Universal),
        "pre-existing global bloom disappeared"
    );
    assert!(
        verify(&reopened).unwrap().ok,
        "reopened adapted file must verify"
    );
    assert!(verify_deep(&reopened).unwrap().ok);

    // Re-reading returns the original logical bytes.
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src
    );

    // The no-op case: a body already carrying a certified path bloom is untouched
    // bit-for-bit (no generation, no bytes added).
    let mut noop = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
    let noop_before = canonical_bytes(&noop);
    let r = adapt_to_workload(
        &mut noop,
        &adapt_sample(8 * 1024, 32),
        &SweepConfig {
            iterations: 2,
            seed: 2,
        },
    )
    .unwrap();
    assert_eq!(r.shell, None);
    assert_eq!(canonical_bytes(&noop), noop_before);
}

// ---------------------------------------------------------------------------
// 8. O8 cross-shell byte-exactness, including boundaries and final partial block.
// ---------------------------------------------------------------------------

fn all_shells_body(src: &[u8], depth: u16) -> EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    manifest(&mut body, ManifestationProfile::Sparse { interval: 16 }).unwrap();
    manifest(&mut body, ManifestationProfile::Full).unwrap();
    manifest(&mut body, ManifestationProfile::Path { depth }).unwrap();
    body
}

#[test]
fn all_shells_are_byte_exact_at_boundaries_and_partial_block() {
    for &depth in &[1u16, 2, 4, 8, 32] {
        let d = depth as usize;
        let lens = [d - 1, d, d + 1, 2 * d - 1, 2 * d, 2 * d + 1, 3 * d + 5, 100];
        for &len in &lens {
            let src = lcg(0x0C0E_0000_0000 ^ ((depth as u64) << 32) ^ len as u64, len);
            let body = all_shells_body(&src, depth);
            let n = len as u64;
            // Empty-object reads succeed identically across every shell.
            if n == 0 {
                assert_eq!(
                    read(
                        &body,
                        ReadRequest {
                            offset: 0,
                            length: 0
                        }
                    )
                    .unwrap(),
                    Vec::<u8>::new()
                );
                for shell in [
                    ShellChoice::Path,
                    ShellChoice::Checkpoint,
                    ShellChoice::History,
                    ShellChoice::Machine,
                ] {
                    assert_eq!(read_shell(&body, shell, 0, 0).unwrap(), Vec::<u8>::new());
                }
                continue;
            }
            // Range set: start, end, single bytes, and the whole thing.
            let mut ranges: Vec<(u64, u64)> =
                vec![(0, 1), (n - 1, 1), (1, n - 1), (0, n), (n / 2, n - n / 2)];
            // The final partial block: last block start to the end.
            let last_block = (n - 1) / d as u64;
            let last_start = last_block * d as u64;
            if last_start < n {
                ranges.push((last_start, n - last_start));
                // A sub-range entirely inside the final (partial) block.
                ranges.push((last_start, 1));
            }

            for &(off, length) in &ranges {
                let expect = src[off as usize..(off + length) as usize].to_vec();
                let via_read = read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length,
                    },
                )
                .unwrap();
                assert_eq!(
                    via_read, expect,
                    "read mismatch D={depth} len={len} ({off},{length})"
                );
                for shell in [
                    ShellChoice::Path,
                    ShellChoice::Checkpoint,
                    ShellChoice::History,
                    ShellChoice::Machine,
                ] {
                    let got = read_shell(&body, shell, off, length).unwrap();
                    assert_eq!(
                        got,
                        expect,
                        "{} mismatch D={depth} len={len} ({off},{length})",
                        shell.as_str()
                    );
                }
            }

            // Empty reads always succeed identically across shells.
            for shell in [
                ShellChoice::Path,
                ShellChoice::Checkpoint,
                ShellChoice::History,
                ShellChoice::Machine,
            ] {
                assert_eq!(read_shell(&body, shell, n, 0).unwrap(), Vec::<u8>::new());
                assert!(read_shell(&body, shell, n + 1, 0).is_err());
            }
        }
    }
}

#[test]
fn file_backed_reads_match_in_memory_reads() {
    let src = lcg(0x0F11_E000_0001, 4096 + 7);
    let dir = TempDir::new("fbody");
    let path = dir.join("f.ekb");
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    manifest(&mut body, ManifestationProfile::Full).unwrap();
    manifest(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();
    write_body(&path, &body).unwrap();

    let fb = ekbloom::FileBody::open(&path).unwrap();
    let n = src.len() as u64;
    let mut ranges = vec![(0u64, 1u64), (0, n), (n - 1, 1), (n - 3, 3), (1, n - 1)];
    ranges.push((n / 4, n / 4));
    for (off, length) in ranges {
        let expect = src[off as usize..(off + length) as usize].to_vec();
        assert_eq!(
            fb.read(ReadRequest {
                offset: off,
                length
            })
            .unwrap(),
            expect,
            "file-backed read mismatch at ({off},{length})"
        );
        assert_eq!(
            read(
                &body,
                ReadRequest {
                    offset: off,
                    length
                }
            )
            .unwrap(),
            expect
        );
    }

    // A segmented `.ekb` (built from a real store) also returns exact states.
    let core = real_core(&src);
    let store = HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        n,
        StoreConfig { segment_bytes: 512 },
        16,
    )
    .unwrap();
    let spath = dir.join("s.ekb");
    write_history_segmented(&spath, &core, &store).unwrap();
    let sf = ekbloom::FileBody::open(&spath).unwrap();
    for i in [0u64, 1, 15, 16, 17, n / 2, n] {
        assert_eq!(
            sf.segmented_state_at(i).unwrap(),
            store.state_at(i).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------
// Extra: `History` byte round-trip and checkpoint totality (supporting coverage).
// ---------------------------------------------------------------------------

#[test]
fn history_and_checkpoint_parsers_reject_hostile_bytes() {
    // History: truncated and count-inconsistent.
    let core = real_core(&lcg(0x5150_0005, 200));
    let hist = History::build(&core, 200, 1).unwrap();
    let hb = hist.to_bytes();
    for cut in 0..hb.len() {
        assert!(
            History::from_bytes(&hb[..cut]).is_err(),
            "history cut {cut}"
        );
    }
    // Checkpoint: count claims more than is present.
    let cp = CheckpointSet::build(&core, 200, 16).unwrap();
    let mut cb = cp.to_bytes();
    cb.truncate(cb.len() - 1);
    assert!(CheckpointSet::from_bytes(&cb).is_err());
}
