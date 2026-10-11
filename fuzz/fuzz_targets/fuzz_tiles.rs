#![no_main]
//! Fuzz the persisted tile-set parsers (`parse_tile_set_prefix`,
//! `TileSet::from_bytes`) and the tile read path.
//!
//! Arbitrary bytes must never panic (in particular no shift-overflow from an
//! unvalidated `tile_shift`). A mutated but CRC-valid payload must be a clean
//! `Err`, and a real tile set must read byte-exactly.

use ekbloom::{
    create, parse_tile_set_prefix, AccessTrace, DirectoryKind, ManifestationBudget,
    ManifestationProfile, TileSet,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes: prefix parser + full parser must never panic.
    let _ = parse_tile_set_prefix(data);
    if let Ok(ts) = TileSet::from_bytes(data) {
        let _ = ts.verify();
        let _ = ts.verify_against(&[]);
        let _ = ts.tile_bytes_at(0);
        let _ = ts.tile_bytes_at(u64::MAX);
        let _ = ts.read(&[], 0, 0);
        let _ = ts.read(&[], 0, 1);
    }

    // 2. A bounded, valid tile set exercises the read path on a real directory.
    let m = data.len().min(300);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let mut body = match create(src, ManifestationProfile::Root) {
        Ok(b) => b,
        Err(_) => return,
    };
    let shift = 1 + (data[0] % 10); // 1..=10
    let kind = if data[0] & 1 == 0 {
        DirectoryKind::Flat
    } else {
        DirectoryKind::TwoLevelRadix
    };
    let trace = AccessTrace {
        regions: vec![(0, m as u64)],
        counts: vec![1],
    };
    if TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        shift,
        kind,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .is_err()
    {
        return;
    }
    let Some(ts) = body.tiles.last() else {
        return;
    };
    let bytes = ts.to_bytes();
    assert!(TileSet::from_bytes(&bytes).is_ok());

    // A mutated copy (digest no longer matches) must be a clean `Err`.
    let mut mutated = bytes.clone();
    for (k, &b) in data.iter().enumerate().take(bytes.len()) {
        mutated[k] ^= b;
    }
    let _ = TileSet::from_bytes(&mutated);

    // Reads derived from the input, within bounds.
    for w in data.chunks(2).take(32) {
        if w.len() == 2 {
            let off = u64::from(w[0]) * (m as u64 / 256 + 1);
            let len = u64::from(w[1]) % 64;
            if off.checked_add(len).map_or(false, |e| e <= m as u64) {
                let got = ts.read(&body.core, off, len).unwrap();
                assert_eq!(got, &src[off as usize..(off + len) as usize]);
            }
        }
    }
});
