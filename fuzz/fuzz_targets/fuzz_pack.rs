#![no_main]
//! Fuzz `OrANS-PACK` (machine 5): the directly-addressable packed representation.
//!
//! Arbitrary bytes fed to `PackContext::from_core` and the machine's decode /
//! check / positional / bulk paths must never panic (no shift or arithmetic
//! overflow, no attacker-sized allocation, no index-out-of-bounds). An accepted
//! core must reveal exactly its declared length and every positional read must
//! equal the sequential decode. A real encode round-trips byte-exactly for every
//! declared width (1..=8).

use ekbloom::machine_pack::{PackContext, PACK};
use ekbloom::OrMachine;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes as a machine-5 core: every entry point is total.
    if let Ok(ctx) = PackContext::from_core(data) {
        let n = ctx.logical_len();
        let _ = ctx.bits();
        let _ = ctx.alphabet_len();
        // A *bounded* sequential range (never the core-declared whole length: a
        // constant core may legitimately declare a huge `n` with no payload) must
        // equal the positional reads that the read paths actually use.
        let cap = n.min(1024);
        let mut out = Vec::new();
        if ctx.unpack_range(data, 0, cap, &mut out).is_ok() {
            assert_eq!(out.len() as u64, cap, "bounded decode length");
            for &i in &[0u64, cap / 2, cap.saturating_sub(1)] {
                if i < cap {
                    assert_eq!(ctx.byte_at(data, i).unwrap(), out[i as usize], "pos {i}");
                }
            }
        }
        let _ = ctx.byte_at(data, 0);
        let _ = ctx.byte_at(data, n);
        let _ = ctx.byte_at(data, u64::MAX);
        let mut out2 = Vec::new();
        let _ = ctx.unpack_range(data, n / 2, 16, &mut out2);
        let _ = PACK.check_core(data, n);
    }
    let _ = PACK.decode(data, 64);
    let _ = PACK.check_core(data, 0);
    let _ = PACK.logical_len(data);

    // 2. A real, bounded source round-trips exactly for every declared width.
    let m = data.len().min(512);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;

    let core = match PACK.encode(src) {
        Ok(c) => c,
        Err(_) => return,
    };
    assert_eq!(PACK.decode(&core, n).unwrap(), src, "auto round-trip");
    let ctx = PackContext::from_core(&core).unwrap();
    for (i, &b) in src.iter().enumerate() {
        assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), b, "auto pos {i}");
    }
    assert!(ctx.byte_at(&core, n).is_err());

    for bits in 1u8..=8 {
        let c = match PACK.encode_with_bits(src, bits) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ctx = match PackContext::from_core(&c) {
            Ok(ctx) => ctx,
            Err(_) => continue,
        };
        assert_eq!(ctx.logical_len(), n, "bits={bits} length");
        for (i, &b) in src.iter().enumerate() {
            assert_eq!(ctx.byte_at(&c, i as u64).unwrap(), b, "bits={bits} pos={i}");
        }
        assert!(ctx.byte_at(&c, n).is_err());
    }
});
