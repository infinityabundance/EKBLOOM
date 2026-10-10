#![no_main]
//! Feed arbitrary bytes to the `.ekb` open/corruption pathway. Random bytes must
//! return `Err` (never panic). If a crafted image *does* parse, subsequent
//! in-memory reads and verification must not panic either.

use ekbloom::{read, reader, verify, ReadRequest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(body) = reader::open_bytes(data) {
        // Fallible and bounds-checked; must not panic.
        let _ = read(
            &body,
            ReadRequest {
                offset: 0,
                length: body.logical_len,
            },
        );
        let _ = verify(&body);
    }
});
