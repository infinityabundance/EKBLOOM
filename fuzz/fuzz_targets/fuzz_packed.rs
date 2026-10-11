#![no_main]
//! Fuzz the packed 16-bit restart/history parsers and the `state_from_hot`
//! reconstruction.
//!
//! Arbitrary bytes must never panic. A shape that parses must round-trip, and a
//! packed structure built over a fuzz-derived core must equal the authoritative
//! `state_log` for every position and stride.

use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::packed::{state_from_hot, PackedHistory, PackedRestarts};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Raw bytes into both parsers: never panic; an accepted parse is
    //    self-consistent and re-serialises to itself.
    let _ = PackedHistory::from_bytes(data);
    if let Ok(v) = PackedHistory::from_bytes(data) {
        let _ = v.state_at(0);
        let _ = v.state_at(u64::MAX);
        let _ = v.physical_bytes();
        assert_eq!(PackedHistory::from_bytes(&v.to_bytes()).unwrap(), v);
    }
    if let Ok(v) = PackedRestarts::from_bytes(data) {
        let _ = v.position_of(0);
        let _ = v.position_of(u64::MAX);
        let _ = v.snapshot_at(&[], 0);
        let _ = v.max_replay_steps();
        assert_eq!(PackedRestarts::from_bytes(&v.to_bytes()).unwrap(), v);
    }

    // 2. Build over a bounded fuzz-derived core and cross-check exactness.
    if data.len() < 2 {
        return;
    }
    let n = (data.len().min(48) - 1) as u64;
    let src = &data[1..1 + n as usize];
    let core = match NL2.encode(src) {
        Ok(c) => c,
        Err(_) => return,
    };
    let log = match state_log(&core, n) {
        Ok(l) => l,
        Err(_) => return,
    };
    let hot = match NL2.decode(&core, n) {
        Ok(h) => h,
        Err(_) => return,
    };

    let ph = match PackedHistory::build(&core, n) {
        Ok(p) => p,
        Err(_) => return,
    };
    for (i, &x) in log.iter().enumerate() {
        assert_eq!(ph.state_at(i as u64), Some(x as u16));
        assert_eq!(state_from_hot(&hot, i as u64), Some(x as u16));
    }

    for sh in [0u8, 1, 2, 3, 5, 8, 16, 48, 63] {
        let pr = match PackedRestarts::build(&core, n, sh) {
            Ok(p) => p,
            Err(_) => continue,
        };
        for (i, &x) in log.iter().enumerate() {
            let snap = pr.snapshot_at(&core, i as u64).unwrap();
            assert_eq!(snap, ekbloom::Snapshot::new(x, i as u64));
        }
        assert!(pr.snapshot_at(&core, n + 1).is_err());
    }
});
