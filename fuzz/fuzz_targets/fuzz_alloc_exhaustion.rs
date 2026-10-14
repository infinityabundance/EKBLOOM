#![no_main]
//! Allocation exhaustion — declared sizes near and past the `ResourcePolicy`
//! ceiling must be refused before allocating.
//!
//! Machines 5/6 carry the declared logical length in the (attacker-authorable)
//! core header; machines 2/4 take it from the caller. A crafted input can
//! declare a length at, one past, or astronomically past the ceiling. Every
//! whole-object entry point (`decode`, `state_log`, `bulk_unpack`, `unpack_range`,
//! a context/prepared `decode_into`) must answer with a typed
//! [`ekbloom::EkbloomError::AllocationTooLarge`] for a declaration over the cap —
//! never an attempt at the declared allocation, never an abort. The target
//! installs a small ceiling so a "legal" declaration cannot dominate the campaign;
//! the mechanism is identical to the production 1 GiB default.

use ekbloom::machine::prepare;
use ekbloom::machine_pack::{PackContext, PACK};
use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_i::RANS0I;
use ekbloom::machine_seg::{SegContext, SEG};
use ekbloom::machine_store::STORE;
use ekbloom::resource::{checked_default, ResourcePolicyGuard};
use ekbloom::{OrMachine, Snapshot, DICT};
use libfuzzer_sys::fuzz_target;

/// A deliberately small ceiling so a forged declaration is refused without a
/// large (but still "legal") allocation dominating the campaign.
const FUZZ_CAP: u64 = 1 << 20;

/// A little-endian `u64` from `data[at..]`, zero-padded past the end.
fn u64_from(data: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    for (i, s) in b.iter_mut().enumerate() {
        *s = data.get(at + i).copied().unwrap_or(0);
    }
    u64::from_le_bytes(b)
}

fuzz_target!(|data: &[u8]| {
    let _guard = ResourcePolicyGuard::set(FUZZ_CAP);

    // Declared lengths that stress the boundary: the input's own value, exactly
    // the cap (legal), one past, `u64::MAX - 1`, and `u64::MAX` (overflow).
    let declared = u64_from(data, 0);
    for n in [declared, FUZZ_CAP, FUZZ_CAP + 1, u64::MAX - 1, u64::MAX] {
        // Machines 2 and 4 take the length from the caller.
        let _ = RANS0.decode(data, n);
        let _ = RANS0.state_log(data, n);
        let _ = RANS0I.decode(data, n);
        let _ = RANS0I.state_log(data, n);
        // Machine 3 (identity) is core-bounded; still exercise the declaration.
        let _ = STORE.decode(data, n);
        // Machines 5 and 6: a forged core header may declare anything.
        let _ = PACK.decode(data, n);
        let _ = SEG.decode(data, n);
        let _ = DICT.decode(data, n);
    }

    // The core-declared lengths (machines 5/6): what the core itself claims.
    if let Ok(ctx) = PackContext::from_core(data) {
        let n = ctx.logical_len();
        let mut out = Vec::new();
        let _ = PACK.decode(data, n);
        let _ = PACK.bulk_unpack(data, &mut out);
        let _ = ctx.unpack_range(data, 0, n, &mut out);
        if let Ok(prepared) = prepare(&PACK, data) {
            let _ = prepared.decode_into(data, &mut Snapshot::new(0, 0), n, &mut out);
        }
        // And the maximal forgery through the same context.
        let _ = PACK.decode(data, u64::MAX);
        let _ = ctx.unpack_range(data, 0, u64::MAX, &mut out);
    }
    if let Ok(ctx) = SegContext::from_core(data) {
        let n = ctx.logical_len();
        let _ = SEG.decode(data, n);
        let _ = SEG.decode(data, u64::MAX);
        if let Ok(mut omega) = SEG.init(data) {
            let mut out = Vec::new();
            let _ = ctx.decode_into(data, &mut omega, n, &mut out);
            let _ = ctx.decode_into(data, &mut omega, u64::MAX, &mut out);
        }
        if let Ok(prepared) = prepare(&SEG, data) {
            let mut out = Vec::new();
            let _ = prepared.decode_into(data, &mut Snapshot::new(0, 0), n, &mut out);
        }
    }

    // The guard is exactly the ceiling: the cap is legal, one byte over is refused.
    assert!(checked_default(FUZZ_CAP).is_ok());
    assert!(checked_default(FUZZ_CAP + 1).is_err());
    assert!(checked_default(u64::MAX).is_err());
});
