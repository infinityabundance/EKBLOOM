#![no_main]
//! Streaming-container fuzz target (`src/stream.rs`).
//!
//! Two properties:
//!
//! 1. **Untrusted input is total.** Arbitrary bytes fed to [`StreamReader::open`]
//!    and its reads must return values or typed errors — never panic
//!    (`read_at`/`read_sequential` are exercised at the extremes, including
//!    `u64::MAX`).
//! 2. **A built container round-trips exactly.** Bytes are written through a
//!    [`StreamBuilder`] in odd-sized chunks with fuzz-selected checkpoint
//!    boundaries and block size/policy, then read back sequentially and at
//!    sampled positions; every byte must equal the source and the reader's
//!    `len()` must match.

use ekbloom::{RepresentationPolicy, StreamBuilder, StreamOptions, StreamReader};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

/// Map a byte onto one of the streaming representation policies.
fn policy(tag: u8) -> RepresentationPolicy {
    match tag % 5 {
        0 => RepresentationPolicy::Auto,
        1 => RepresentationPolicy::Packed,
        2 => RepresentationPolicy::Segmented,
        3 => RepresentationPolicy::Word16,
        _ => RepresentationPolicy::IndependentLanes,
    }
}

/// Read a reader to completion through odd-sized buffers.
fn read_all(reader: &StreamReader<Cursor<Vec<u8>>>) -> Vec<u8> {
    let sizes = [1usize, 3, 17, 4096];
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        let size = sizes[i % sizes.len()];
        let mut buf = vec![0u8; size];
        match reader.read_sequential(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(k) => out.extend_from_slice(&buf[..k]),
        }
        i += 1;
    }
    out
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // 1. Arbitrary bytes into the reader: total, never panicking.
    if let Ok(reader) = StreamReader::open(Cursor::new(data.to_vec())) {
        let n = reader.len();
        let mut one = [0u8; 1];
        let _ = reader.read_at(0, &mut one);
        let _ = reader.read_at(n, &mut one);
        let _ = reader.read_at(u64::MAX, &mut one);
        let mut buf = vec![0u8; 64];
        let _ = reader.read_sequential(&mut buf);
    }

    // 2. Build a real container and require an exact roundtrip.
    let tag = data[0];
    let block_bytes = 1 + u32::from(data[0]); // 1..=256, always non-zero
    let src = &data[1..];
    let mut sink = Vec::new();
    {
        let mut builder = StreamBuilder::new(
            &mut sink,
            StreamOptions {
                block_bytes,
                machine: policy(tag),
            },
        )
        .expect("non-zero block bytes");
        for (i, chunk) in src.chunks(7).enumerate() {
            builder.write(chunk).expect("write");
            if (i as u8).wrapping_add(tag) % 3 == 0 {
                builder.checkpoint().expect("checkpoint");
            }
        }
        builder.finish().expect("finish");
    }

    let reader = StreamReader::open(Cursor::new(sink)).expect("open built container");
    assert_eq!(reader.len(), src.len() as u64, "logical length");
    assert_eq!(read_all(&reader), src, "sequential readback");

    // Sampled positional reads are byte-exact (including the last byte).
    let n = src.len();
    if n > 0 {
        for &off in &[0usize, n / 2, n - 1] {
            let mut one = [0u8; 1];
            reader
                .read_at(off as u64, &mut one)
                .expect("positional read");
            assert_eq!(one[0], src[off], "positional byte at {off}");
        }
    }
});
