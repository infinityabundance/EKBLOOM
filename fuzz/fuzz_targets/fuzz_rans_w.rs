#![no_main]
//! Fuzz the machine-7 (`OrANS-RANS0W`, 16-bit word renorm) parser/decoder.
//!
//! Arbitrary bytes fed to `RansWContext::from_core` and the machine's decode /
//! check / state-log / reveal / peek / skip paths must never panic (no shift or
//! arithmetic overflow, no attacker-sized allocation, no index-out-of-bounds). In
//! particular a **crafted cursor at the extreme** (a restart/checkpoint cursors is
//! loaded verbatim) must be a clean typed `Err`, never an unchecked `cursor + 1`
//! overflow. A real encode round-trips byte-exactly.

use ekbloom::machine::OrMachine;
use ekbloom::machine_rans_w::{encode as encode_word16, RansWContext, RANS0W};
use ekbloom::Snapshot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes as a machine-7 core: every entry point is total.
    if let Ok(ctx) = RansWContext::from_core(data) {
        let _ = ctx.header_bytes();
        if let Ok(mut omega) = RANS0W.init(data) {
            let _ = RANS0W.peek(data, &omega);
            let _ = RANS0W.reveal(data, &mut omega);
            let _ = ctx.skip(data, &mut omega, 8);
            let mut out = Vec::new();
            let _ = ctx.decode_into(data, &mut omega, 16, &mut out);
        }

        // 1b. A crafted cursor at the extreme must be a typed error, not a panic.
        for cur in [u64::MAX, u64::MAX - 1, data.len() as u64 + 1, 1 << 15] {
            let mut omega = Snapshot::new(1 << 15, cur);
            let _ = ctx.peek(data, &omega);
            let _ = ctx.skip(data, &mut omega, 2);
            let mut out = Vec::new();
            let _ = ctx.decode_into(data, &mut omega, 4, &mut out);
        }
    }
    let _ = RANS0W.decode(data, 64);
    let _ = RANS0W.check_core(data, 64);
    let _ = RANS0W.state_log(data, 64);

    // 2. A real, bounded source round-trips byte-exactly.
    let m = data.len().min(512);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;
    let core = encode_word16(src);
    assert_eq!(
        RANS0W.decode(&core, n).unwrap(),
        src,
        "machine 7 round-trip"
    );
    assert!(RANS0W.check_core(&core, n).is_ok());
    let log = RANS0W.state_log(&core, n).unwrap();
    assert_eq!(log.len() as u64, n + 1);
});
