//! Cold-read hardening: the tight machine-2 block decoder, exact machine/block
//! selection, and the **fair** block-indexed LZ4 baseline.
//!
//! Pins:
//! * the fast prepared block decoder (`RansContext::decode_into_slice` /
//!   `BlockIndex::read_range_counted_rans`) is byte-exact against the
//!   authoritative machine for every source category, every block size and
//!   all three machines — including the per-symbol fallback;
//! * `baseline::select_cold_block` returns a **certified exact** path that fits
//!   the byte cap and stays byte-exact;
//! * a block-indexed LZ4 reader is byte-exact, and a cold read decompresses only
//!   the block(s) it actually touches (counted), never the whole object.

use ekbloom::baseline::select_cold_block;
use ekbloom::blocks::BlockIndex;
use ekbloom::machine::{machine_by_id, MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE};
use ekbloom::machine_rans::RansContext;

/// Block sizes exercised for the fast decoder exactness sweep.
const BLOCK_SIZES: [u32; 8] = [1, 2, 3, 7, 64, 256, 1024, 4096];

/// Deterministic pseudorandom bytes (LCG) — incompressible.
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

/// A skewed 4-symbol source machine 2 compresses (`V < N`).
fn skewed(len: usize) -> Vec<u8> {
    let mut s = 0x5EED_1234_ABCD_0001u64;
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

/// Every source category the machines must handle.
fn categories() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        vec![0x5A],
        (0u8..=255).collect(),
        vec![7u8; 300],
        (0..300u32)
            .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
            .collect(),
        b"hello, world hello, world hello".to_vec(),
        skewed(4000),
        lcg(0xDEAD_BEEF_1234_5678, 4000),
    ]
}

/// In-bounds `(offset, length)` probes including boundary crossings, partial
/// final reads and a zero-length read.
fn probes(n: u64, b: u32) -> Vec<(u64, u64)> {
    let mut v = vec![(0, 0), (0, 1), (0, n.min(9)), (n, 0)];
    let bb = u64::from(b);
    for k in 1..4u64 {
        let boundary = (bb * k).min(n);
        if boundary > 0 {
            v.push((boundary - 1, 2.min(n - (boundary - 1))));
            v.push((boundary, bb.min(n - boundary)));
        }
    }
    if n >= 5 {
        v.push((n - 3, 3));
        v.push((n / 2, (n - n / 2).min(7)));
    }
    v
}

#[test]
fn fast_block_decoder_is_byte_exact_for_every_machine_category_and_block_size() {
    for machine_id in [MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE] {
        let machine = machine_by_id(machine_id).unwrap();
        for src in categories() {
            let core = machine.encode(&src).unwrap();
            let n = src.len() as u64;
            // The authoritative full decode, for a cross-check.
            let authoritative = machine.decode(&core, n).unwrap();
            assert_eq!(authoritative, src, "authoritative decode m{machine_id}");

            for &b in &BLOCK_SIZES {
                let index = BlockIndex::build_machine(&core, n, b, machine_id).unwrap();
                let ctx =
                    (machine_id == MACHINE_RANS0).then(|| RansContext::from_core(&core).unwrap());
                for &(off, len) in &probes(n, b) {
                    if off + len > n {
                        continue;
                    }
                    let want = &src[off as usize..(off + len) as usize];
                    // A zero-length read short-circuits with zero steps; otherwise
                    // the exact count is the bounded skip plus the reveals.
                    let expect_steps = if len == 0 {
                        0
                    } else {
                        (off % u64::from(b)) + len
                    };
                    // Default (engine) path.
                    let (got, steps) = index.read_range_counted(&core, off, len).unwrap();
                    assert_eq!(got, want, "m{machine_id} B={b} ({off},{len})");
                    assert_eq!(steps, expect_steps, "steps m{machine_id} B={b}");
                    // Prepared machine-2 path.
                    if let Some(ctx) = &ctx {
                        let (got, steps) =
                            index.read_range_counted_rans(&core, ctx, off, len).unwrap();
                        assert_eq!(got, want, "prepared m2 B={b} ({off},{len})");
                        assert_eq!(steps, expect_steps);
                    }
                }
            }
        }
    }
}

#[test]
fn prepared_decoder_matches_per_symbol_step_sequence() {
    // The fused-table decoder must reproduce the exact state/cursor sequence of
    // the authoritative per-symbol `reveal`, symbol for symbol.
    for src in [skewed(3000), lcg(0x1234_5678_9ABC_DEF0, 3000)] {
        let core = machine_by_id(MACHINE_RANS0).unwrap().encode(&src).unwrap();
        let ctx = RansContext::from_core(&core).unwrap();
        let machine = machine_by_id(MACHINE_RANS0).unwrap();
        let mut omega = machine.init(&core).unwrap();
        // Decode in irregular chunks so the local-state decoder is exercised
        // across chunk boundaries, comparing to a fresh authoritative replay of
        // the same prefix after each chunk.
        let mut decoded = Vec::new();
        for chunk in [1usize, 7, 64, 300, 1000, 1628] {
            let end = (decoded.len() + chunk).min(src.len());
            let want = end - decoded.len();
            let mut out = vec![0u8; want];
            ctx.decode_into_slice(&core, &mut omega, &mut out).unwrap();
            decoded.extend_from_slice(&out);

            let mut author = machine.init(&core).unwrap();
            for _ in 0..decoded.len() {
                machine.reveal(&core, &mut author).unwrap();
            }
            assert_eq!(author, omega, "state/cursor diverged at {}", decoded.len());
        }
        assert_eq!(decoded, src);
    }
}

