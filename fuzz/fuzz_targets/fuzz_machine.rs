#![no_main]
//! Interpreter over the raw fuzz bytes: derive a logical length and decode a
//! core. Must never panic — only return values or errors. When a decode
//! succeeds the decoded length must equal the requested logical length, and the
//! state log must have `logical_len + 1` entries.

use ekbloom::machine::{state_log, OrMachine, NL2};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // When there are at least 8 bytes, the first 8 are a little-endian length
    // request; the rest is the core. Otherwise the whole input is the core and
    // its length is the logical length.
    let (core, logical_len) = if data.len() >= 8 {
        let raw = u64::from_le_bytes(data[..8].try_into().unwrap());
        let core = &data[8..];
        let cap = core.len() as u64;
        (core, raw % (cap + 1))
    } else {
        (data, data.len() as u64)
    };

    if let Ok(decoded) = NL2.decode(core, logical_len) {
        assert_eq!(decoded.len() as u64, logical_len, "decode length mismatch");
        if let Ok(log) = state_log(core, logical_len) {
            assert_eq!(
                log.len() as u64,
                logical_len + 1,
                "state log length mismatch"
            );
        }
    }
});
