#![no_main]
//! Dictionary (`OrANS-DICT`, machine 9) fuzz target.
//!
//! A serialized dictionary record must parse totally: `Dictionary::from_bytes`
//! returns a value or a typed error (bad magic/version, wrong frequency sum, a
//! zero, or a content-id mismatch), never a panic. A record that validates may be
//! registered, looked up by its content id and used to decode a machine-9 core; a
//! forged record core (out-of-range state, absurd declared length) must be a typed
//! error, never a wrong byte or an unbounded allocation.

use ekbloom::dictionary::{self, Dictionary};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(dict) = Dictionary::from_bytes(data) {
        let _ = dictionary::register(&dict);
        debug_assert!(dictionary::registered(&dict.id()));
        let _ = dictionary::lookup(&dict.id());
        // A machine-9 core that references this dictionary, parsed from the tail
        // of the input (arbitrary, so mostly forged).
        let _ = dict.decode(data);
        let _ = dict.encode_record(&data[..data.len().min(64)]);
    }

    // Resolving an unregistered id must be a cheap `None`, never a panic.
    let _ = dictionary::lookup(&[0u8; 16]);
});
