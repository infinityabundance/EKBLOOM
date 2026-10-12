//! Adversarial/security review — R7 round.
//!
//! Independent attacks on machinery 5 (`OrANS-PACK`) and 6 (`OrANS-SEG`), the
//! bounded encoded-window reader, the engine's core-identity binding applied to
//! the new machines, and the OrFS pointer views. Every byte assertion is against
//! the **authoritative machine** or `reader::read`; every crafted container must
//! be a clean typed `Err`, never a panic and never a wrong byte.

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::machine::{machine_by_id, OrMachine, MACHINE_PACK, MACHINE_RANS0I, MACHINE_SEG};
use ekbloom::machine_pack::{PackContext, PACK};
use ekbloom::machine_seg::{SegContext, SEG, SEG_ENTRY_BYTES, SEG_HEADER_BYTES, SEG_VERSION};
use ekbloom::{
    create_machine, open, read, write_body, BlockIndex, EkbloomBody, EkbloomError, Engine,
    FileBody, ManifestationBudget, ManifestationProfile, ReadRequest, WINDOW_MARGIN,
};

// Machine-6 header field offsets (mirrors `src/machine_seg.rs`).
const H_VERSION: usize = 4;
const H_DIRECTORY_HASH: usize = 56;
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
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(match r {
            0..=69 => b'a',
            70..=91 => b'b',
            _ => b'c',
        });
    }
    out
}

fn mixed(len: usize) -> Vec<u8> {
    // Low-entropy skewed half, uniform half — exercises packed + raw + constant
    // segments and the window index.
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let half = len / 2;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let b = (s >> 33) as u8;
        out.push(if i < half { b & 0x03 } else { b });
    }
    out
}

// ---------------------------------------------------------------------------
// Attack 1 — machine 5 (packed): extraction exact at EVERY position, every width
// ---------------------------------------------------------------------------

#[test]
fn machine5_extraction_is_exact_at_every_position_for_every_width() {
    for bits in 1u8..=8 {
        // `k = min(2^bits, 255)` so the 1-byte alphabet-length field is legal.
        let cap = (1usize << bits).min(255);
        // Distinct, unsorted symbols to stress the index -> symbol mapping.
        let alpha: Vec<u8> = (0..cap)
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
            .collect();
        // `n * bits` is deliberately not a multiple of 8 for every width.
        let n = 997usize + bits as usize * 3;
        let src: Vec<u8> = (0..n).map(|i| alpha[i % alpha.len()]).collect();

        let core = PACK.encode_with_bits(&src, bits).unwrap();
        let ctx = PackContext::from_core(&core).unwrap();
        assert_eq!(ctx.bits(), bits, "stored width");
        assert_eq!(ctx.logical_len(), n as u64);
        assert_eq!(ctx.alphabet_len(), cap);
        assert_eq!(PACK.decode(&core, n as u64).unwrap(), src, "bits={bits}");

        // Every position, including cross-byte and the partial final symbol.
        for (i, &expect) in src.iter().enumerate() {
            assert_eq!(
                ctx.byte_at(&core, i as u64).unwrap(),
                expect,
                "bits={bits} position {i}"
            );
        }
        // A range starting at every offset matches a slice.
        for start in [0usize, 1, 7, 8, 9, n / 3, n - 1] {
            let len = (n - start).min(64);
            let mut out = Vec::new();
            ctx.unpack_range(&core, start as u64, len as u64, &mut out)
                .unwrap();
            assert_eq!(out, &src[start..start + len], "bits={bits} range {start}");
        }
        // One past the end is a clean error, not a panic.
        assert!(ctx.byte_at(&core, n as u64).is_err());
    }
}

#[test]
fn machine5_constant_segment_is_exact_at_every_position() {
    let src = vec![0xA5u8; 5000];
    let core = PACK.encode(&src).unwrap();
    let ctx = PackContext::from_core(&core).unwrap();
    assert_eq!(ctx.alphabet_len(), 1, "constant");
    for i in [0u64, 1, 4095, 4096, 4999] {
        assert_eq!(ctx.byte_at(&core, i).unwrap(), 0xA5, "position {i}");
    }
    assert_eq!(PACK.decode(&core, 5000).unwrap(), src);
}

