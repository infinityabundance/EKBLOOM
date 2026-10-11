//! Selective hot-region byte tiles (`src/tiles.rs`).
//!
//! These tests pin the contract: tile reads (including boundary-crossing) are
//! byte-exact against the authoritative source for materialised and
//! non-materialised ranges; directory lookup is correct for both `Flat` and
//! `TwoLevelRadix` (including empty top-level groups); certification detects a
//! tampered tile byte; the [`ManifestationBudget`] hard cap is respected; the
//! manifestation is additive (earlier generations stay byte-identical and still
//! verify); power-of-two geometry is exact for every tile size; and the tile set
//! survives a byte roundtrip.

use ekbloom::{
    create, read, verify_deep, AccessTrace, DirectoryKind, EkbloomError, ManifestationBudget,
    ManifestationProfile, ReadRequest, TileSet,
};

/// Deterministic LCG corpus (identical generator to the other harnesses).
fn lcg(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((s >> 33) as u8);
    }
    out
}

/// `count` contiguous `len`-byte hot regions starting at 0, counts decreasing.
fn hot_trace(region_len: u64, count: u64) -> AccessTrace {
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    for k in 0..count {
        regions.push((k * region_len, region_len));
        counts.push(10_000u64.saturating_sub(k));
    }
    AccessTrace { regions, counts }
}

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn assert_reads_match(body: &ekbloom::EkbloomBody, src: &[u8], off: u64, len: u64) {
    let tiles = body.tiles.last().expect("a tile set");
    let got = tiles.read(&body.core, off, len).unwrap();
    assert_eq!(
        got,
        &src[off as usize..(off + len) as usize],
        "tile read ({off},{len}) must be byte-exact"
    );
    // And identical to the authoritative reader.
    assert_eq!(
        got,
        read(
            body,
            ReadRequest {
                offset: off,
                length: len
            }
        )
        .unwrap()
    );
}

#[test]
fn hot_reads_are_byte_exact_including_boundary_crossings() {
    let src = lcg(0x7111_0000_0000_0001, 256 * 1024);
    let n = src.len() as u64;
    // Both tile 0 and tile 1 (at 4 KiB) are requested, so a read that crosses the
    // 4 KiB boundary is served from two materialised tiles.
    let trace = AccessTrace {
        regions: vec![(0, 1024), (1024, 1024), (2048, 1024)],
        counts: vec![1000, 900, 800],
    };
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let report =
        TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous())
            .unwrap();
    assert!(report.tiles_selected >= 1);
    assert!(report.certified);
    assert!(body
        .tiles
        .last()
        .unwrap()
        .verify_against(&body.core)
        .unwrap());

    // Inside one tile, straddling a tile boundary, and at the object ends.
    for (off, len) in [
        (0u64, 1u64),
        (4095, 2),
        (4096 - 100, 300),
        (1000, 100),
        (8192, 4096),
        (n - 5, 5),
    ] {
        assert_reads_match(&body, &src, off, len);
    }
}

#[test]
fn non_materialised_ranges_reconstruct_byte_exactly() {
    let src = lcg(0x7111_0000_0000_0002, 128 * 1024);
    let n = src.len() as u64;
    // Only the very front is hot; everything else is absent and reconstructed.
    let trace = AccessTrace {
        regions: vec![(0, 4096)],
        counts: vec![1000],
    };
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )
    .unwrap();

    // A read that starts in the materialised tile and runs into the absent one.
    assert_reads_match(&body, &src, 4096 - 8, 64);
    // Fully cold ranges (single bytes and ranges).
    for (off, len) in [(n / 2, 1u64), (n - 1, 1), (n - 128, 128), (60_000, 77)] {
        assert_reads_match(&body, &src, off, len);
    }
}

#[test]
fn directory_lookup_is_correct_for_both_kinds_and_empty_groups() {
    // 256 KiB, 1 KiB tiles => 256 tiles => four 64-tile radix groups. Hot regions
    // in groups 0 and 3 leave group 2 empty.
    let src = lcg(0x7111_0000_0000_0003, 256 * 1024);
    let hi = 200 * 1024u64;
    let trace = AccessTrace {
        regions: vec![(0, 1024), (1024, 1024), (hi, 1024), (hi + 1024, 1024)],
        counts: vec![1000, 900, 800, 700],
    };
    for kind in [DirectoryKind::Flat, DirectoryKind::TwoLevelRadix] {
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        let report = TileSet::manifest_hot_tiles(&mut body, &trace, 10, kind, &generous()).unwrap();
        let tiles = body.tiles.last().unwrap();
        assert_eq!(tiles.directory_kind(), kind);
        assert_eq!(tiles.tile_size(), 1024);
        assert_eq!(tiles.tiles_total(), 256);
        assert_eq!(tiles.present_tiles(), report.tiles_selected);
        assert_eq!(tiles.directory_bytes(), report.directory_bytes);

        // Materialised tiles return exactly the source bytes.
        for t in [0u64, 1, (hi >> 10), (hi >> 10) + 1] {
            let bytes = tiles.tile_bytes_at(t).expect("materialised tile");
            let start = (t * 1024) as usize;
            assert_eq!(bytes, &src[start..start + 1024]);
        }
        // A tile in the untouched middle group (tile 100 => group 1) is absent.
        assert!(tiles.tile_bytes_at(100).is_none());
        assert!(tiles.tile_bytes_at(255).is_none());
        assert!(tiles.tile_bytes_at(256).is_none());
    }
}

