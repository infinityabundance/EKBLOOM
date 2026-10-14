#![no_main]
//! Malformed declared lengths — huge / overflowing counts must be typed errors.
//!
//! Every structure on the read path carries a declared count or byte length
//! (`GenesisHeader.core_len`, a section header's `length`, a history `count` ==
//! `logical_len + 1`, a checkpoint `count`, a tile/block directory length, a
//! dictionary `dict_len`). A forged declaration of `u64::MAX` must **never**
//! directly size an allocation: the parser must answer with a typed error
//! ([`ekbloom::EkbloomError::Overflow`] / `Truncated` / `AllocationTooLarge` /
//! `BadSection`), never OOM, never panic. This target drives the whole `.ekb`
//! scan plus each raw structure parser from arbitrary bytes (the fuzzer mutates
//! the declarations in place), and pins the resource-policy boundary: one byte
//! past the (deliberately tiny) ceiling is refused, the ceiling itself is legal.

use ekbloom::blocks::{parse_block_index_prefix, BlockIndex};
use ekbloom::checkpoint::CheckpointSet;
use ekbloom::format;
use ekbloom::history::History;
use ekbloom::machine::{machine_by_id, MACHINE_DICT};
use ekbloom::resource::{checked_default, ResourcePolicyGuard};
use ekbloom::tiles::{parse_tile_set_prefix, TileSet};
use ekbloom::{reader, EkbloomError};
use libfuzzer_sys::fuzz_target;

/// A small ceiling so a forged declaration that would be "legal" under the
/// production 1 GiB cap cannot dominate the campaign with a real allocation.
const FUZZ_CAP: u64 = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let _guard = ResourcePolicyGuard::set(FUZZ_CAP);

    // 1. The whole `.ekb` open path: header `core_len`, section lengths, footer.
    let _ = reader::open_bytes(data);
    let _ = format::scan(data);

    // 2. Each raw structure parser, whose header bytes the fuzzer owns. A forged
    //    `u64::MAX` count/length must be an error before any allocation.
    let _ = History::from_bytes(data);
    let _ = parse_block_index_prefix(data);
    let _ = BlockIndex::from_bytes(data);
    let _ = parse_tile_set_prefix(data);
    let _ = TileSet::from_bytes(data);
    let _ = CheckpointSet::from_bytes(data);
    for id in 1..=9u32 {
        if let Some(m) = machine_by_id(id) {
            let _ = CheckpointSet::from_bytes_with(m, data);
        }
    }
    // Machine 9 carries a `dict_id` ++ `dict_len` frame; a huge `dict_len` (up to
    // `u32::MAX`) must stay a typed error.
    let _ = MACHINE_DICT;
    let _ = ekbloom::machine_dict::DictContext::from_core(data);

    // 3. The boundary is exactly the ceiling (inclusive at the cap, over is
    //    refused), and the refusal is the typed `AllocationTooLarge`.
    assert!(checked_default(FUZZ_CAP).is_ok());
    assert!(matches!(
        checked_default(FUZZ_CAP + 1),
        Err(EkbloomError::AllocationTooLarge { .. })
    ));
    assert!(checked_default(u64::MAX).is_err());
});
