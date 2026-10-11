//! Algebra, exactness and oracle tests for `ekbloom::prefix`.
//!
//! Only the public API is exercised: `LinMap`/`Affine` algebra against a direct
//! bit-matrix reference, `block_summary` against repeated machine steps,
//! `prefix_scan` against the machine state log, `reconstruct_blocks` against the
//! authoritative `NL2::decode` for every thread count, and `state_at_via_prefix`
//! against `state_log` for every position on small inputs.

use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::prefix::{
    block_summary, prefix_scan, reconstruct_blocks, reconstruct_scalar, state_at_via_prefix,
    Affine, LinMap,
};
use ekbloom::state::Snapshot;

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

fn random_map(seed: u64) -> LinMap {
    let mut cols = [0u16; 16];
    let mut s = seed;
    for c in cols.iter_mut() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *c = (s >> 33) as u16;
    }
    LinMap { cols }
}

/// Independent reference: matrix action, one XOR per set bit of `x`.
fn ref_apply(cols: &[u16; 16], x: u16) -> u16 {
    let mut y = 0u16;
    let mut k = 0u32;
    while k < 16 {
        if (x >> k) & 1 == 1 {
            y ^= cols[k as usize];
        }
        k += 1;
    }
    y
}

/// Apply a map `n` times by hand.
fn iterate(m: &LinMap, n: u32, x: u16) -> u16 {
    let mut y = x;
    let mut i = 0u32;
    while i < n {
        y = m.apply(y);
        i += 1;
    }
    y
}

#[test]
fn linmap_apply_matches_matrix_action_and_is_linear() {
    for seed in [1u64, 2, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0] {
        let m = random_map(seed);
        let mut x = 0u32;
        while x < 65536 {
            let xu = x as u16;
            assert_eq!(
                m.apply(xu),
                ref_apply(&m.cols, xu),
                "apply mismatch at {xu}"
            );
            x += 1;
        }
        // Linearity over GF(2).
        for a in [0u16, 1, 0x00FF, 0x1234, 0xFFFF] {
            for b in [0u16, 2, 0x8000, 0xABCD, 0x5555] {
                assert_eq!(m.apply(a ^ b), m.apply(a) ^ m.apply(b));
            }
        }
    }
}

#[test]
fn linmap_compose_and_power_match_direct_action() {
    let a = random_map(0xAAAA);
    let b = random_map(0xBBBB);
    let c = random_map(0xCCCC);

    // compose == apply inner then outer.
    let ab = a.compose(&b);
    for x in [0u16, 1, 2, 0x0100, 0x1234, 0xFFFF, 0x00FF] {
        assert_eq!(ab.apply(x), a.apply(b.apply(x)));
    }

    // power == iterated application (including the identity at n = 0).
    for n in [0u32, 1, 2, 3, 5, 16, 255] {
        let p = a.power(u64::from(n));
        for x in [0u16, 1, 0x8000, 0x1234, 0xFFFF] {
            assert_eq!(p.apply(x), iterate(&a, n, x), "power mismatch n={n} x={x}");
        }
    }

    // Associativity of composition.
    let left = a.compose(&b).compose(&c);
    let right = a.compose(&b.compose(&c));
    assert_eq!(left, right, "(a o b) o c != a o (b o c)");

    // Identity is neutral.
    let id = LinMap::identity();
    assert_eq!(a.compose(&id), a);
    assert_eq!(id.compose(&a), a);
}

#[test]
fn affine_compose_matches_sequential_apply() {
    let m1 = Affine {
        map: random_map(0x1111),
        add: 0x2468,
    };
    let m2 = Affine {
        map: random_map(0x2222),
        add: 0x1357,
    };
    let m3 = Affine {
        map: random_map(0x3333),
        add: 0x0F0F,
    };
    let c = m1.compose(&m2);
    for x in [0u16, 1, 2, 0x0100, 0x1234, 0xFFFF, 0x00FF] {
        assert_eq!(c.apply(x), m1.apply(m2.apply(x)));
    }
    // Associativity.
    assert_eq!(m1.compose(&m2).compose(&m3), m1.compose(&m2.compose(&m3)));
    // Identity is neutral.
    let id = Affine::identity();
    assert_eq!(m1.compose(&id), m1);
    assert_eq!(id.compose(&m1), m1);
}

