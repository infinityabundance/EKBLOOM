//! Pinned-hot competitor (`PinnedHotCache`, `pinned_block_count`).
//!
//! A pinned-hot codec decompresses its hot blocks **once** and retains them
//! **permanently**; cold blocks decode on demand and are not retained. These tests
//! pin: byte-exactness of the pinned and on-demand paths, **fair footprint
//! accounting** (the pinned decoded bytes are counted alongside the compressed
//! blocks and the block index), non-eviction (pinning is permanent and bounded),
//! and determinism for a fixed seed. The whole corpus/sample budget is small so the
//! suite stays fast; the codecs are identical to `examples/frontier.rs`.

use ekbloom::{pinned_block_count, PinnedHotCache};

/// Block size used by the frontier's block-indexed LZ4 competitor.
const BLOCK: usize = 4096;

/// Deterministic LCG corpus (the same generator the frontier and baselines use).
fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
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

/// Block-indexed LZ4 blocks (one compressed block per `BLOCK`-byte chunk).
fn lz4_blocks(src: &[u8]) -> Vec<Vec<u8>> {
    src.chunks(BLOCK).map(lz4_flex::compress).collect()
}

/// Byte-exact read through a pinned-hot cache (mirrors `examples/frontier.rs`): a
/// pinned block is served from the permanent set; a cold block decodes on demand.
fn lz4_read_pinned(
    cache: &mut PinnedHotCache,
    blocks: &[Vec<u8>],
    n: usize,
    offset: u64,
    len: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = offset as usize;
    let end = pos + len as usize;
    while pos < end {
        let b = pos / BLOCK;
        let start = b * BLOCK;
        let seg_end = end.min(start + BLOCK);
        if cache.is_pinned(b as u64) {
            let decoded = cache.get(b as u64).expect("pinned block");
            out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        } else {
            let full = (n - start).min(BLOCK);
            let decoded = lz4_flex::decompress(&blocks[b], full).expect("lz4 block");
            cache.note_miss();
            out.extend_from_slice(&decoded[pos - start..seg_end - start]);
        }
        pos = seg_end;
    }
    out
}

/// Build a pinned-hot LZ4 cache over `src` with `hot_bytes` pinned; returns the
/// cache and the number of blocks pinned.
fn pinned_lz4(src: &[u8], blocks: &[Vec<u8>], hot_bytes: u64) -> (PinnedHotCache, u64) {
    let count = pinned_block_count(hot_bytes, BLOCK as u64, blocks.len() as u64);
    let mut cache = PinnedHotCache::new(BLOCK as u64, count as usize);
    for (b, block) in blocks.iter().enumerate().take(count as usize) {
        let start = b * BLOCK;
        let full = (src.len() - start).min(BLOCK);
        let decoded = lz4_flex::decompress(block, full).expect("lz4 block");
        assert!(cache.pin(b as u64, decoded), "pin admits block {b}");
    }
    (cache, count)
}

#[test]
fn pinned_hot_lz4_is_byte_exact_on_pinned_and_on_demand_blocks() {
    let src = lcg_bytes(0xB1_0000_0000_0001, 64 * 1024);
    let n = src.len() as u64;
    let blocks = lz4_blocks(&src);
    // Pin the first 16 KiB (4 blocks); everything after is cold/on-demand.
    let (mut cache, count) = pinned_lz4(&src, &blocks, 16 * 1024);
    assert_eq!(count, 4);

    // Every 4 KiB-aligned read — pinned (hot) or on-demand (cold) — is byte-exact.
    let start_hits = cache.hits();
    let start_misses = cache.misses();
    for off in (0..n - BLOCK as u64 + 1).step_by(BLOCK) {
        let got = lz4_read_pinned(&mut cache, &blocks, src.len(), off, BLOCK as u64);
        assert_eq!(
            got,
            &src[off as usize..(off + BLOCK as u64) as usize],
            "byte-exact at {off}"
        );
    }
    // The pinned prefix served hits; the cold tail forced on-demand misses, and
    // the pinned set was never displaced by those cold reads.
    assert_eq!(cache.hits() - start_hits, 4, "four pinned hot reads");
    assert!(
        cache.misses() > start_misses,
        "cold tail must decode on demand"
    );
    assert_eq!(
        cache.pinned_blocks(),
        4,
        "cold reads cannot evict pinned blocks"
    );
}

#[test]
fn pinned_footprint_counts_the_pinned_decoded_bytes() {
    let src = lcg_bytes(0xB1_0000_0000_0002, 32 * 1024);
    let blocks = lz4_blocks(&src);
    let (cache, count) = pinned_lz4(&src, &blocks, 12 * 1024);
    assert_eq!(count, 3);

    // Fair footprint: compressed blocks + the per-block u32 offset table + the
    // pinned decoded bytes. The pinned bytes are a real, additive retention.
    let compressed: u64 = blocks.iter().map(|b| b.len() as u64).sum();
    let index_bytes = blocks.len() as u64 * 4;
    let storage = compressed + index_bytes;
    let pinned_bytes = count * BLOCK as u64;
    assert_eq!(cache.pinned_bytes(), pinned_bytes);
    assert!(pinned_bytes > 0);
    let retained = storage + cache.pinned_bytes();
    assert!(
        retained > storage,
        "the pinned decoded bytes must raise the footprint above storage alone"
    );
    assert_eq!(retained, storage + pinned_bytes);
}

