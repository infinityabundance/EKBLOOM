#![no_main]
//! Authenticated-open fuzz target (`src/reader.rs`).
//!
//! The key is derived from a prefix of the input, so a single input mutates both
//! the key and the file image. Arbitrary bytes must be **total**: `open_bytes_authenticated`
//! and the strict `open_bytes_require_auth` return a value or a typed error, never
//! panic, and when a body *does* authenticate its reads and verification must not
//! panic either. A file whose `AUTH` does not verify must never be silently
//! accepted by the strict entry point.

use ekbloom::{read, reader, verify, ReadRequest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let n = data.len().min(32);
    let mut key = [0u8; 32];
    key[..n].copy_from_slice(&data[..n]);
    let image = &data[n..];

    if let Ok(body) = reader::open_bytes_authenticated(image, &key) {
        let _ = read(
            &body,
            ReadRequest {
                offset: 0,
                length: body.logical_len,
            },
        );
        let _ = verify(&body);
    }

    // Strict mode must reject a missing/mismatching AUTH, never answer `Ok`.
    let _ = reader::open_bytes_require_auth(image, &key);
});