#[test]
fn block_summary_matches_repeated_machine_steps() {
    // The one-step linear part A is the summary of a length-1 block.
    let a = block_summary(&[0u8]).map;
    for len in [0usize, 1, 2, 3, 5, 8, 17, 64, 255, 300] {
        let r = lcg(0x0B10_C0DE ^ len as u64, len);
        for &x0 in &[0u16, 1, 0x1234, 0xFFFF, 0x8000] {
            // Machine reference: reveal `len` symbols from `x0` over `r`.
            let mut omega = Snapshot::new(u32::from(x0), 0);
            for _ in 0..len {
                NL2.reveal(&r, &mut omega).unwrap();
            }
            let expect = omega.state as u16;
            let got = block_summary(&r).apply(x0);
            assert_eq!(got, expect, "summary mismatch len={len} x0={x0:#06x}");
        }
        // The linear part is exactly A^len.
        assert_eq!(
            block_summary(&r).map,
            a.power(len as u64),
            "A^len len={len}"
        );
    }
}

#[test]
fn prefix_scan_yields_correct_block_start_states() {
    let core = lcg(0x9E37_79B9_7F4A_7C15, 4096);
    for &bs in &[1usize, 2, 3, 7, 64, 100, 512] {
        let summaries: Vec<Affine> = {
            let mut v = Vec::new();
            let mut lo = 0usize;
            while lo < core.len() {
                let hi = (lo + bs).min(core.len());
                v.push(block_summary(&core[lo..hi]));
                lo = hi;
            }
            v
        };
        let nb = summaries.len();
        let prefix = prefix_scan(&summaries);
        assert_eq!(prefix.len(), nb + 1);
        assert_eq!(prefix[0], Affine::identity());

        let logs = state_log(&core, core.len() as u64).unwrap();

        // prefix[k] is the composition of the first k blocks.
        for k in 0..=nb {
            let mut folded = Affine::identity();
            for blk in &summaries[..k] {
                folded = blk.compose(&folded);
            }
            assert_eq!(prefix[k], folded, "prefix fold mismatch k={k} bs={bs}");
            // Its action on genesis is the machine state at position k*bs.
            let pos = (k * bs).min(core.len());
            assert_eq!(
                u32::from(prefix[k].apply(0)),
                logs[pos],
                "start-state mismatch k={k} bs={bs}"
            );
        }
    }
}

fn make_inputs() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = Vec::new();
    v.push(Vec::new());
    v.push(vec![0x00]);
    v.push(vec![0xFF]);
    v.push((0u8..=255).collect());
    v.push(vec![7u8; 100]);
    v.push(vec![0x11u8; 4096]);
    v.push(
        (0..1000u32)
            .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
            .collect(),
    );
    v.push(lcg(0x0BAD_F00D_CAFE_BEEF, 257));
    v.push(lcg(0x1122_3344_5566_7788, 1000));
    v.push(lcg(0xDEAD_BEEF_1234_5678, 4097));
    // Lengths deliberately not divisible by the block sizes under test.
    v.push(lcg(0x5151_5151_5151_5151, 63));
    v.push(lcg(0x6161_6161_6161_6161, 65));
    v.push(lcg(0x7171_7171_7171_7171, 100));
    v
}

#[test]
fn reconstruct_blocks_matches_decode_for_all_thread_counts() {
    for core in make_inputs() {
        let n = core.len() as u64;
        let oracle = NL2.decode(&core, n).unwrap();

        // Scalar baseline agrees with the authoritative decode from genesis.
        assert_eq!(reconstruct_scalar(0, &core), oracle, "scalar mismatch");

        for &bs in &[1usize, 2, 7, 64, 256, 1024] {
            let summaries: Vec<Affine> = {
                let mut v = Vec::new();
                let mut lo = 0usize;
                while lo < core.len() {
                    let hi = (lo + bs).min(core.len());
                    v.push(block_summary(&core[lo..hi]));
                    lo = hi;
                }
                v
            };
            let nb = summaries.len();
            let prefix = prefix_scan(&summaries);
            let starts: Vec<u16> = prefix[..nb].iter().map(|a| a.apply(0)).collect();
            let blocks: Vec<&[u8]> = {
                let mut v = Vec::new();
                let mut lo = 0usize;
                while lo < core.len() {
                    let hi = (lo + bs).min(core.len());
                    v.push(&core[lo..hi]);
                    lo = hi;
                }
                v
            };

            for &threads in &[1usize, 2, 3, 4, 8, 16] {
                let out = reconstruct_blocks(&starts, &blocks, threads).unwrap();
                assert_eq!(out.len(), nb, "block count bs={bs} threads={threads}");
                let flat: Vec<u8> = out.concat();
                assert_eq!(
                    flat, oracle,
                    "reconstruction mismatch bs={bs} threads={threads} n={n}"
                );
            }

            // Threads = 0 is clamped to 1 and still exact.
            let out0 = reconstruct_blocks(&starts, &blocks, 0).unwrap();
            assert_eq!(out0.concat(), oracle);
        }
    }
}

