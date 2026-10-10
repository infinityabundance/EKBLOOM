#![no_main]
//! Construct a [`BloomShell`] from arbitrary bytes and feed it to the strict
//! `PathBloom` / `GlobalBloom` / `ContextBloom` parsers. Must never panic.

use ekbloom::bloom::{BloomKind, BloomShell, Certification};
use ekbloom::bloom_context::ContextBloom;
use ekbloom::bloom_global::GlobalBloom;
use ekbloom::bloom_path::PathBloom;
use libfuzzer_sys::fuzz_target;

fn kind_of(tag: u8) -> BloomKind {
    match tag % 4 {
        0 => BloomKind::Universal,
        1 => BloomKind::Contextual,
        2 => BloomKind::Regional,
        _ => BloomKind::Path,
    }
}

fn status_of(tag: u8) -> Certification {
    match tag % 3 {
        0 => Certification::Unverified,
        1 => Certification::Certified,
        _ => Certification::Rejected,
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 12 {
        return;
    }

    let kind = kind_of(data[0]);
    let status = status_of(data[1]);
    let depth = u16::from_le_bytes([data[2], data[3]]);
    let logical_len = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let entries = if data.len() >= 20 {
        u64::from_le_bytes(data[12..20].try_into().unwrap())
    } else {
        0
    };
    let payload = if data.len() > 20 {
        data[20..].to_vec()
    } else {
        Vec::new()
    };
    let core_hash = [0u8; 32];

    let shell = BloomShell {
        kind,
        depth,
        generation: 0,
        logical_len,
        entries,
        payload,
        status,
        core_hash,
    };

    // Each parser validates kind/depth/length strictly and returns a Result.
    let _ = PathBloom::from_shell(&shell);
    let _ = GlobalBloom::from_shell(&shell);
    let _ = ContextBloom::from_shell(&shell);
});