#[test]
fn machine_and_block_selection_returns_a_certified_exact_path() {
    let src = skewed(64 * 1024);
    let n = src.len() as u64;
    let cap = 2 * n;
    let sel = select_cold_block(
        &src,
        &[MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE],
        &[256, 1024],
        cap,
    )
    .unwrap();
    assert!([MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE].contains(&sel.machine_id));
    assert!(sel.stored_bytes <= cap);
    // The returned path is independently certifiable and byte-exact.
    assert!(sel.certify().unwrap(), "selected path failed certification");
    for off in [0u64, 1, 4095, 4096, 40_000] {
        let len = 100.min(n - off);
        assert_eq!(
            sel.read(off, len).unwrap(),
            &src[off as usize..(off + len) as usize]
        );
    }

    // A tight cap that only machine 2's compressed representation can meet forces
    // the selector onto machine 2 — the certified exact path still holds.
    let core2 = machine_by_id(MACHINE_RANS0).unwrap().encode(&src).unwrap();
    let idx2 = BlockIndex::build_machine(&core2, n, 1024, MACHINE_RANS0).unwrap();
    let tight = core2.len() as u64 + idx2.directory_bytes() + idx2.restart_bytes();
    assert!(tight < n, "machine 2 must be smaller than raw here");
    let sel = select_cold_block(
        &src,
        &[MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE],
        &[1024],
        tight,
    )
    .unwrap();
    assert_eq!(sel.machine_id, MACHINE_RANS0);
    assert!(sel.certify().unwrap());

    // No representation fits an impossibly small cap.
    assert!(select_cold_block(&src, &[MACHINE_RANS0], &[1024], 8).is_err());
}

// ---------------------------------------------------------------------------
// Fair block-indexed LZ4 baseline.
// ---------------------------------------------------------------------------

/// A block-indexed LZ4 reader that reports the exact number of compressed blocks
/// it decompressed and the total decoded bytes — so a cold read can be shown to
/// touch only the block(s) it needs, never the whole object.
struct Lz4Blocks {
    blocks: Vec<Vec<u8>>,
    block: usize,
    n: usize,
    decompressed_blocks: std::cell::Cell<u64>,
    decoded_bytes: std::cell::Cell<u64>,
}

impl Lz4Blocks {
    fn build(source: &[u8], block: usize) -> Lz4Blocks {
        Lz4Blocks {
            blocks: source.chunks(block).map(lz4_flex::compress).collect(),
            block,
            n: source.len(),
            decompressed_blocks: std::cell::Cell::new(0),
            decoded_bytes: std::cell::Cell::new(0),
        }
    }

    /// Direct-access read: decompress only the covering blocks.
    fn read(&self, offset: u64, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        let mut pos = offset as usize;
        let end = pos + len as usize;
        while pos < end {
            let b = pos / self.block;
            let start = b * self.block;
            let seg_end = end.min(start + self.block);
            let full = (self.n - start).min(self.block);
            let decoded = lz4_flex::decompress(&self.blocks[b], full).expect("lz4 block");
            self.decompressed_blocks
                .set(self.decompressed_blocks.get() + 1);
            self.decoded_bytes
                .set(self.decoded_bytes.get() + decoded.len() as u64);
            out.extend_from_slice(&decoded[pos - start..seg_end - start]);
            pos = seg_end;
        }
        out
    }
}

#[test]
fn block_indexed_lz4_is_byte_exact() {
    let src = skewed(16 * 1024 + 7);
    for &b in &[1024usize, 4096, 16384] {
        let lz4 = Lz4Blocks::build(&src, b);
        // Whole object.
        assert_eq!(lz4.read(0, src.len() as u64), src);
        // Every block boundary and an assortment of unaligned reads.
        for &(off, len) in &[
            (0u64, 1u64),
            (0, 4096),
            (1, 4096),
            (b as u64 - 1, 2),
            (b as u64, b as u64 + 3),
            (src.len() as u64 - 5, 5),
            (src.len() as u64, 0),
        ] {
            if off + len <= src.len() as u64 {
                assert_eq!(
                    lz4.read(off, len),
                    &src[off as usize..(off + len) as usize],
                    "lz4 B={b} ({off},{len})"
                );
            }
        }
    }
}

#[test]
fn cold_lz4_read_decompresses_only_the_needed_blocks() {
    let src = skewed(64 * 1024);
    let n = src.len() as u64;

    // A 4 KiB-aligned 4 KiB read with a 4 KiB grid touches exactly one block and
    // decodes one block's worth of bytes — not the whole 64 KiB object.
    let lz4 = Lz4Blocks::build(&src, 4096);
    let got = lz4.read(8192, 4096);
    assert_eq!(got, &src[8192..12288]);
    assert_eq!(lz4.decompressed_blocks.get(), 1);
    assert_eq!(lz4.decoded_bytes.get(), 4096);
    assert!(lz4.decoded_bytes.get() < n);

    // An unaligned read spanning exactly two 4 KiB blocks decompresses two.
    let lz4 = Lz4Blocks::build(&src, 4096);
    let _ = lz4.read(4090, 12);
    assert_eq!(lz4.decompressed_blocks.get(), 2);
    assert_eq!(lz4.decoded_bytes.get(), 8192);

    // A finer 1 KiB grid decomposes a 4 KiB read into four 1 KiB blocks.
    let lz4 = Lz4Blocks::build(&src, 1024);
    let _ = lz4.read(4096, 4096);
    assert_eq!(lz4.decompressed_blocks.get(), 4);
    assert_eq!(lz4.decoded_bytes.get(), 4096);
    assert!(lz4.decoded_bytes.get() < n);
}
