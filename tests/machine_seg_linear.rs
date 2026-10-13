//! Machine 6 (`OrANS-SEG`) whole-object decode is **linear** in N.
//!
//! Regression guard for a quadratic defect: the default `OrMachine::decode`
//! revealed one symbol at a time, and each `Seg::reveal` re-parsed the whole
//! container — including a full directory+payload BLAKE3 hash — so the trait
//! `decode` was `O(N^2)` (~415 s at `N = 2^20`). `Seg` now overrides `decode`
//! (and the whole-object `state_log`) to parse once and dispatch segment-wise via
//! `SegContext::decode_into`, which is `O(N)`.

use std::time::Instant;

use ekbloom::{
    create_machine, machine_by_id, read, ManifestationProfile, ReadRequest, MACHINE_SEG,
};

/// A mixed source — a constant run, a low-entropy (packed) run and a uniform
/// (raw) run — so the segmented container exercises constant, packed and raw
/// segments across its directory.
fn mixed_source(len: usize) -> Vec<u8> {
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let quarter = len / 4;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let b = (s >> 33) as u8;
        out.push(if i < quarter {
            0xA5
        } else if i < 2 * quarter {
            b & 0x03
        } else {
            b
        });
    }
    out
}

/// Byte-exactness: the trait `decode` and the whole-object `state_log` agree with
/// the authoritative `reader::read` at the full `2^20` domain.
#[test]
fn machine6_decode_is_byte_exact_at_two_megabytes() {
    let n = 1usize << 20;
    let src = mixed_source(n);
    let machine = machine_by_id(MACHINE_SEG).expect("machine 6 is registered");
    let core = machine.encode(&src).expect("encode");

    // Authoritative oracle: the reader's segment-wise path.
    let body = create_machine(&src, ManifestationProfile::Root, MACHINE_SEG).expect("body");
    let via_reader = read(
        &body,
        ReadRequest {
            offset: 0,
            length: n as u64,
        },
    )
    .expect("reader");
    assert_eq!(via_reader, src, "reader oracle");

    // The trait `decode` must agree byte-for-byte at 2^20.
    let via_trait = machine.decode(&core, n as u64).expect("machine 6 decode");
    assert_eq!(via_trait, src, "machine 6 trait decode mismatch at 2^20");
    assert_eq!(
        via_trait, via_reader,
        "trait decode must equal reader::read"
    );

    // `state_log` projects `[x_0, ..., x_n]` with `x_0 = 0` and `x_i = s_{i-1}`.
    let log = machine
        .state_log(&core, n as u64)
        .expect("machine 6 state_log");
    assert_eq!(log.len(), n + 1, "state log length");
    assert_eq!(log[0], 0, "genesis state");
    assert_eq!(log[1], u32::from(src[0]), "first state");
    assert_eq!(log[n], u32::from(src[n - 1]), "last state");
    assert!(
        log[1..].iter().zip(&src).all(|(&x, &b)| x == u32::from(b)),
        "state log must be the symbol projection"
    );
}

/// Bounded-time / scaling: decoding `2^20` completes far below any quadratic
/// floor, and the cost grows ~linearly (not quadratically) with N.
#[test]
fn machine6_decode_scales_linearly() {
    let small = 1usize << 14; // 16 KiB
    let large = 1usize << 20; // 1 MiB
    let factor = (large / small) as f64; // 64x

    let machine = machine_by_id(MACHINE_SEG).expect("machine 6 is registered");

    // Best-of-three to shed allocator/scheduler noise; the container is built once.
    let timed = |n: usize| -> f64 {
        let src = mixed_source(n);
        let core = machine.encode(&src).expect("encode");
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            let out = machine.decode(&core, n as u64).expect("machine 6 decode");
            let dt = t.elapsed().as_secs_f64();
            assert_eq!(out, src, "decode mismatch at n={n}");
            best = best.min(dt);
        }
        best
    };

    let t_small = timed(small);
    let t_large = timed(large);

    // The old O(N^2) path was ~415 s here; the linear path is sub-millisecond.
    assert!(
        t_large < 15.0,
        "machine 6 decode of 2^20 took {t_large:.3}s (quadratic?)"
    );

    // Linear time grows with `factor` (64x); quadratic would grow with
    // `factor^2` (4096x). A 256x ceiling is far above linear and far below
    // quadratic, so it is robust to measurement noise.
    let ratio = t_large / t_small.max(1e-9);
    assert!(
        ratio < 256.0,
        "machine 6 decode scaled {ratio:.1}x for a {factor:.0}x size increase (expected ~{factor:.0}x)"
    );
}
