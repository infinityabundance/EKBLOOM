#![no_main]
//! Whole-object allocation bound (resource policy) fuzz target.
//!
//! A tiny forged core can declare an astronomical logical length (machines 5/6
//! carry the declared length in the core header; machines 2/4 take it from the
//! caller). Every whole-object entry point — `decode`, `state_log`,
//! `bulk_unpack`, `unpack_range`, a context/prepared `decode_into` — must answer
//! with a typed [`EkbloomError::AllocationTooLarge`] rather than attempting the
//! declared allocation. This target installs a small default ceiling and drives
//! those entry points from the core's declared length; the process must never
//! abort with a capacity-overflow / allocation failure.

use ekbloom::machine_pack::PACK;
use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_i::RANS0I;
use ekbloom::machine_seg::{SegContext, SEG};
use ekbloom::machine_store::STORE;
use ekbloom::resource::{checked_default, ResourcePolicyGuard};
use ekbloom::{EkbloomError, OrMachine};
use libfuzzer_sys::fuzz_target;

/// A deliberately small ceiling so a forged declaration is refused without a
/// large (but still "legal") allocation dominating the campaign.
const FUZZ_CAP: u64 = 1 << 20; // 1 MiB

fuzz_target!(|data: &[u8]| {
    let _guard = ResourcePolicyGuard::set(FUZZ_CAP);

    // Machines 2 and 4 take the logical length from the caller: a forged huge
    // declaration must be refused up front, never allocated.
    let _ = RANS0.decode(data, u64::MAX);
    let _ = RANS0.state_log(data, u64::MAX);
    let _ = RANS0.decode(data, FUZZ_CAP + 1);
    let _ = RANS0I.decode(data, u64::MAX);
    let _ = RANS0I.state_log(data, u64::MAX);
    let _ = RANS0I.decode(data, FUZZ_CAP + 1);

    // Machine 3 (identity) is core-bounded by `check_core`; still exercise it.
    let _ = STORE.decode(data, data.len() as u64);
    let _ = STORE.decode(data, u64::MAX);

    // Machine 5: the declared length lives in the (untrusted) packed header.
    if let Ok(ctx) = ekbloom::machine_pack::PackContext::from_core(data) {
        let n = ctx.logical_len();
        let _ = PACK.decode(data, n);
        let mut out = Vec::new();
        let _ = PACK.bulk_unpack(data, &mut out);
        let _ = ctx.unpack_range(data, 0, n, &mut out);
        let _ = PACK.decode(data, u64::MAX);
    }

    // Machine 6: the declared length lives in the (hashed) container header.
    if let Ok(ctx) = SegContext::from_core(data) {
        let n = ctx.logical_len();
        let _ = SEG.decode(data, n);
        let _ = SEG.decode(data, u64::MAX);
        if let Ok(mut omega) = SEG.init(data) {
            let mut out = Vec::new();
            let _ = ctx.decode_into(data, &mut omega, n, &mut out);
            let _ = ctx.decode_into(data, &mut omega, u64::MAX, &mut out);
        }
    }

    // The guard is exactly the ceiling: one byte over is refused, the ceiling
    // itself is permitted.
    assert!(checked_default(FUZZ_CAP).is_ok());
    assert!(matches!(
        checked_default(FUZZ_CAP + 1),
        Err(EkbloomError::AllocationTooLarge { .. })
    ));
});
