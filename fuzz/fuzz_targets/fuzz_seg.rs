#![no_main]
//! Fuzz `OrANS-SEG` (machine 6): the versioned, hashed segmented container.
//!
//! Arbitrary bytes fed to `SegContext::from_core` and the machine's decode /
//! positional / skip paths must never panic (no arithmetic overflow, no
//! attacker-sized allocation, no out-of-bounds slice; a forged directory is a
//! clean `Err`). An accepted container reveals exactly its declared length and
//! its positional reads agree with a sequential decode. A real fixed or
//! variable container round-trips byte-exactly.

use ekbloom::machine_seg::{SegContext, SEG};
use ekbloom::OrMachine;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes as a machine-6 core: every entry point is total.
    if let Ok(ctx) = SegContext::from_core(data) {
        let n = ctx.logical_len();
        let _ = ctx.segment_count();
        let _ = ctx.variable();
        let _ = ctx.directory_bytes();
        let _ = ctx.payload_bytes();
        let _ = ctx.payload_offset();
        // A *bounded* sequential decode (never the core-declared whole length: a
        // constant segment may declare a huge `n` with a one-byte payload) must
        // equal the positional reads the read paths actually use.
        let cap = n.min(1024);
        if let Ok(mut omega) = SEG.init(data) {
            let mut out = Vec::new();
            if ctx.decode_into(data, &mut omega, cap, &mut out).is_ok() {
                assert_eq!(out.len() as u64, cap, "bounded decode length");
                for &i in &[0u64, cap / 3, cap / 2, cap.saturating_sub(1)] {
                    if i < cap {
                        assert_eq!(ctx.byte_at(data, i).unwrap(), out[i as usize], "pos {i}");
                    }
                }
            }
        }
        let _ = ctx.byte_at(data, 0);
        let _ = ctx.byte_at(data, n);
        let _ = ctx.byte_at(data, u64::MAX);
        if let Ok(mut omega) = SEG.init(data) {
            let _ = SEG.peek(data, &omega);
            let _ = SEG.reveal(data, &mut omega);
        }
        let _ = SEG.check_core(data, n);
    }
    let _ = SEG.decode(data, 64);
    let _ = SEG.check_core(data, 0);

    // 2. A real container round-trips exactly, in both directory forms.
    let m = data.len().min(1024);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;

    let core = SEG.encode(src).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    assert_eq!(ctx.logical_len(), n);
    assert_eq!(SEG.decode(&core, n).unwrap(), src, "fixed round-trip");
    for (i, &b) in src.iter().enumerate() {
        assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), b, "fixed pos {i}");
    }
    assert!(ctx.byte_at(&core, n).is_err());

    let split = 1 + (data[0] as usize % m);
    if split < m {
        let core = SEG
            .encode_variable(src, &[split as u64, (m - split) as u64])
            .unwrap();
        let ctx = SegContext::from_core(&core).unwrap();
        assert!(ctx.variable());
        assert_eq!(ctx.logical_len(), n);
        assert_eq!(SEG.decode(&core, n).unwrap(), src, "variable round-trip");
        for (i, &b) in src.iter().enumerate() {
            assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), b, "variable pos {i}");
        }
    }

    // Structured parser fuzzing: the directory BLAKE3 hash gates uniformly random
    // bytes, so mutate a *valid* container byte-by-byte and require a clean `Err`
    // or a panic-free, total context.
    for i in 0..core.len().min(64) {
        let mut bad = core.clone();
        bad[i] ^= data[i % data.len()];
        let Ok(ctx) = SegContext::from_core(&bad) else {
            continue;
        };
        let bn = ctx.logical_len();
        let _ = ctx.byte_at(&bad, 0);
        let _ = ctx.byte_at(&bad, bn.min(4096));
        let _ = ctx.byte_at(&bad, u64::MAX);
        if let Ok(mut omega) = SEG.init(&bad) {
            let mut out = Vec::new();
            let _ = ctx.decode_into(&bad, &mut omega, bn.min(256), &mut out);
        }
    }
});