#[test]
fn radix_directory_is_smaller_than_flat_when_sparse() {
    // 1 MiB, 4 KiB tiles => 256 tiles; only a small contiguous hot run. The flat
    // directory pays 4 B for every tile; the radix directory pays only for the
    // groups it touches.
    let src = lcg(0x7111_0000_0000_0004, 1024 * 1024);
    let trace = AccessTrace {
        regions: (0..16).map(|k| (k * 4096, 4096)).collect(),
        counts: (0..16u64).map(|k| 1000 - k).collect(),
    };
    let mut flat_body = create(&src, ManifestationProfile::Root).unwrap();
    let flat =
        TileSet::manifest_hot_tiles(&mut flat_body, &trace, 12, DirectoryKind::Flat, &generous())
            .unwrap();
    let mut radix_body = create(&src, ManifestationProfile::Root).unwrap();
    let radix = TileSet::manifest_hot_tiles(
        &mut radix_body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &generous(),
    )
    .unwrap();
    assert_eq!(flat.tiles_selected, 16);
    assert_eq!(radix.tiles_selected, 16);
    assert_eq!(flat.directory_bytes, 256 * 4, "flat pays per tile");
    assert!(
        radix.directory_bytes < flat.directory_bytes,
        "radix {} should beat flat {}",
        radix.directory_bytes,
        flat.directory_bytes
    );
}

#[test]
fn tampered_tile_byte_is_detected() {
    let src = lcg(0x7111_0000_0000_0005, 64 * 1024);
    let trace = hot_trace(64, 16);
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    let tiles = body.tiles.last().unwrap();

    // Pristine: the digest holds and the tiles certify.
    assert!(tiles.verify());
    assert!(tiles.verify_against(&body.core).unwrap());

    // A byte roundtrip is exact.
    let bytes = tiles.to_bytes();
    let back = TileSet::from_bytes(&bytes).unwrap();
    assert_eq!(&back, tiles);
    assert!(back.verify_against(&body.core).unwrap());

    // Flip the final byte (inside the tile blob): deserialisation must reject it.
    let mut tampered = bytes.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xFF;
    assert!(matches!(
        TileSet::from_bytes(&tampered),
        Err(EkbloomError::BadSection { .. })
    ));

    // Certification against a different core fails.
    let other = lcg(0x7111_0000_0000_0006, 64 * 1024);
    let other_body = create(&other, ManifestationProfile::Root).unwrap();
    assert!(!tiles.verify_against(&other_body.core).unwrap());
}

#[test]
fn budget_is_respected_and_a_no_fit_leaves_the_body_untouched() {
    let src = lcg(0x7111_0000_0000_0007, 256 * 1024);
    let trace = hot_trace(64, 256);

    // A budget with room for a few tiles: the committed body fits, and at least
    // one but not all tiles are materialised.
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let base = body.physical_bytes();
    let budget = ManifestationBudget {
        maximum_physical_bytes: base + 8 * 1024,
    };
    let report =
        TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &budget).unwrap();
    assert!(report.after_physical_bytes <= budget.maximum_physical_bytes);
    assert_eq!(body.physical_bytes(), report.after_physical_bytes);
    assert!(report.tiles_selected > 0);
    assert!(report.tiles_selected < report.tiles_requested);
    assert!(verify_deep(&body).unwrap().ok);

    // No remaining allowance: nothing fits, the body is byte-for-byte unchanged.
    let mut tight = create(&src, ManifestationProfile::Root).unwrap();
    let snapshot = tight.clone();
    let zero_room = ManifestationBudget {
        maximum_physical_bytes: tight.physical_bytes(),
    };
    let report =
        TileSet::manifest_hot_tiles(&mut tight, &trace, 12, DirectoryKind::Flat, &zero_room)
            .unwrap();
    assert_eq!(report.tiles_selected, 0);
    assert_eq!(report.added_physical_bytes, 0);
    assert_eq!(report.new_generation, report.before_generation);
    assert_eq!(tight, snapshot, "a no-fit call must not touch the body");
}

