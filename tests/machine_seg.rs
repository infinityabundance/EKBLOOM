//! `OrANS-SEG` (`machine_id = 6`): the versioned segmented container.
//!
//! Roundtrip for every corpus category (including the constant-symbol and mixed
//! cases), byte-exact `O(1)` `byte_at`/`reveal`/`peek` at *every* position on
//! small corpora and *sampled across segment boundaries* on a larger one, partial
//! and boundary offsets, variable-size segments, forged-directory rejection (bad
//! content id, overlapping / out-of-range offsets, truncated payload, wrong
//! version, bad directory hash), `physical_bytes()` metadata accounting, and the
//! machines 1-5 non-regression.

use ekbloom::machine::prepare;
use ekbloom::{
    create_machine, encode_with_k, machine_by_id, ManifestationProfile, OrMachine, SegContext,
    Snapshot, MACHINE_NL2, MACHINE_PACK, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_SEG, MACHINE_STORE,
    PACK, RANS0, RANS0I, SEG, SEG_ENTRY_BYTES, SEG_HEADER_BYTES, SEG_KIND_CONSTANT, SEG_MAGIC,
    SEG_VERSION, STORE,
};

// Header/directory field offsets (mirroring the module's frozen layout).
const H_VERSION: usize = 4;
const H_DIRECTORY_HASH: usize = 56;
const E_LOGICAL_START: usize = 0;
const E_PHYSICAL_OFFSET: usize = 16;
const E_PHYSICAL_LEN: usize = 24;
const E_CONTENT_ID: usize = 40;

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

fn alternating(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 2) as u8).collect()
}

fn mixed(len: usize) -> Vec<u8> {
    let half = len / 2;
    let mut out = skewed(0x5E61_0000_00CC_0002, half);
    out.extend_from_slice(&lcg(0x5E61_0000_00CC_0003, len - half));
    out
}

fn categories() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one", vec![0x5A]),
        ("all_bytes", (0u8..=255).collect()),
        ("repeating", b"abcabcabcabcabcabcabcabc".to_vec()),
        ("zero_run", vec![0u8; 300]),
        ("constant", vec![0x9Cu8; 100_000]),
        ("alternating", alternating(257)),
        ("skewed4", skewed(0x5E61_0000_0001, 4096)),
        ("random", lcg(0x5E61_0000_0003, 2048)),
        ("mixed", mixed(32_768)),
    ]
}

/// Decode a whole container through the prepared context.
fn decode_via_ctx(core: &[u8]) -> Vec<u8> {
    let ctx = SegContext::from_core(core).unwrap();
    let mut out = Vec::new();
    let mut omega = Snapshot::new(0, 0);
    ctx.decode_into(core, &mut omega, ctx.logical_len(), &mut out)
        .unwrap();
    out
}

/// Recompute and patch the stored directory hash so a forged payload/entry reaches
/// the entry-level checks rather than the hash check.
fn repatch_hash(core: &mut [u8]) {
    let h = *blake3::hash(&core[SEG_HEADER_BYTES..]).as_bytes();
    core[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32].copy_from_slice(&h);
}