#[test]
fn reconstruct_blocks_rejects_mismatched_lengths_and_handles_empty() {
    let core = lcg(1, 128);
    let summaries: Vec<Affine> = core.chunks(16).map(block_summary).collect();
    let prefix = prefix_scan(&summaries);
    let starts: Vec<u16> = prefix[..summaries.len()]
        .iter()
        .map(|a| a.apply(0))
        .collect();
    let blocks: Vec<&[u8]> = core.chunks(16).collect();

    assert!(reconstruct_blocks(&starts[..1], &blocks, 4).is_err());
    assert!(reconstruct_blocks(&[], &[], 4).unwrap().is_empty());
}

#[test]
fn reconstruct_blocks_with_parallel_start_states_is_exact_from_nonzero_x0() {
    // A non-genesis start state must still decode exactly (machine-level oracle).
    let core = lcg(0xFEED_FACE_CAFE_1234, 700);
    let bs = 32usize;
    let x0 = 0xBEEFu16;
    let summaries: Vec<Affine> = core.chunks(bs).map(block_summary).collect();
    let prefix = prefix_scan(&summaries);
    let starts: Vec<u16> = prefix[..summaries.len()]
        .iter()
        .map(|a| a.apply(x0))
        .collect();
    let blocks: Vec<&[u8]> = core.chunks(bs).collect();
    let flat: Vec<u8> = reconstruct_blocks(&starts, &blocks, 8).unwrap().concat();

    let mut omega = Snapshot::new(u32::from(x0), 0);
    let mut expect = Vec::with_capacity(core.len());
    for _ in 0..core.len() {
        expect.push(NL2.reveal(&core, &mut omega).unwrap());
    }
    assert_eq!(flat, expect);
}

#[test]
fn state_at_via_prefix_matches_state_log_for_all_positions() {
    let core = lcg(0x0F0F_0F0F_1234_5678, 2000);
    let n = core.len();
    let logs = state_log(&core, n as u64).unwrap();

    // stride = 1: every position is a block boundary, exact for all i.
    let per_byte: Vec<Affine> = core.iter().map(|&b| block_summary(&[b])).collect();
    let prefix = prefix_scan(&per_byte);
    assert_eq!(prefix.len(), n + 1);
    for (i, &expect) in logs.iter().enumerate() {
        assert_eq!(
            u32::from(state_at_via_prefix(&prefix, 0, 1, i as u64)),
            expect,
            "state_at mismatch at i={i}"
        );
    }

    // stride > 1: exact at every block boundary.
    for &bs in &[1u64, 3, 64, 256, 2000, 4096] {
        let summaries: Vec<Affine> = {
            let mut v = Vec::new();
            let mut lo = 0usize;
            while lo < n {
                let hi = (lo + bs as usize).min(n);
                v.push(block_summary(&core[lo..hi]));
                lo = hi;
            }
            v
        };
        let prefix = prefix_scan(&summaries);
        let mut k = 0u64;
        while (k as usize) * (bs as usize) <= n {
            let pos = (k as usize) * (bs as usize);
            assert_eq!(
                u32::from(state_at_via_prefix(&prefix, 0, bs, pos as u64)),
                logs[pos],
                "boundary mismatch bs={bs} pos={pos}"
            );
            k += 1;
        }
    }

    // Out-of-range and empty scans never panic.
    let _ = state_at_via_prefix(&[], 0x1234, 64, 10_000_000);
    let _ = state_at_via_prefix(&prefix, 0x1234, 0, 5);
    let _ = state_at_via_prefix(&prefix, 0x1234, 64, u64::MAX);
}
