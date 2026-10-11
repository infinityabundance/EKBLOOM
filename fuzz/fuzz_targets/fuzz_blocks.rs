#![no_main]
//! Fuzz the persisted block-index parsers (`parse_block_index_prefix`,
//! `BlockIndex::from_bytes`) and the block restart read path.
//!
//! Arbitrary bytes must never panic (no shift/overflow, no attacker-sized
//! allocation). A mutated but internally-consistent index must be a clean `Err`
//! or a clean rejection from `verify_against`/`certify`, and a real index must
//! read byte-exactly against the authoritative machine on both machines.

use ekbloom::blocks::{BlockIndex, BLOCK_INDEX_PREFIX_LEN};
use ekbloom::machine::{OrMachine, MACHINE_NL2, MACHINE_RANS0, NL2};
use ekbloom::machine_rans::RANS0;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes: prefix parser + full parser must never panic.
    let _ = ekbloom::parse_block_index_prefix(data);
    if let Ok(idx) = BlockIndex::from_bytes(data) {
        let _ = idx.verify();
        let _ = idx.verify_against(&[]);
        let _ = idx.certify(&[]);
        let _ = idx.block_of(0);
        let _ = idx.block_of(u64::MAX);
        let _ = idx.max_steps(0, 0);
        let _ = idx.read_range(&[], 0, 0);
        let _ = idx.read_range(&[], 0, 1);
        let _ = idx.read_range(&[], u64::MAX, 1);
        let _ = idx.to_bytes();
    }

    // 2. A real index over a bounded core exercises exact reads on both machines.
    let m = data.len().min(600);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;

    for machine in [MACHINE_NL2, MACHINE_RANS0] {
        let core = match machine {
            MACHINE_NL2 => match NL2.encode(src) {
                Ok(c) => c,
                Err(_) => continue,
            },
            _ => match RANS0.encode(src) {
                Ok(c) => c,
                Err(_) => continue,
            },
        };
        let block_bytes = 1 + (data[0] as u32 % 4096);
        let idx = match BlockIndex::build_machine(&core, n, block_bytes, machine) {
            Ok(i) => i,
            Err(_) => continue,
        };
        assert!(idx.verify());
        assert!(idx.verify_against(&core).unwrap_or(false));
        assert!(idx.certify(&core).unwrap_or(false));

        // Every restart lies exactly where a fresh authoritative pass says.
        let bytes = idx.to_bytes();
        assert!(BlockIndex::from_bytes(&bytes).is_ok());

        // Bounded, input-derived reads are byte-exact and within the step bound.
        for w in data.chunks(3).take(64) {
            if w.len() < 3 {
                continue;
            }
            let off = (u64::from(w[0]) | (u64::from(w[1]) << 8)) % (n + 1);
            let len = u64::from(w[2]) % 32;
            if off.checked_add(len).map_or(false, |e| e <= n) {
                let got = idx.read_range(&core, off, len).unwrap();
                assert_eq!(&got, &src[off as usize..(off + len) as usize]);
                if n > 0 {
                    let (_, steps) = idx.read_range_counted(&core, off, len).unwrap();
                    assert!(steps <= idx.max_steps(off, len));
                }
            }
        }

        // A crafted prefix with an out-of-range block size is rejected cleanly.
        if bytes.len() >= BLOCK_INDEX_PREFIX_LEN {
            for bad in [0u32, (1u32 << 30) + 1, u32::MAX] {
                let mut crafted = bytes.clone();
                crafted[14..18].copy_from_slice(&bad.to_le_bytes());
                assert!(BlockIndex::from_bytes(&crafted).is_err());
            }
        }
    }
});
