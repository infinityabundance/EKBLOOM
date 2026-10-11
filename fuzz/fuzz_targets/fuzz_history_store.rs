#![no_main]
//! Drive the persistent segmented history store and the `HISTORY_SEGMENTED`
//! `.ekb` section from raw bytes: build a store over a fuzz-derived core, embed
//! it, then corrupt the section payload (header, index entries and states) with
//! the section CRC-32 recomputed so the parser is actually reached, and query
//! state positions out of range. Every path must return a value or a clean
//! `Err` — never panic, never read out of bounds, never allocate from a
//! declaration.
//!
//! The CRC-32 is implemented inline (IEEE, zlib polynomial) so the target does
//! not depend on a crate-internal dependency. All lengths are bounded so a fuzz
//! input can never drive an unbounded build.

use ekbloom::{write_history_segmented, FileBody, HistoryStore, OrMachine, StoreConfig, NL2};
use libfuzzer_sys::fuzz_target;

const HS_FIXED_LEN: usize = 104;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn align8(x: u64) -> u64 {
    (x + 7) & !7
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }

    // Bounded build parameters.
    let n = (data[0] as usize) % 40; // 0..=39 logical symbols
    let segs = [4u32, 8, 16, 32, 64, 128, 1024];
    let seg = segs[(data[1] as usize) % segs.len()];
    let intervals = [1u32, 4, 16, 64, 1024, 65536];
    let interval = intervals[(data[2] as usize) % intervals.len()];

    let src: Vec<u8> = (0..n).map(|i| data[(3 + i) % data.len()]).collect();
    let core = match NL2.encode(&src) {
        Ok(c) => c,
        Err(_) => return,
    };

    let dir = std::env::temp_dir().join(format!("ekbloom-fuzz-hs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let store = match HistoryStore::build(
        &dir.join("segs"),
        |p| core[p as usize],
        n as u64,
        StoreConfig { segment_bytes: seg },
        interval,
    ) {
        Ok(s) => s,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };

    // Round-trip: embed the store and query every position plus out-of-range ones.
    let path = dir.join("a.ekb");
    if write_history_segmented(&path, &core, &store).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    if let Ok(fb) = FileBody::open(&path) {
        let _ = fb.segmented_state_at(n as u64);
        for &b in data.iter().take(32) {
            let i = (b as u64) % (n as u64 + 2);
            let _ = fb.segmented_state_at(i);
        }
        let _ = fb.segmented_state_at(u64::MAX);
        let _ = fb.segmented_state_at(u64::MAX - 1);
        for i in 0..=n as u64 {
            let _ = fb.segmented_state_at(i);
        }
    }

    // Corrupt the segmented payload (header, index and states) with the fuzz
    // bytes and recompute the section CRC so the streaming parser is reached.
    if let Ok(mut bytes) = std::fs::read(&path) {
        let base = align8(80 + core.len() as u64) as usize;
        if base + 40 <= bytes.len() {
            let payload_off =
                u64::from_le_bytes(bytes[base + 8..base + 16].try_into().unwrap()) as usize;
            let payload_len =
                u64::from_le_bytes(bytes[base + 16..base + 24].try_into().unwrap()) as usize;
            if payload_len >= HS_FIXED_LEN && payload_off + payload_len <= bytes.len() {
                for (k, &b) in data.iter().enumerate() {
                    let pos =
                        payload_off + (k.wrapping_mul(7).wrapping_add(b as usize)) % payload_len;
                    bytes[pos] ^= b;
                }
                let crc = crc32(&bytes[payload_off..payload_off + payload_len]);
                bytes[base + 32..base + 36].copy_from_slice(&crc.to_le_bytes());
                let p2 = dir.join("b.ekb");
                if std::fs::write(&p2, &bytes).is_ok() {
                    if let Ok(fb) = FileBody::open(&p2) {
                        for &b in data.iter().take(64) {
                            let _ = fb.segmented_state_at((b as u64) % (n as u64 + 3));
                        }
                        for i in 0..=(n as u64 + 1) {
                            let _ = fb.segmented_state_at(i);
                        }
                    }
                }
            }
        }
    }

    // Corrupt the on-disk segment files and query the store: a truncated or
    // zeroed segment must be a clean error, never a panic.
    let sp = dir.join("segs").join("seg-00000000.bin");
    if let Ok(d) = std::fs::read(&sp) {
        if !d.is_empty() {
            let cut = d.len() / 2;
            let _ = std::fs::write(&sp, &d[..cut]);
        }
        let _ = store.state_at(0);
        let _ = store.state_at(n as u64);
        let _ = store.read_segment(0);
        let _ = store.read_segment(usize::MAX);
    }

    let _ = std::fs::remove_dir_all(&dir);
});