fn entry_off(i: usize) -> usize {
    SEG_HEADER_BYTES + i * SEG_ENTRY_BYTES
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

#[test]
fn roundtrip_every_category() {
    for (name, src) in categories() {
        let core = SEG.encode(&src).unwrap();
        assert_eq!(
            core[..4],
            SEG_MAGIC,
            "{name}: container must carry the segment magic"
        );
        assert_eq!(decode_via_ctx(&core), src, "{name}: prepared roundtrip");
        // The OrMachine path (re-parses per step) agrees on small inputs.
        if src.len() <= 4096 {
            assert_eq!(
                SEG.decode(&core, src.len() as u64).unwrap(),
                src,
                "{name}: machine roundtrip"
            );
        }
        assert!(SEG.check_core(&core, src.len() as u64).is_ok());
    }
}

#[test]
fn constant_source_uses_one_byte_per_segment() {
    let src = vec![0x42u8; 20_000];
    let core = SEG.encode(&src).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    assert!(ctx.segment_count() >= 1);
    for e in ctx.entries() {
        assert_eq!(e.machine_id, SEG_KIND_CONSTANT, "constant segment");
        assert_eq!(e.physical_len, 1, "constant payload is one byte");
    }
    assert_eq!(decode_via_ctx(&core), src);
    assert_eq!(ctx.byte_at(&core, 0).unwrap(), 0x42);
    assert_eq!(ctx.byte_at(&core, 19_999).unwrap(), 0x42);
}

#[test]
fn mixed_corpus_is_packed_then_raw() {
    // Exactly half skewed, half uniform, aligned to the 4096-byte fixed segments.
    let src = mixed(1 << 20); // 1 MiB
    let core = SEG.encode(&src).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    let half_seg = (src.len() / 2) / 4096;
    for i in 0..half_seg {
        assert_eq!(
            ctx.entries()[i].machine_id,
            MACHINE_PACK,
            "skewed segment {i} must be packed"
        );
    }
    for i in half_seg..ctx.segment_count() {
        assert_eq!(
            ctx.entries()[i].machine_id,
            MACHINE_STORE,
            "uniform segment {i} must be raw"
        );
    }
    assert_eq!(decode_via_ctx(&core), src);
    // Segmented beats the whole-object packed-only core (2-bit on the skewed half
    // but 8-bit identity on the uniform half) and raw.
    let packed_only = PACK.encode(&src).unwrap();
    assert!(
        core.len() < packed_only.len(),
        "segmented {} < packed-only {}",
        core.len(),
        packed_only.len()
    );
    assert!(core.len() < src.len());
}

#[test]
fn byte_at_and_reveal_are_exact_at_every_position() {
    for (name, src) in categories() {
        if src.len() > 8192 {
            continue; // small categories only; the sampled test covers the rest
        }
        let core = SEG.encode(&src).unwrap();
        let ctx = SegContext::from_core(&core).unwrap();
        let prepared = prepare(&SEG, &core).unwrap();
        let mut omega = SEG.init(&core).unwrap();
        assert_eq!(omega, Snapshot::new(0, 0), "{name} genesis");
        for (i, &expect) in src.iter().enumerate() {
            let i = i as u64;
            assert_eq!(ctx.byte_at(&core, i).unwrap(), expect, "{name} byte_at {i}");
            assert_eq!(SEG.peek(&core, &omega).unwrap(), expect, "{name} peek {i}");
            assert_eq!(
                prepared.peek(&core, &omega).unwrap(),
                expect,
                "{name} prep {i}"
            );
            assert_eq!(
                prepared.reveal(&core, &mut omega).unwrap(),
                expect,
                "{name} reveal {i}"
            );
            assert_eq!(omega.cursor, i + 1, "{name} cursor at {i}");
        }
        assert!(
            ctx.byte_at(&core, src.len() as u64).is_err(),
            "{name} past end"
        );
    }
}

#[test]
fn sampled_across_segment_boundaries_on_a_larger_corpus() {
    let src = mixed(1 << 20); // 1 MiB, boundary at 512 KiB = segment 128
    let core = SEG.encode(&src).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    let prepared = prepare(&SEG, &core).unwrap();
    let n = src.len() as u64;
    let seg = 4096u64;
    let mut positions: Vec<u64> = Vec::new();
    let mut b = 0u64;
    while b < n {
        positions.push(b.saturating_sub(1));
        positions.push(b);
        positions.push(b + 1);
        positions.push((b + seg - 1).min(n - 1));
        b += seg;
    }
    positions.push(n - 1);
    for &i in &positions {
        if i >= n {
            continue;
        }
        assert_eq!(
            ctx.byte_at(&core, i).unwrap(),
            src[i as usize],
            "byte_at {i}"
        );
        let omega = Snapshot::new(0, i);
        assert_eq!(
            prepared.peek(&core, &omega).unwrap(),
            src[i as usize],
            "peek {i}"
        );
    }
}

#[test]
fn range_reads_cross_boundaries_and_partial_ends() {
    let src = mixed(1 << 18); // 256 KiB
    let core = SEG.encode(&src).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    let n = src.len() as u64;
    // A range that starts just before a boundary and spans into the next segment.
    for &(start, len) in &[
        (4090u64, 12u64),
        (4096, 1),
        (4095, 4096),
        (0, 1),
        (n - 5, 5),
        (n - 1, 1),
        (n / 2 - 3, 9),
    ] {
        let mut out = Vec::new();
        let mut omega = Snapshot::new(0, start);
        ctx.decode_into(&core, &mut omega, len, &mut out).unwrap();
        assert_eq!(out, &src[start as usize..(start + len) as usize]);
    }
    // One past the end is a clean error.
    let mut out = Vec::new();
    let mut omega = Snapshot::new(0, n);
    assert!(ctx.decode_into(&core, &mut omega, 1, &mut out).is_err());
    // skip is O(1) and exact.
    let mut omega = Snapshot::new(0, 0);
    ctx.skip(&core, &mut omega, n).unwrap();
    assert_eq!(omega.cursor, n);
    assert!(ctx.skip(&core, &mut omega, 1).is_err());
}

#[test]
fn variable_size_segments_roundtrip_and_are_exact() {
    let src = mixed(200_000);
    let boundaries = [1u64, 7, 100, 4096, 65_536, 3, 50_000, 80_257];
    let sum: u64 = boundaries.iter().sum();
    assert_eq!(sum, src.len() as u64);
    let core = SEG.encode_variable(&src, &boundaries).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    assert!(ctx.variable(), "container must use the two-level directory");
    assert_eq!(decode_via_ctx(&core), src);
    // Every position is exact.
    let prepared = prepare(&SEG, &core).unwrap();
    for (i, &expect) in src.iter().enumerate() {
        let i = i as u64;
        assert_eq!(ctx.byte_at(&core, i).unwrap(), expect, "byte_at {i}");
        let omega = Snapshot::new(0, i);
        assert_eq!(prepared.peek(&core, &omega).unwrap(), expect, "peek {i}");
    }
    // A boundary sum that does not match the source is a clean error.
    assert!(SEG.encode_variable(&src, &[1, 2]).is_err());
    assert!(SEG.encode_variable(&src, &[0]).is_err());
}

#[test]
fn empty_container_is_valid_and_reads_fail_cleanly() {
    let core = SEG.encode(&[]).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    assert_eq!(ctx.logical_len(), 0);
    assert_eq!(ctx.segment_count(), 0);
    assert_eq!(core.len(), SEG_HEADER_BYTES);
    assert!(ctx.byte_at(&core, 0).is_err());
    assert_eq!(SEG.decode(&core, 0).unwrap(), Vec::<u8>::new());
}

#[test]
fn forged_container_is_rejected_cleanly() {
    let src = skewed(0x5E61_0000_4001, 32_768);
    let good = SEG.encode(&src).unwrap();
    assert!(SegContext::from_core(&good).is_ok());

    // Wrong version.
    let mut bad = good.clone();
    bad[H_VERSION..H_VERSION + 2].copy_from_slice(&(SEG_VERSION + 1).to_le_bytes());
    assert!(SegContext::from_core(&bad).is_err(), "wrong version");

    // Wrong magic.
    let mut bad = good.clone();
    bad[0] = b'X';
    assert!(SegContext::from_core(&bad).is_err(), "wrong magic");

    // Truncated payload.
    let mut bad = good.clone();
    bad.pop();
    assert!(SegContext::from_core(&bad).is_err(), "truncated payload");

    // Truncated to below the header.
    assert!(SegContext::from_core(&good[..SEG_HEADER_BYTES - 1]).is_err());

    // Directory-hash mismatch: flip a payload byte without re-patching the hash.
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert!(
        SegContext::from_core(&bad).is_err(),
        "directory hash must cover the payloads"
    );

    // Content-id mismatch: flip a payload byte AND re-patch the directory hash so
    // the entry-level content-id check is what fires.
    let mut bad = good.clone();
    bad[last] ^= 0xFF;
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "content id must match the payload"
    );

    // Overlapping physical offsets (segment 1 points at segment 0's payload).
    let mut bad = good.clone();
    let zero_off =
        bad[entry_off(0) + E_PHYSICAL_OFFSET..entry_off(0) + E_PHYSICAL_OFFSET + 8].to_vec();
    bad[entry_off(1) + E_PHYSICAL_OFFSET..entry_off(1) + E_PHYSICAL_OFFSET + 8]
        .copy_from_slice(&zero_off);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "overlapping physical offsets"
    );

    // Out-of-range physical offset.
    let mut bad = good.clone();
    put_u64(&mut bad, entry_off(1) + E_PHYSICAL_OFFSET, u64::MAX / 2);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "out-of-range physical offset"
    );

    // Over-long physical length (runs past the payload region).
    let mut bad = good.clone();
    put_u64(&mut bad, entry_off(1) + E_PHYSICAL_LEN, u64::MAX / 2);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "out-of-range physical length"
    );

    // Non-contiguous logical layout (segment 1 start shifted forward).
    let mut bad = good.clone();
    let start1 = get_u64(&bad, entry_off(1) + E_LOGICAL_START);
    put_u64(&mut bad, entry_off(1) + E_LOGICAL_START, start1 + 7);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "non-contiguous logical layout"
    );

    // Content-id field tampered (all zeroes) with a matching directory hash.
    let mut bad = good.clone();
    bad[entry_off(0) + E_CONTENT_ID..entry_off(0) + E_CONTENT_ID + 16].fill(0);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "tampered content-id field"
    );
}

