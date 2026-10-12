#![no_main]
//! Fuzz the raw parsers/decoders of the two new machines:
//! `OrANS-Store` (machine 3, identity) and `OrANS-RANS0I` (machine 4,
//! interleaved order-0 rANS).
//!
//! Arbitrary bytes fed to machine 4's `RansIContext::from_core` and its decode /
//! check / state-log / reveal / peek / skip paths must never panic (no shift or
//! arithmetic overflow, no attacker-sized allocation, no index-out-of-bounds).
//! Crafted cores that parse must either decode or return a clean `Err`. A real
//! machine-4 encode round-trips byte-exactly for every lane count and its symbols
//! must equal machine 2's; machine 3's identity codec must round-trip verbatim.

use ekbloom::machine::OrMachine;
use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_i::{encode_with_k, RansIContext, RANS0I};
use ekbloom::machine_store::STORE;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes as a machine-4 core: all entry points are total.
    if let Ok(ctx) = RansIContext::from_core(data) {
        let _ = ctx.lanes();
        if let Ok(mut omega) = RANS0I.init(data) {
            let _ = RANS0I.peek(data, &omega);
            let _ = RANS0I.reveal(data, &mut omega);
            let _ = ctx.skip(data, &mut omega, 8);
            let mut out = Vec::new();
            let _ = ctx.decode_into(data, &mut omega, 16, &mut out);
        }
    }
    let _ = RANS0I.decode(data, 64);
    let _ = RANS0I.check_core(data, 64);
    let _ = RANS0I.state_log(data, 64);

    // 2. Raw bytes as a machine-3 (verbatim) core: decode is a bounded copy.
    let _ = STORE.decode(data, data.len() as u64);
    let _ = STORE.check_core(data, data.len() as u64);
    let _ = STORE.decode(data, data.len() as u64 + 1);
    let _ = STORE.state_log(data, data.len() as u64);

    // 3. A real, bounded source round-trips on both machines for every lane count.
    let m = data.len().min(512);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;

    let identity = STORE.encode(src).unwrap();
    assert_eq!(identity, src, "machine 3 must be the identity");
    assert_eq!(STORE.decode(&identity, n).unwrap(), src);

    let m2 = RANS0.decode(&RANS0.encode(src).unwrap(), n).unwrap();
    assert_eq!(m2, src, "machine 2 oracle");
    for k in 1..=8usize {
        let core = encode_with_k(src, k).unwrap();
        assert!(RANS0I.check_core(&core, n).is_ok());
        assert_eq!(RANS0I.decode(&core, n).unwrap(), src, "machine 4 k={k}");
    }
});
