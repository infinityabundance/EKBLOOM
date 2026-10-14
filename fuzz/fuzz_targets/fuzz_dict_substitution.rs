#![no_main]
//! Dictionary substitution — wrong / forged / relabelled ids, oversized
//! dictionaries and dictionary/record mismatches must be typed errors.
//!
//! `OrANS-DICT` (machine 9) frames a record with a 16-byte `dict_id` and a
//! `dict_len`. A core may **reference** its dictionary (resolved from the
//! registry by content id) or **embed** it (validated by recomputing the content
//! id). An adversary may: reference an unregistered / forged id, relabel an
//! embedded dictionary so its bytes disagree with the frame id, declare an
//! oversized `dict_len`, or present a record encoded under a *different*
//! dictionary. None of these may panic, allocate without bound, or silently
//! return a wrong byte. `decode` is also driven from the fuzzer bytes directly.

use std::sync::OnceLock;

use ekbloom::dictionary::{self, Dictionary};
use ekbloom::machine::{OrMachine, MACHINE_DICT};
use ekbloom::machine_dict::DictContext;
use ekbloom::state::Snapshot;
use ekbloom::{frame_dictionary_embedded, frame_dictionary_reference, DICT};
use libfuzzer_sys::fuzz_target;

const RECORD: &[u8] = b"user=alice|region=us-east-1|kind=alpha|v=7";

/// A dictionary trained once from a fixed text-like corpus.
fn dict_a() -> &'static Dictionary {
    static A: OnceLock<Dictionary> = OnceLock::new();
    A.get_or_init(|| {
        register_train(&[
            b"user=alice|region=us-east-1|kind=alpha",
            b"user=bob|region=eu-west-1|kind=beta",
            b"user=carol|region=us-west-2|kind=gamma",
        ])
    })
}

/// A *different* dictionary (distinct content id) for mismatch cases.
fn dict_b() -> &'static Dictionary {
    static B: OnceLock<Dictionary> = OnceLock::new();
    B.get_or_init(|| {
        register_train(&[
            b"temperature=21.5;humidity=40;pressure=1013",
            b"temperature=19.0;humidity=55;pressure=1002",
            b"temperature=30.1;humidity=12;pressure=1008",
        ])
    })
}

fn register_train(records: &[&[u8]]) -> Dictionary {
    let dict = Dictionary::train(records).expect("train dictionary");
    // Best-effort registration so the *valid* referenced path also resolves; a
    // poisoned lock is simply a typed error at lookup time.
    let _ = dictionary::register(&dict);
    dict
}

fn u32_from(data: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    for (i, s) in b.iter_mut().enumerate() {
        *s = data.get(at + i).copied().unwrap_or(0);
    }
    u32::from_le_bytes(b)
}

fuzz_target!(|data: &[u8]| {
    let a = dict_a();
    let b = dict_b();
    let _ = MACHINE_DICT;

    // Raw parser / machine-9 entry points from arbitrary bytes.
    let _ = Dictionary::from_bytes(data);
    let _ = DICT.init(data);
    let _ = DICT.decode(data, data.len() as u64);
    let _ = DICT.decode(data, u64::from(u32_from(data, 0)));
    if let Ok(ctx) = DictContext::from_core(data) {
        let mut omega = Snapshot::new(ctx.initial_state(), ctx.header_bytes());
        let mut out = Vec::new();
        let _ = ctx.reveal(data, &mut omega);
        let _ = ctx.peek(data, &omega);
        let n = ctx.logical_len();
        let _ = ctx.skip(data, &mut omega, n);
        let _ = ctx.decode_into(data, &mut omega, n, &mut out);
        let _ = ctx.validate_restart(&omega);
    }

    // Structured substitution: start from genuinely valid framed cores and mutate
    // only the framing / record bytes the fuzzer supplies.
    if let Ok(record_core) = a.encode(RECORD) {
        let mut referenced = frame_dictionary_reference(a, &record_core);
        // (1) Forged / relabelled referenced id.
        let take = data.len().min(16);
        referenced[..take].copy_from_slice(&data[..take]);
        let _ = DICT.init(&referenced);
        let _ = DICT.decode(&referenced, RECORD.len() as u64);

        // (2) Oversized declared `dict_len` (u32, up to ~4 GiB) with a tiny core.
        let mut oversized = frame_dictionary_reference(a, &record_core);
        oversized[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let _ = DICT.init(&oversized);
        if data.len() >= 4 {
            oversized[16..20].copy_from_slice(&data[..4]);
            let _ = DICT.init(&oversized);
        }

        // (3) A referenced core whose id is valid but whose record state is forged.
        let mut forged_state = frame_dictionary_reference(a, &record_core);
        let state_off = 20 + 4;
        if forged_state.len() >= state_off + 4 {
            forged_state[state_off..state_off + 4]
                .copy_from_slice(&u32_from(data, 0).to_le_bytes());
        }
        let _ = DICT.init(&forged_state);
    }

    // (4) Dictionary/record mismatch: a core framed with dictionary A's id but
    //     whose embedded/spelled record came from dictionary B.
    if let Ok(other_core) = b.encode(RECORD) {
        let mut mismatch = frame_dictionary_reference(a, &other_core);
        mismatch[16..20].copy_from_slice(&0u32.to_le_bytes()); // reference A
        let _ = DICT.init(&mismatch);

        // (5) Embedded dictionary B relabelled as dictionary A: the recomputed
        //     content id disagrees with the frame id, so it must be refused.
        let mut embedded = frame_dictionary_embedded(b, &other_core);
        embedded[..16].copy_from_slice(&a.id());
        let _ = DICT.init(&embedded);
    }
});
