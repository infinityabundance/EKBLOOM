//! Property-based tests (directive §21) using `proptest`.
//!
//! Properties:
//!   * machine roundtrip: arbitrary `Vec<u8>` encode then decode is identity;
//!   * `read(body, {offset,length})` equals the exact source slice for arbitrary
//!     in-bounds requests, and out-of-bounds requests return an error (never
//!     panic);
//!   * `CheckpointSet::restore(core, i)` equals `state_log[i]` for arbitrary `i`
//!     and interval;
//!   * `PathBloom::symbol_at(pos)` equals the authoritative decode for arbitrary
//!     `pos`;
//!   * `History` / `CheckpointSet` / `BloomShell` byte roundtrips for arbitrary
//!     valid structures.
//!
//! A small deterministic case count keeps the runtime bounded.

use ekbloom::bloom::{BloomShell, SUPPORTED_DEPTHS};
use ekbloom::bloom_path::PathBloom;
use ekbloom::checkpoint::{CheckpointSet, SUPPORTED_INTERVALS};
use ekbloom::history::History;
use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::state::Snapshot;
use ekbloom::{create, read, ManifestationProfile, ReadRequest};
use proptest::collection::vec;
use proptest::prelude::*;

/// Deterministic, bounded proptest configuration.
fn cfg() -> ProptestConfig {
    ProptestConfig {
        cases: 48,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(cfg())]

    /// `decode(encode(src)) == src` for arbitrary bytes.
    #[test]
    fn machine_roundtrip(src in vec(any::<u8>(), 0..65_536)) {
        let core = NL2.encode(&src).unwrap();
        let decoded = NL2.decode(&core, src.len() as u64).unwrap();
        prop_assert_eq!(decoded, src);
    }

    /// In-bounds `read` returns the exact source slice for arbitrary requests.
    #[test]
    fn read_in_bounds_matches_source(
        src in vec(any::<u8>(), 0..4096),
        offset_frac in 0.0f64..1.0,
        len_frac in 0.0f64..1.0,
    ) {
        let body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
        let n = src.len() as u64;
        let offset = if n == 0 { 0 } else { ((offset_frac * n as f64) as u64) % n };
        let remaining = n - offset;
        let length = if remaining == 0 {
            0
        } else {
            ((len_frac * remaining as f64) as u64) % (remaining + 1)
        };
        let got = read(&body, ReadRequest { offset, length }).unwrap();
        prop_assert_eq!(got, src[offset as usize..(offset + length) as usize].to_vec());
    }

    /// Out-of-bounds and overflowing requests error, never panic.
    #[test]
    fn read_out_of_bounds_errors(
        src in vec(any::<u8>(), 0..2048),
        extra in 1u64..1024,
    ) {
        let body = create(&src, ManifestationProfile::Root).unwrap();
        let n = src.len() as u64;
        let past_end = ReadRequest { offset: n, length: extra };
        let overflowing = ReadRequest {
            offset: u64::MAX,
            length: 2,
        };
        prop_assert!(read(&body, past_end).is_err());
        prop_assert!(read(&body, overflowing).is_err());
    }

    /// Checkpoint restart equals the authoritative state log.
    #[test]
    fn checkpoint_restore_matches_state_log(
        src in vec(any::<u8>(), 0..512),
        k_idx in 0usize..SUPPORTED_INTERVALS.len(),
        i_frac in 0.0f64..1.0,
    ) {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let k = SUPPORTED_INTERVALS[k_idx];
        let cps = CheckpointSet::build(&core, n, k).unwrap();
        let logs = state_log(&core, n).unwrap();
        let i = if n == 0 { 0 } else { ((i_frac * n as f64) as u64) % (n + 1) };
        prop_assert_eq!(
            cps.restore(&core, i).unwrap(),
            Snapshot::new(logs[i as usize], i)
        );
    }

    /// Path-bloom symbol lookup equals the authoritative decode.
    #[test]
    fn path_symbol_at_matches_decode(
        src in vec(any::<u8>(), 1..1024),
        d_idx in 0usize..SUPPORTED_DEPTHS.len(),
        pos_frac in 0.0f64..1.0,
    ) {
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let d = SUPPORTED_DEPTHS[d_idx];
        let pb = PathBloom::from_shell(&PathBloom::build(&core, n, d, 0).unwrap()).unwrap();
        let decoded = NL2.decode(&core, n).unwrap();
        let pos = ((pos_frac * n as f64) as u64) % n;
        prop_assert_eq!(pb.symbol_at(pos).unwrap(), decoded[pos as usize]);
        prop_assert!(pb.symbol_at(n).is_err());
    }

    /// `History` byte roundtrip for arbitrary valid structures.
    #[test]
    fn history_byte_roundtrip(src in vec(any::<u8>(), 0..1024)) {
        let core = NL2.encode(&src).unwrap();
        let history = History::build(&core, src.len() as u64, 3).unwrap();
        let bytes = history.to_bytes();
        prop_assert_eq!(History::from_bytes(&bytes).unwrap(), history);
    }

    /// `CheckpointSet` byte roundtrip for arbitrary valid structures.
    #[test]
    fn checkpoint_byte_roundtrip(
        src in vec(any::<u8>(), 0..1024),
        k_idx in 0usize..SUPPORTED_INTERVALS.len(),
    ) {
        let core = NL2.encode(&src).unwrap();
        let k = SUPPORTED_INTERVALS[k_idx];
        let cps = CheckpointSet::build(&core, src.len() as u64, k).unwrap();
        let bytes = cps.to_bytes();
        prop_assert_eq!(CheckpointSet::from_bytes(&bytes).unwrap(), cps);
    }

    /// `BloomShell` byte roundtrip for arbitrary valid structures.
    #[test]
    fn bloom_shell_byte_roundtrip(
        src in vec(any::<u8>(), 0..512),
        d_idx in 0usize..SUPPORTED_DEPTHS.len(),
    ) {
        let core = NL2.encode(&src).unwrap();
        let d = SUPPORTED_DEPTHS[d_idx];
        let shell = PathBloom::build(&core, src.len() as u64, d, 7).unwrap();
        let bytes = shell.to_bytes();
        let back = BloomShell::from_bytes(&bytes).unwrap();
        prop_assert_eq!(back, shell);
    }
}
