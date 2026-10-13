//! Streaming container: roundtrip, byte-exact positional and sequential reads,
//! checkpoint boundaries, determinism, bounded memory, and typed rejection of
//! crafted/truncated/wrong-version containers.

use ekbloom::{
    EkbloomError, RepresentationPolicy, StreamBuilder, StreamOptions, StreamReader,
    STREAM_HEADER_BYTES,
};
use std::io::Cursor;

// Header/directory/footer field offsets (mirroring the frozen layout).
const H_VERSION: usize = 8;
const H_CRC: usize = 52;
const F_CRC: usize = 152;
const F_DIRECTORY_OFFSET: usize = 40;

/// Deterministic pseudorandom bytes.
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

/// Skewed (low-entropy) bytes: compresses under packing/rANS.
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

fn mixed(seed: u64, len: usize) -> Vec<u8> {
    let half = len / 2;
    let mut out = skewed(seed, half);
    out.extend_from_slice(&lcg(seed ^ 0xABCD, len - half));
    out
}

/// Build a container into a `Vec<u8>` sink, feeding `src` in odd-sized chunks.
fn build(src: &[u8], block_bytes: u32, policy: RepresentationPolicy) -> Vec<u8> {
    let mut sink = Vec::new();
    {
        let mut b = StreamBuilder::new(
            &mut sink,
            StreamOptions {
                block_bytes,
                machine: policy,
            },
        )
        .unwrap();
        for chunk in src.chunks(131) {
            b.write(chunk).unwrap();
        }
        b.finish().unwrap();
    }
    sink
}

