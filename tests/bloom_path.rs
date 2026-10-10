//! End-to-end integration tests for the exact path bloom.
//!
//! These exercise only the public API (`ekbloom::{bloom, bloom_key,
//! bloom_path, machine}`) and assert agreement with the authoritative machine
//! decode and state log across every supported depth and a wide range of inputs.

use ekbloom::bloom::{BloomKind, BloomShell, Certification, SUPPORTED_DEPTHS};
use ekbloom::bloom_key::{first_conflict, key_from_state};
use ekbloom::bloom_path::PathBloom;
use ekbloom::error::EkbloomError;
use ekbloom::machine::{state_log, OrMachine, NL2};

/// Deterministic pseudorandom bytes from a small LCG (no new dependencies).
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

/// Inputs spanning empty, single byte, every byte value, repeating, alternating,
/// deterministic pseudorandom, and lengths near (and off) block boundaries.
fn cases() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        (0u8..=255).collect(),
        vec![7u8; 64],
        (0..65u32)
            .map(|i| if i % 2 == 0 { 0x33 } else { 0xCC })
            .collect(),
        lcg(0x1234_5678_9ABC_DEF0, 1000),
    ];
    for &n in &[
        1usize, 2, 3, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129,
    ] {
        v.push(lcg(0xABCD_0000 + n as u64, n));
    }
    v
}

#[test]
fn symbols_and_blocks_match_authoritative_every_depth() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let decoded = NL2.decode(&core, n).unwrap();
        let logs = state_log(&core, n).unwrap();

        for &d in SUPPORTED_DEPTHS.iter() {
            let shell = PathBloom::build(&core, n, d, 5).unwrap();
            assert_eq!(shell.kind, BloomKind::Path);
            assert_eq!(shell.depth, d);
            assert_eq!(shell.logical_len, n);
            assert_eq!(shell.status, Certification::Unverified);

            let pb = PathBloom::from_shell(&shell).unwrap();
            let dd = u64::from(d);
            let nblocks = n.div_ceil(dd);
            assert_eq!(pb.cold.len() as u64, nblocks);
            assert_eq!(pb.hot.len() as u64, n);

            // physical_bytes == N + 4 * ceil(N / D).
            assert_eq!(pb.physical_bytes(), n + 4 * nblocks);
            // The shell's manifested payload prepends the 18-byte path prefix.
            assert_eq!(shell.physical_bytes(), pb.physical_bytes() + 18);

            // Every logical position equals the authoritative decoded symbol.
            for pos in 0..n {
                assert_eq!(
                    pb.symbol_at(pos).unwrap(),
                    decoded[pos as usize],
                    "symbol mismatch n={n} D={d} pos={pos}"
                );
            }
            assert!(pb.symbol_at(n).is_err());

            // Every block's symbols and successor state are authoritative.
            for j in 0..nblocks {
                let (syms, succ) = pb.block_at(j).unwrap();
                let start = j * dd;
                let end = (start + dd).min(n);
                assert_eq!(
                    syms.len() as u64,
                    end - start,
                    "block length n={n} D={d} j={j}"
                );
                for (k, s) in syms.iter().enumerate() {
                    assert_eq!(
                        *s,
                        decoded[start as usize + k],
                        "block symbol n={n} D={d} j={j} k={k}"
                    );
                }
                // Successor is state_log[D*(j+1)], or state_log[N] for the final
                // partial block.
                assert_eq!(succ, logs[end as usize], "successor n={n} D={d} j={j}");
            }
            assert!(pb.block_at(nblocks).is_err());
        }
    }
}

#[test]
fn shell_and_bloom_byte_roundtrips() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let core_hash = *blake3::hash(&core).as_bytes();

        for &d in SUPPORTED_DEPTHS.iter() {
            let shell = PathBloom::build(&core, n, d, 11).unwrap();
            assert_eq!(shell.core_hash, core_hash);

            // BloomShell byte roundtrip is exact.
            let bytes = shell.to_bytes();
            let back = BloomShell::from_bytes(&bytes).unwrap();
            assert_eq!(back, shell);
            assert_eq!(back.to_bytes(), bytes);
            assert_eq!(back.content_hash(), shell.content_hash());

            // PathBloom re-derivation roundtrip.
            let pb = PathBloom::from_shell(&shell).unwrap();
            let rebuilt = pb.to_shell(shell.generation, shell.core_hash);
            assert_eq!(rebuilt, shell);
            assert_eq!(PathBloom::from_shell(&rebuilt).unwrap(), pb);
        }
    }
}

#[test]
fn from_bytes_rejects_truncation_every_cut() {
    for src in cases() {
        let core = NL2.encode(&src).unwrap();
        for &d in SUPPORTED_DEPTHS.iter() {
            let shell = PathBloom::build(&core, src.len() as u64, d, 1).unwrap();
            let bytes = shell.to_bytes();
            for cut in 0..bytes.len() {
                assert!(
                    BloomShell::from_bytes(&bytes[..cut]).is_err(),
                    "expected truncation error at cut {cut} (len={}, D={d})",
                    bytes.len()
                );
            }
        }
    }
}

#[test]
fn from_shell_rejects_truncated_payload() {
    let core = NL2.encode(&lcg(3, 200)).unwrap();
    for &d in SUPPORTED_DEPTHS.iter() {
        let shell = PathBloom::build(&core, 200, d, 2).unwrap();
        // Remove the last payload byte: length no longer matches nblocks.
        let mut short = shell.clone();
        let last = short.payload.len() - 1;
        short.payload.truncate(last);
        assert!(PathBloom::from_shell(&short).is_err());
    }
}

#[test]
fn build_rejects_unsupported_depth() {
    let core = NL2.encode(b"depth").unwrap();
    for d in [0u16, 3, 5, 6, 7, 9, 31, 33, 64, 100] {
        assert!(matches!(
            PathBloom::build(&core, 5, d, 0),
            Err(EkbloomError::BadDepth { .. })
        ));
    }
}

#[test]
fn key_projection_and_sufficiency_primitive() {
    // Low 16 bits of the state word.
    assert_eq!(key_from_state(0xDEAD_BEEF), 0xBEEF);

    // All outcomes agree => no conflict => key is sufficient for the domain.
    let consistent = vec![(0u16, 1u8), (1, 2), (2, 3), (1, 2), (0, 1)];
    assert_eq!(first_conflict(consistent), None);

    // Distinct outcomes for one key => conflict with reproducible evidence.
    let conflicting = vec![(0u16, 1u8), (0, 2), (5, 9)];
    let c = first_conflict(conflicting).unwrap();
    assert_eq!(c.key, 0);
    assert_eq!(c.out_a, 1);
    assert_eq!(c.out_b, 2);
    assert_eq!(c.count, 2);
}