#[test]
fn physical_bytes_includes_all_metadata_exactly() {
    let src = mixed(131_072);
    let body = create_machine(&src, ManifestationProfile::Root, MACHINE_SEG).unwrap();
    let core = &body.core;
    let ctx = SegContext::from_core(core).unwrap();
    // The container is exactly header + directory + payloads, and `physical_bytes`
    // of a Root body is exactly the container length.
    assert_eq!(
        SEG_HEADER_BYTES as u64 + ctx.directory_bytes() + ctx.payload_bytes(),
        core.len() as u64
    );
    assert_eq!(body.physical_bytes(), core.len() as u64);
    assert_eq!(body.visible_bytes(), core.len() as u64);
    assert_eq!(body.logical_len, src.len() as u64);
    // Sampled logical symbols are available from the body (the body's snapshot
    // replay is O(i) per call, so sample rather than scan every position).
    let mut positions: Vec<u64> = (0..src.len() as u64).step_by(997).collect();
    positions.push(src.len() as u64 - 1);
    for &i in &positions {
        assert_eq!(body.symbol_at(i).unwrap(), src[i as usize], "symbol {i}");
    }
}

#[test]
fn prepare_view_matches_direct_context() {
    let src = mixed(65_536);
    let core = SEG.encode(&src).unwrap();
    let prepared = prepare(&SEG, &core).unwrap();
    let mut out = Vec::new();
    let mut omega = SEG.init(&core).unwrap();
    prepared
        .decode_into(&core, &mut omega, src.len() as u64, &mut out)
        .unwrap();
    assert_eq!(out, src);
    assert_eq!(prepared.min_cursor(), 0);
    assert!(!prepared.is_windowed());
    assert!(prepared.validate_restart(&omega).is_ok());
}