#[test]
fn pinning_is_permanent_and_capacity_is_bounded() {
    let mut cache = PinnedHotCache::new(BLOCK as u64, 2);
    assert_eq!(cache.block_bytes(), BLOCK as u64);
    assert_eq!(cache.max_pinned_blocks(), 2);

    assert!(cache.pin(0, vec![1u8; BLOCK]));
    assert!(cache.pin(1, vec![2u8; BLOCK]));
    // Capacity is full: a further pin is rejected and does NOT evict a pinned one.
    assert!(!cache.pin(2, vec![3u8; BLOCK]));
    assert_eq!(cache.pinned_blocks(), 2);
    assert_eq!(cache.pinned_bytes(), 2 * BLOCK as u64);
    assert!(!cache.is_pinned(2));
    // A duplicate pin is rejected and leaves the set unchanged.
    assert!(!cache.pin(0, vec![9u8; BLOCK]));
    assert_eq!(cache.pinned_blocks(), 2);

    // A cold lookup misses and cannot displace a pinned block.
    assert!(cache.get(5).is_none());
    cache.note_miss();
    assert!(cache.is_pinned(0) && cache.is_pinned(1));
    assert_eq!(cache.pinned_blocks(), 2);
    assert_eq!(cache.pinned_bytes(), 2 * BLOCK as u64);

    // A pinned lookup is a hit; the hit rate reflects hits over hits+misses.
    assert_eq!(cache.get(0), Some([1u8; BLOCK].as_slice()));
    assert_eq!(cache.hits(), 1);
    assert_eq!(cache.misses(), 1);
    assert!((cache.hit_rate() - 0.5).abs() < 1e-12);
}

#[test]
fn pinned_hot_is_deterministic_for_a_fixed_seed() {
    let src = lcg_bytes(0xB1_0000_0000_0004, 32 * 1024);
    let n = src.len() as u64;

    // A fixed-seed workload: pinned bytes, hit/miss counts and the produced output
    // must be identical across two independent builds.
    let run = || {
        let blocks = lz4_blocks(&src);
        let (mut cache, _) = pinned_lz4(&src, &blocks, 8 * 1024);
        let mut out: Vec<u8> = Vec::new();
        let mut s: u64 = 0x5EED_0000_0000_0001;
        for _ in 0..128 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let off = (s >> 33) % (n - BLOCK as u64 + 1);
            out.extend_from_slice(&lz4_read_pinned(
                &mut cache,
                &blocks,
                src.len(),
                off,
                BLOCK as u64,
            ));
        }
        (cache.pinned_bytes(), cache.hits(), cache.misses(), out)
    };
    assert_eq!(run(), run());
}

#[test]
fn pinned_hot_zstd_is_byte_exact_on_pinned_and_on_demand_chunks() {
    const CHUNK: usize = 8192;
    let src = lcg_bytes(0xB1_0000_0000_0005, 48 * 1024);
    let frames: Vec<Vec<u8>> = src
        .chunks(CHUNK)
        .map(|c| zstd::stream::encode_all(c, 0).expect("zstd frame"))
        .collect();
    let count = pinned_block_count(16 * 1024, CHUNK as u64, frames.len() as u64);
    let mut cache = PinnedHotCache::new(CHUNK as u64, count as usize);
    for (c, frame) in frames.iter().enumerate().take(count as usize) {
        let decoded = zstd::stream::decode_all(&frame[..]).expect("zstd frame");
        assert!(cache.pin(c as u64, decoded));
    }

    // Read each chunk: the pinned prefix from the cache, the tail on demand.
    for (c, frame) in frames.iter().enumerate() {
        let start = c * CHUNK;
        let len = (src.len() - start).min(CHUNK);
        let got = if cache.is_pinned(c as u64) {
            cache.get(c as u64).expect("pinned chunk")[..len].to_vec()
        } else {
            cache.note_miss();
            let decoded = zstd::stream::decode_all(&frame[..]).expect("zstd frame");
            decoded[..len].to_vec()
        };
        assert_eq!(got, &src[start..start + len], "zstd chunk {c} exact");
    }
    assert_eq!(cache.pinned_blocks(), count as usize);
    assert_eq!(cache.pinned_bytes(), count as u64 * CHUNK as u64);
    assert_eq!(cache.hits(), count);
    assert_eq!(cache.misses(), frames.len() as u64 - count);
}