/// Read all bytes sequentially through odd-sized buffers.
fn read_all(reader: &StreamReader<Cursor<Vec<u8>>>) -> Vec<u8> {
    let mut out = Vec::new();
    let sizes = [1usize, 3, 17, 4096, 65537];
    let mut i = 0usize;
    loop {
        let size = sizes[i % sizes.len()];
        let mut buf = vec![0u8; size];
        let n = reader.read_sequential(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        i += 1;
    }
    out
}

#[test]
fn roundtrip_small_medium_large() {
    for &len in &[0usize, 1, 2, 4095, 4096, 4097, 100_000, 1_000_003] {
        let src = mixed(0x5EED_0000_0000_0001, len);
        let bytes = build(&src, 8192, RepresentationPolicy::Auto);
        let reader = StreamReader::open(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.len(), len as u64);
        assert_eq!(read_all(&reader), src);
        assert_eq!(reader.read_sequential(&mut [0u8; 8]).unwrap(), 0);

        // Every position is byte-exact for small corpora; sampled otherwise.
        if len <= 4097 {
            for (i, &b) in src.iter().enumerate() {
                let mut one = [0u8; 1];
                reader.read_at(i as u64, &mut one).unwrap();
                assert_eq!(one[0], b);
            }
        }
    }
}

#[test]
fn every_block_and_sampled_positions_are_byte_exact() {
    let src = mixed(0x5EED_0000_0000_0002, 256 * 1024 + 7);
    let block = 8192u32;
    let bytes = build(&src, block, RepresentationPolicy::Auto);
    let reader = StreamReader::open(Cursor::new(bytes)).unwrap();

    // Every block boundary (and boundary - 1) is exact.
    for e in reader.entries() {
        let start = e.logical_start as usize;
        let end = (e.logical_start + e.logical_len) as usize;
        let mut one = [0u8; 1];
        reader.read_at(e.logical_start, &mut one).unwrap();
        assert_eq!(one[0], src[start]);
        reader.read_at((end - 1) as u64, &mut one).unwrap();
        assert_eq!(one[0], src[end - 1]);
    }

    // Sampled positions with assorted lengths, including cross-block spans.
    let n = src.len();
    for &len in &[1usize, 7, 64, 4096, 9999] {
        let mut off = 0usize;
        while off + len <= n {
            let mut got = vec![0u8; len];
            reader.read_at(off as u64, &mut got).unwrap();
            assert_eq!(got, &src[off..off + len], "len={len} off={off}");
            off += 997;
        }
    }

    // Out-of-bounds and zero-length behavior.
    let mut one = [0u8; 1];
    assert!(matches!(
        reader.read_at(n as u64, &mut one),
        Err(EkbloomError::OutOfBounds { .. })
    ));
    reader.read_at(n as u64, &mut []).unwrap();
}

#[test]
fn read_sequential_equals_source() {
    let src = skewed(0x5EED_0000_0000_0003, 500_000);
    let bytes = build(&src, 16 * 1024, RepresentationPolicy::Auto);
    let reader = StreamReader::open(Cursor::new(bytes)).unwrap();
    assert_eq!(read_all(&reader), src);
}

#[test]
fn checkpoint_boundaries_are_restarts() {
    let a = lcg(1, 1000);
    let b = skewed(2, 2000);
    let c = lcg(3, 17);
    let mut sink = Vec::new();
    {
        let mut sb = StreamBuilder::new(
            &mut sink,
            StreamOptions {
                block_bytes: 4096,
                machine: RepresentationPolicy::Auto,
            },
        )
        .unwrap();
        sb.write(&a).unwrap();
        sb.checkpoint().unwrap();
        sb.write(&b).unwrap();
        sb.checkpoint().unwrap();
        sb.write(&c).unwrap();
        sb.finish().unwrap();
    }
    let reader = StreamReader::open(Cursor::new(sink)).unwrap();
    assert_eq!(reader.block_count(), 3);
    let lens: Vec<u64> = reader.entries().iter().map(|e| e.logical_len).collect();
    assert_eq!(lens, vec![1000, 2000, 17]);

    let mut expect = a.clone();
    expect.extend_from_slice(&b);
    expect.extend_from_slice(&c);
    assert_eq!(read_all(&reader), expect);

    // A positional read spanning the first checkpoint boundary is exact.
    let mut span = vec![0u8; 2000];
    reader.read_at(0, &mut span).unwrap();
    assert_eq!(span, &expect[..2000]);
}

#[test]
fn all_policies_roundtrip() {
    let src = mixed(0x5EED_0000_0000_0004, 300_000);
    for policy in [
        RepresentationPolicy::Auto,
        RepresentationPolicy::Packed,
        RepresentationPolicy::Segmented,
        RepresentationPolicy::Word16,
        RepresentationPolicy::IndependentLanes,
    ] {
        let bytes = build(&src, 32 * 1024, policy);
        let reader = StreamReader::open(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.len(), src.len() as u64);
        assert_eq!(read_all(&reader), src, "policy {policy:?}");
        let mut got = vec![0u8; 4096];
        reader.read_at(123_456, &mut got).unwrap();
        assert_eq!(got, &src[123_456..123_456 + 4096], "policy {policy:?}");
    }
}

#[test]
fn determinism_for_fixed_options() {
    let src = mixed(0x5EED_0000_0000_0005, 200_000);
    for policy in [RepresentationPolicy::Auto, RepresentationPolicy::Packed] {
        let a = build(&src, 4096, policy);
        let b = build(&src, 4096, policy);
        assert_eq!(a, b, "policy {policy:?} is not deterministic");
    }
}

#[test]
fn crafted_truncated_and_wrong_version_are_typed_errors() {
    let src = mixed(0x5EED_0000_0000_0006, 400_000);
    let bytes = build(&src, 8192, RepresentationPolicy::Auto);

    // Too short to hold header + footer.
    assert!(matches!(
        StreamReader::open(Cursor::new(bytes[..STREAM_HEADER_BYTES].to_vec())),
        Err(EkbloomError::Truncated { .. })
    ));

    // Bad header magic.
    let mut m = bytes.clone();
    m[0] ^= 0xFF;
    assert!(matches!(
        StreamReader::open(Cursor::new(m)),
        Err(EkbloomError::BadMagic)
    ));

    // Wrong header version (with the header CRC repaired so the version is what fails).
    let mut v = bytes.clone();
    v[H_VERSION] = 9;
    let crc = crc32fast::hash(&v[..H_CRC]);
    v[H_CRC..H_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(
        StreamReader::open(Cursor::new(v)),
        Err(EkbloomError::UnsupportedVersion { .. })
    ));

    // Corrupt a directory byte -> directory digest mismatch. The directory sits
    // after the payloads, at the footer's `directory_offset`.
    let mut d = bytes.clone();
    let footer_at = d.len() - 160;
    let dir_off = u64::from_le_bytes(
        d[footer_at + F_DIRECTORY_OFFSET..footer_at + F_DIRECTORY_OFFSET + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    d[dir_off] ^= 0x01;
    assert!(matches!(
        StreamReader::open(Cursor::new(d)),
        Err(EkbloomError::ChecksumMismatch { .. })
    ));

    // Corrupt footer CRC -> rejected.
    let mut f = bytes.clone();
    let n = f.len();
    f[n - 160 + F_CRC] ^= 0xFF;
    assert!(matches!(
        StreamReader::open(Cursor::new(f)),
        Err(EkbloomError::ChecksumMismatch { .. })
    ));

    // Corrupt a block payload -> content-id mismatch on read.
    let mut p = bytes.clone();
    p[STREAM_HEADER_BYTES] ^= 0x01;
    let reader = StreamReader::open(Cursor::new(p)).unwrap();
    let mut one = [0u8; 1];
    assert!(matches!(
        reader.read_at(0, &mut one),
        Err(EkbloomError::ChecksumMismatch { .. })
    ));

    // Trailing junk after the footer -> geometry mismatch.
    let mut t = bytes.clone();
    t.push(0);
    assert!(StreamReader::open(Cursor::new(t)).is_err());
}

#[test]
fn block_bytes_zero_is_rejected() {
    let err = StreamBuilder::new(
        Vec::new(),
        StreamOptions {
            block_bytes: 0,
            machine: RepresentationPolicy::Auto,
        },
    );
    assert!(matches!(err, Err(EkbloomError::InvalidArgument { .. })));
}

#[test]
fn bounded_memory_for_a_stream_larger_than_the_block() {
    // A sink that discards bytes and counts them, so nothing of the stream is
    // retained by the harness.
    struct Sink(u64);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let n: u64 = 16 * 1024 * 1024; // 16 MiB stream
    let block: u32 = 16 * 1024; // 16 KiB blocks -> 1024 blocks (256x ratio)
    let base_peak = ekbloom::peak_rss_bytes();

    let mut builder = StreamBuilder::new(
        Sink(0),
        StreamOptions {
            block_bytes: block,
            machine: RepresentationPolicy::Packed,
        },
    )
    .unwrap();
    let mut chunk = vec![0u8; 1 << 20];
    let mut written = 0u64;
    let mut gen = 0x1234_5678_9ABC_DEF0u64;
    while written < n {
        let take = ((n - written) as usize).min(chunk.len());
        for b in chunk[..take].iter_mut() {
            gen = gen
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (gen >> 33) as u8;
        }
        builder.write(&chunk[..take]).unwrap();
        written += take as u64;
    }
    let manifest = builder.finish().unwrap();
    let peak_delta = ekbloom::peak_rss_bytes().saturating_sub(base_peak);

    // The directory is O(blocks) and tiny; the payloads were streamed out.
    assert_eq!(manifest.logical_len, n);
    assert_eq!(manifest.directory_bytes, manifest.block_count * 56);
    assert!(
        manifest.directory_bytes < (1 << 20),
        "directory {} B is unexpectedly large",
        manifest.directory_bytes
    );
    // A whole-stream buffer would grow RSS by >= 16 MiB (the stream size); a
    // bounded builder grows it by roughly one block plus the directory.
    assert!(
        peak_delta < (12 << 20),
        "peak RSS grew by {peak_delta} B for a {n} B stream: the stream was buffered"
    );
}