#[test]
fn machines_one_through_five_are_unaffected() {
    assert_eq!(MACHINE_NL2, 1);
    assert_eq!(MACHINE_RANS0, 2);
    assert_eq!(MACHINE_STORE, 3);
    assert_eq!(MACHINE_RANS0I, 4);
    assert_eq!(MACHINE_PACK, 5);
    assert_eq!(MACHINE_SEG, 6);

    let src = skewed(0x5E61_0000_5001, 4096);

    let core = machine_by_id(MACHINE_NL2).unwrap().encode(&src).unwrap();
    assert_eq!(
        machine_by_id(MACHINE_NL2)
            .unwrap()
            .decode(&core, src.len() as u64)
            .unwrap(),
        src
    );
    let core = RANS0.encode(&src).unwrap();
    assert_eq!(RANS0.decode(&core, src.len() as u64).unwrap(), src);
    let core = STORE.encode(&src).unwrap();
    assert_eq!(core, src);
    assert_eq!(STORE.decode(&core, src.len() as u64).unwrap(), src);
    let core = encode_with_k(&src, 4).unwrap();
    assert_eq!(RANS0I.decode(&core, src.len() as u64).unwrap(), src);
    let core = PACK.encode(&src).unwrap();
    assert_eq!(PACK.decode(&core, src.len() as u64).unwrap(), src);
}
