#![no_main]
//! Machine roundtrip fuzz target: for arbitrary input, encode then decode across
//! **all** registered machines (ids 1–9) and assert the decoded bytes equal the
//! source exactly.
//!
//! The input is read as `[selector: up to 3 bytes][body]`. The selector picks a
//! requested logical length in `[0, 2^20]`; the source is the body repeated
//! cyclically to that length (so a tiny input can drive a longer source). For
//! fuzz throughput the *materialized* length per iteration is capped at
//! [`FUZZ_LEN`] — the full `2^20` domain is machine-checked deterministically by
//! gate **G1** ([`tests/gates.rs`](../../tests/gates.rs), `docs/GATES.md`).
//!
//! For every machine whose `encode` succeeds, `decode` must succeed and return
//! the source byte-for-byte (invertibility), and a shorter requested length must
//! return the source prefix for the machines that take the length from the caller
//! (machines 5/6/9 declare it in the core and reject a mismatch).

use ekbloom::machine::{machine_by_id, OrMachine};
use ekbloom::machine_seg::{SegContext, SEG};
use ekbloom::MACHINE_SEG;
use libfuzzer_sys::fuzz_target;

/// The advertised logical-length domain (`2^20`).
const MAX_LEN: usize = 1 << 20;
/// Per-iteration materialization cap (fuzz throughput); the full domain is
/// covered by gate **G1**.
const FUZZ_LEN: usize = 1 << 12;

/// Materialize `len` bytes by repeating `body` cyclically.
fn expand(body: &[u8], len: usize) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    if body.is_empty() {
        return vec![0u8; len];
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(body[i % body.len()]);
    }
    out
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // `[selector: 3 LE bytes][body]`; a 1–2 byte input keeps its own length.
    let (selector, body): (usize, &[u8]) = if data.len() >= 3 {
        let sel = u32::from_le_bytes([data[0], data[1], data[2], 0]) as usize;
        (sel, &data[3..])
    } else {
        (data.len(), data)
    };
    let requested = selector % (MAX_LEN + 1);
    let len = requested.min(FUZZ_LEN);
    let src = expand(body, len);
    let n = src.len() as u64;

    for id in 1..=9u32 {
        let Some(machine) = machine_by_id(id) else {
            continue;
        };
        let Ok(core) = machine.encode(&src) else {
            // A machine may decline a source; nothing to check.
            continue;
        };
        if id == MACHINE_SEG {
            // `Seg` does not override the reference `OrMachine::decode`, which
            // re-parses the core per symbol (O(n^2)); use the linear segment-wise
            // `SegContext::decode_into` the read API uses.
            let ctx = SegContext::from_core(&core).expect("seg ctx from an encoded core");
            let mut omega = SEG.init(&core).expect("seg init");
            let mut out = Vec::new();
            ctx.decode_into(&core, &mut omega, n, &mut out)
                .unwrap_or_else(|e| panic!("machine 6: decode_into failed (n={n}): {e:?}"));
            assert_eq!(out, src, "machine 6: roundtrip mismatch (n={n})");
            continue;
        }
        let out = machine
            .decode(&core, n)
            .unwrap_or_else(|e| panic!("machine {id}: decode failed after encode (n={n}): {e:?}"));
        assert_eq!(out, src, "machine {id}: roundtrip mismatch (n={n})");

        // A shorter requested length must reveal exactly that prefix for the
        // caller-length machines; the core-declared machines reject a mismatch,
        // which is also a valid (non-panicking) outcome.
        if n >= 2 {
            for l in [n / 2, n - 1] {
                if let Ok(prefix) = machine.decode(&core, l) {
                    assert_eq!(
                        prefix,
                        src[..l as usize],
                        "machine {id}: prefix length {l} mismatch (n={n})"
                    );
                }
            }
        }
    }
});