#[test]
fn manifestation_is_additive_and_earlier_generations_stay_intact() {
    let src = lcg(0x7111_0000_0000_0008, 512 * 1024);

    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let before_generation = body.generation;
    let before_physical = body.physical_bytes();

    // First manifestation: a hot run near the front.
    let first = AccessTrace {
        regions: vec![(0, 4096)],
        counts: vec![1000],
    };
    let r1 = TileSet::manifest_hot_tiles(&mut body, &first, 12, DirectoryKind::Flat, &generous())
        .unwrap();
    let first_tiles = body.tiles.first().cloned().expect("first tile set");
    let mid_generation = body.generation;
    let mid_physical = body.physical_bytes();
    assert_eq!(r1.new_generation, before_generation + 1);
    assert_eq!(mid_physical, before_physical + r1.added_physical_bytes);

    // Second manifestation, disjoint hot run deeper in the object.
    let second = AccessTrace {
        regions: vec![(400 * 1024, 4096)],
        counts: vec![500],
    };
    let r2 = TileSet::manifest_hot_tiles(&mut body, &second, 12, DirectoryKind::Flat, &generous())
        .unwrap();
    assert_eq!(r2.new_generation, mid_generation + 1);
    assert_eq!(r2.before_physical_bytes, mid_physical);
    assert_eq!(
        body.physical_bytes(),
        mid_physical + r2.added_physical_bytes
    );

    // Additive: the first tile set is byte-identical and still certifies, the
    // second was appended, and both generations verify.
    assert_eq!(body.tiles.len(), 2);
    assert_eq!(body.tiles[0], first_tiles);
    assert!(body.tiles[0].verify_against(&body.core).unwrap());
    assert!(body.tiles[1].verify_against(&body.core).unwrap());
    assert!(verify_deep(&body).unwrap().ok);

    // Both tile sets read byte-exactly over their own hot runs.
    for tiles in &body.tiles {
        for &(off, len) in &[(0u64, 4096u64), (400 * 1024, 4096)] {
            let got = tiles.read(&body.core, off, len).unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
        }
    }
}

#[test]
fn power_of_two_lookups_are_exact_for_every_tile_size() {
    let src = lcg(0x7111_0000_0000_0009, 192 * 1024);
    let n = src.len() as u64;
    let trace = hot_trace(512, 64); // a 32 KiB hot run

    for shift in [10u8, 12, 14, 16] {
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        let report = TileSet::manifest_hot_tiles(
            &mut body,
            &trace,
            shift,
            DirectoryKind::TwoLevelRadix,
            &generous(),
        )
        .unwrap();
        let tiles = body.tiles.last().unwrap();
        let size = 1u64 << shift;
        assert_eq!(tiles.tile_size(), size);
        assert_eq!(tiles.tiles_total(), n.div_ceil(size));
        assert!(report.certified);

        // Reads aligned to, straddling, and inside tile boundaries are exact.
        for (off, len) in [
            (0u64, 1u64),
            (size - 1, 2),
            (size, 1),
            (size - 5, 10),
            (3 * size - 1, 3),
            (n - 1, 1),
        ] {
            if off + len <= n {
                assert_reads_match(&body, &src, off, len);
            }
        }
    }
}

#[test]
fn tile_set_byte_roundtrip_roundtrips_geometry_and_data() {
    let src = lcg(0x7111_0000_0000_000A, 256 * 1024);
    let trace = hot_trace(64, 128);
    for kind in [DirectoryKind::Flat, DirectoryKind::TwoLevelRadix] {
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        TileSet::manifest_hot_tiles(&mut body, &trace, 12, kind, &generous()).unwrap();
        let tiles = body.tiles.last().unwrap();

        let bytes = tiles.to_bytes();
        let back = TileSet::from_bytes(&bytes).unwrap();
        assert_eq!(&back, tiles);
        assert_eq!(back.tile_shift, tiles.tile_shift);
        assert_eq!(back.tile_size(), tiles.tile_size());
        assert_eq!(back.tiles_total(), tiles.tiles_total());
        assert_eq!(back.directory_kind(), kind);
        assert_eq!(back.physical_bytes(), tiles.physical_bytes());
        assert!(back.verify());
        assert!(back.verify_against(&body.core).unwrap());

        // Reads through the parsed copy match the source exactly.
        for (off, len) in [(0u64, 4096u64), (4094, 4), (n() - 7, 7)] {
            let got = back.read(&body.core, off, len).unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
        }

        // Truncating the blob is rejected, not silently accepted.
        let mut short = bytes.clone();
        short.truncate(short.len() - 1);
        assert!(TileSet::from_bytes(&short).is_err());
    }
}

/// The corpus length used by the roundtrip test.
fn n() -> u64 {
    256 * 1024
}