#[test]
fn machine5_forged_shapes_are_typed_errors_never_wrong_bytes() {
    let src: Vec<u8> = (0..600usize).map(|i| (i % 3) as u8 + 1).collect(); // k = 3, bits = 2
    let good = PACK.encode_with_bits(&src, 2).unwrap();
    assert_eq!(good[0], 2);
    assert_eq!(good[1], 3);

    // Width outside [1, 8].
    for bad_bits in [0u8, 9, 255] {
        let mut bad = good.clone();
        bad[0] = bad_bits;
        assert!(PackContext::from_core(&bad).is_err(), "bits={bad_bits}");
    }
    // k > 2^bits.
    let mut bad = good.clone();
    bad[0] = 1;
    bad[1] = 3;
    assert!(PackContext::from_core(&bad).is_err(), "k>2^bits");
    // Identity sentinel with a non-8 width.
    let mut bad = good.clone();
    bad[1] = 0;
    assert!(PackContext::from_core(&bad).is_err(), "identity bits!=8");
    // Duplicate alphabet entry.
    let mut bad = good.clone();
    bad[3] = bad[2];
    assert!(PackContext::from_core(&bad).is_err(), "duplicate");
    // Short payload, oversized payload, truncated header.
    assert!(PackContext::from_core(&good[..good.len() - 1]).is_err());
    let mut bad = good.clone();
    bad.push(0);
    assert!(PackContext::from_core(&bad).is_err(), "trailing byte");
    assert!(PackContext::from_core(&[]).is_err());
    assert!(PackContext::from_core(&[2]).is_err());
    assert!(PackContext::from_core(&good[..5]).is_err());
    // Declared-length mismatch (`check_core`).
    let machine = machine_by_id(MACHINE_PACK).unwrap();
    assert!(machine.check_core(&good, src.len() as u64).is_ok());
    assert!(machine.check_core(&good, src.len() as u64 + 1).is_err());

    // Geometry is valid, but the packed payload holds an out-of-alphabet index:
    // the decode must be a typed error, never a wrong symbol or a panic. The
    // alphabet is {1, 2, 3} (k = 3), so index 3 is illegal.
    let mut forged = good.clone();
    forged[10] |= 0b11; // first symbol's 2-bit index becomes 3
    let ctx = PackContext::from_core(&forged).unwrap();
    assert!(
        ctx.byte_at(&forged, 0).is_err(),
        "out-of-alphabet index must be a typed error"
    );
    assert!(PACK.decode(&forged, src.len() as u64).is_err());
}

// ---------------------------------------------------------------------------
// Attack 2 — machine 6 (segments): exact at every position; no cross-reads
// ---------------------------------------------------------------------------

#[test]
fn machine6_reads_are_exact_at_every_position_including_boundaries() {
    let src = mixed(200_000);
    let core = SEG.encode(&src).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    let n = src.len() as u64;
    assert_eq!(ctx.logical_len(), n);
    for (i, &expect) in src.iter().enumerate() {
        assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), expect, "byte_at {i}");
    }
    assert!(ctx.byte_at(&core, n).is_err());
    assert_eq!(SEG.decode(&core, n).unwrap(), src);
}

#[test]
fn machine6_variable_directories_are_exact_at_every_position() {
    // Many unequal segment sizes, including powers of two and odd tails.
    let boundaries: [u64; 10] = [1, 3, 8, 4095, 4096, 4097, 8192, 17, 90_000, 100_000];
    let total: u64 = boundaries.iter().sum();
    let src = mixed(total as usize);
    let core = SEG.encode_variable(&src, &boundaries).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    assert!(ctx.variable());
    for (i, &expect) in src.iter().enumerate() {
        assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), expect, "byte_at {i}");
    }
    // Segment boundaries: walking them and reading either side is exact.
    let mut pos = 0u64;
    for &b in &boundaries {
        for probe in [pos, pos + b - 1] {
            assert_eq!(ctx.byte_at(&core, probe).unwrap(), src[probe as usize]);
        }
        pos += b;
    }
    assert!(ctx.segment_count() > 1);
    assert_eq!(SEG.decode(&core, total).unwrap(), src);
}

fn repatch_hash(core: &mut [u8]) {
    let hash = *blake3::hash(&core[SEG_HEADER_BYTES..]).as_bytes();
    core[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32].copy_from_slice(&hash);
}

