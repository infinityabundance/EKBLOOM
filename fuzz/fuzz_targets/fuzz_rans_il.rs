#![no_main]
//! Fuzz the machine-8 (`OrANS-RANS0IL`, independent-lane) parser/decoder.
//!
//! Arbitrary bytes fed to `IlContext::from_core` and the machine's decode / check /
//! state-log / reveal / skip paths must never panic (no arithmetic overflow, no
//! attacker-sized allocation, no out-of-bounds slice). Crafted per-lane cursors
//! (loaded verbatim from a restart) must be clean typed errors. A real encode
//! round-trips byte-exactly for every lane count.

use ekbloom::machine::OrMachine;
use ekbloom::machine_rans_il::{
    decode_il, encode_il, il_block_restarts, IlContext, IlState, MAX_IL_LANES, RANS0IL,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes as a machine-8 core: every entry point is total.
    if let Ok(ctx) = IlContext::from_core(data) {
        let _ = ctx.lanes();
        let mut s = ctx.genesis(data);
        let _ = ctx.reveal(data, &mut s);
        let _ = ctx.skip(data, &mut s, 8);
        let mut out = Vec::new();
        let _ = ctx.decode_into(data, &mut s, 16, &mut out);

        // 1b. Crafted per-lane cursors at the extreme must be typed errors.
        let mut bad = IlState {
            lanes: [1 << 23; MAX_IL_LANES],
            cursors: [u64::MAX; MAX_IL_LANES],
            lane: 0,
        };
        let _ = ctx.reveal(data, &mut bad);
        let _ = ctx.skip(data, &mut bad, 4);
        let _ = ctx.validate(&bad);
    }
    let _ = RANS0IL.decode(data, 64);
    let _ = RANS0IL.check_core(data, 64);
    let _ = RANS0IL.state_log(data, 64);

    // 2. A real, bounded source round-trips for every lane count.
    let m = data.len().min(512);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;
    for k in 1..=8usize {
        let Ok(core) = encode_il(src, k) else {
            continue;
        };
        assert_eq!(RANS0IL.decode(&core, n).unwrap(), src, "machine 8 k={k}");
        let mut out = vec![0u8; m];
        decode_il(&core, &mut out).unwrap();
        assert_eq!(out, src);
        let _ = il_block_restarts(&core, n, 64);
    }
});