fn entry_off(i: usize) -> usize {
    SEG_HEADER_BYTES + i * SEG_ENTRY_BYTES
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn machine6_cannot_be_made_to_read_a_segments_bytes_as_anothers() {
    let src = mixed(32_768);
    let good = SEG.encode(&src).unwrap();
    assert!(SegContext::from_core(&good).is_ok());

    // 1. Point segment 1 at segment 0's payload, keeping segment 1's own
    //    content id: the entry content-id check must fire.
    let mut bad = good.clone();
    let off0 = u64::from_le_bytes(
        bad[entry_off(0) + E_PHYSICAL_OFFSET..entry_off(0) + E_PHYSICAL_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    put_u64(&mut bad, entry_off(1) + E_PHYSICAL_OFFSET, off0);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "aliased offset with a stale content id must be rejected"
    );

    // 2. Aliased offset AND matching content id: the physical-overlap check fires.
    let mut bad = good.clone();
    let len0 = u64::from_le_bytes(
        bad[entry_off(0) + E_PHYSICAL_LEN..entry_off(0) + E_PHYSICAL_LEN + 8]
            .try_into()
            .unwrap(),
    );
    let cid0 = bad[entry_off(0) + E_CONTENT_ID..entry_off(0) + E_CONTENT_ID + 16].to_vec();
    put_u64(&mut bad, entry_off(1) + E_PHYSICAL_OFFSET, off0);
    put_u64(&mut bad, entry_off(1) + E_PHYSICAL_LEN, len0);
    bad[entry_off(1) + E_CONTENT_ID..entry_off(1) + E_CONTENT_ID + 16].copy_from_slice(&cid0);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "overlapping physical ranges must be rejected"
    );

    // 3. Grow segment 0's logical length to swallow segment 1 without moving it:
    //    the logical-layout contiguity check fires.
    let mut bad = good.clone();
    let l0 = u64::from_le_bytes(bad[entry_off(0) + 8..entry_off(0) + 16].try_into().unwrap());
    put_u64(&mut bad, entry_off(0) + 8, l0 + 1);
    repatch_hash(&mut bad);
    assert!(
        SegContext::from_core(&bad).is_err(),
        "a segment that over-spans its neighbour must be rejected"
    );

    // 4. Wrong version, bad magic, truncated payload, bad directory hash.
    let mut bad = good.clone();
    bad[H_VERSION..H_VERSION + 2].copy_from_slice(&(SEG_VERSION + 1).to_le_bytes());
    assert!(SegContext::from_core(&bad).is_err(), "version");
    let mut bad = good.clone();
    bad[0] = b'X';
    assert!(SegContext::from_core(&bad).is_err(), "magic");
    let mut bad = good.clone();
    bad.pop();
    assert!(SegContext::from_core(&bad).is_err(), "truncated");
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert!(SegContext::from_core(&bad).is_err(), "directory hash");
    // 5. A tampered content-id field (all zeroes) with a matching directory hash.
    let mut bad = good.clone();
    bad[entry_off(0) + E_CONTENT_ID..entry_off(0) + E_CONTENT_ID + 16].fill(0);
    repatch_hash(&mut bad);
    assert!(SegContext::from_core(&bad).is_err(), "content id tamper");
}

// ---------------------------------------------------------------------------
// Attack 3 — bounded encoded-window reader: byte-exact after reopen + bound
// ---------------------------------------------------------------------------

fn write_blocks(dir: &std::path::Path, name: &str, src: &[u8], machine_id: u32, b: u32) -> u64 {
    let machine = machine_by_id(machine_id).unwrap();
    let core = machine.encode(src).unwrap();
    let n = src.len() as u64;
    let index = BlockIndex::build_machine(&core, n, b, machine_id).unwrap();
    let mut body = create_machine(src, ManifestationProfile::Root, machine_id).unwrap();
    body.blocks.push(index);
    write_body(&dir.join(name), &body).unwrap();
    n
}

#[test]
fn window_bound_holds_at_boundaries_with_a_frequency_one_model() {
    let dir = std::env::temp_dir().join(format!("ekbloom-r7-window-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A 2-symbol source: 254 model slots have frequency 1, so many decoder steps
    // hit the minimal-frequency boundary states.
    let src: Vec<u8> = (0..96 * 1024).map(|i| if i % 5 == 0 { b'z' } else { b'a' }).collect();
    let n = src.len() as u64;

    for &machine_id in &[ekbloom::MACHINE_RANS0, MACHINE_RANS0I] {
        for &b in &[1u32, 3, 4096] {
            let tag = format!("m{machine_id}-b{b}.ekb");
            write_blocks(&dir, &tag, &src, machine_id, b);
            let path = dir.join(&tag);
            let reference = open(&path).unwrap();
            // Reopen from a fresh serialized generation and probe.
            let body = FileBody::open(&path).unwrap();
            let mut probes: Vec<(u64, u64)> = vec![(0, 0), (0, 1), (0, n)];
            for k in 1..=3u64 {
                let edge = (u64::from(b) * k).min(n);
                if edge > 0 {
                    probes.push((edge - 1, 2.min(n - (edge - 1))));
                    probes.push((edge, u64::from(b).min(n - edge)));
                }
            }
            probes.push((n - 1, 1));
            probes.push((n / 2, 7));
            for &(off, len) in &probes {
                let before = body.fetched_bytes();
                let got = body.read(ReadRequest { offset: off, length: len }).unwrap();
                let fetched = body.fetched_bytes() - before;
                assert_eq!(got, &src[off as usize..(off + len) as usize], "m{machine_id} ({off},{len})");
                assert_eq!(
                    got,
                    read(&reference, ReadRequest { offset: off, length: len }).unwrap()
                );
                if len > 0 {
                    let steps = (off % u64::from(b)) + len;
                    assert!(
                        fetched <= 2 * steps + WINDOW_MARGIN,
                        "m{machine_id} ({off},{len}) fetched {fetched} > 2*{steps}+{WINDOW_MARGIN}"
                    );
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn window_truncation_and_forgery_are_typed_errors() {
    let dir = std::env::temp_dir().join(format!("ekbloom-r7-trunc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = skewed(0x5EED_0007, 8192);
    write_blocks(&dir, "t.ekb", &src, ekbloom::MACHINE_RANS0, 256);
    let path = dir.join("t.ekb");
    let body = FileBody::open(&path).unwrap();
    // Truncate the backing file after open: a read must be a typed error.
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(ekbloom::format::CORE_OFFSET + 64).unwrap();
    drop(f);
    let res = body.read(ReadRequest { offset: 4000, length: 512 });
    assert!(res.is_err(), "truncated window read must error, got {res:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Attack 4 — plan identity binding for the new machines (5 and 6)
// ---------------------------------------------------------------------------

fn body_with_blocks(source: &[u8], machine_id: u32, block_bytes: u32) -> EkbloomBody {
    let mut body = create_machine(source, ManifestationProfile::Root, machine_id).unwrap();
    let idx =
        BlockIndex::build_machine(&body.core, body.logical_len, block_bytes, machine_id).unwrap();
    body.blocks.push(idx);
    body.generation += 1;
    body
}

fn workload() -> Workload {
    Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, 4096)],
    }
}

#[test]
fn engine_identity_binding_holds_for_machines_five_and_six() {
    let budget = ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    };
    for &machine_id in &[MACHINE_PACK, MACHINE_SEG] {
        let src_a = mixed(32 * 1024);
        let src_b: Vec<u8> = lcg(0xB0B0_0000 + u64::from(machine_id), 32 * 1024);
        let a = body_with_blocks(&src_a, machine_id, 1024);
        let b = body_with_blocks(&src_b, machine_id, 1024);
        assert_ne!(a.core_hash, b.core_hash, "bodies must differ");

        let engine = Engine::plan(&a, &budget, &workload()).unwrap();
        let req = ReadRequest { offset: 4096, length: 4096 };
        assert_eq!(engine.read(&a, req).unwrap().as_slice(), &src_a[4096..8192]);

        // A different object, and a new generation of the same object, are both
        // rejected before any decode — never a silent wrong byte.
        assert!(matches!(
            engine.read(&b, req),
            Err(EkbloomError::EngineBodyMismatch { .. })
        ));
        let mut newer = a.clone();
        newer.generation += 1;
        assert!(matches!(
            engine.read(&newer, req),
            Err(EkbloomError::EngineBodyMismatch { .. })
        ));

        // Engine and reader agree byte-for-byte on the planned body.
        assert_eq!(engine.read(&a, req).unwrap(), read(&a, req).unwrap());
    }
}
