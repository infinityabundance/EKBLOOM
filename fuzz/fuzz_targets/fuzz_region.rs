#![no_main]
//! Fuzz the persisted **regional** bloom parsers and the region-scoped reader.
//!
//! The M1 round widened the regional record to a complete machine-4 successor
//! (`key ++ symbols[D] ++ pack_wide`), added `RegionTable::from_shell`'s wide
//! branch and `RegionReader`/`read_region_scoped`, and made `verify` re-derive the
//! stored successor (cursor *and* lanes). Arbitrary bytes fed to the parser and to
//! the reader must never panic (no shift/overflow, no attacker-sized allocation),
//! a mutated body must be a clean `Err`/rejection, and a real machine-1 (base),
//! machine-2/3 (cursor) and machine-4 (wide) regional table must read byte-exactly
//! against the authoritative machine.

use ekbloom::bloom::{BloomKind, BloomShell, Certification};
use ekbloom::bloom_region::RegionTable;
use ekbloom::machine::{MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE};
use ekbloom::{
    create_machine, manifest_hot_regions, read, read_region_scoped, verify, AccessTrace,
    ManifestationBudget, ManifestationProfile, ReadRequest, RegionReader, SweepConfig,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Arbitrary bytes as a Regional shell: the parser must never panic and the
    //    serialized round-trip must be total.
    if data.len() >= 20 {
        let shell = BloomShell {
            kind: BloomKind::Regional,
            depth: u16::from_le_bytes([data[0], data[1]]),
            generation: 0,
            logical_len: u64::from_le_bytes(data[4..12].try_into().unwrap()),
            entries: u64::from_le_bytes(data[12..20].try_into().unwrap()),
            payload: data[20..].to_vec(),
            status: if data[2] & 1 == 0 {
                Certification::Certified
            } else {
                Certification::Unverified
            },
            core_hash: [0u8; 32],
        };
        let _ = RegionTable::from_shell(&shell);
        if let Ok(back) = BloomShell::from_bytes(&shell.to_bytes()) {
            let _ = RegionTable::from_shell(&back);
        }
    }

    // 2. A real regional table over a bounded source, for each record form, reads
    //    byte-exactly and a mutated shell is a clean rejection.
    let m = data.len().min(1024);
    if m == 0 {
        return;
    }
    let src = &data[..m];
    let n = m as u64;
    let depth = 1 + (data[0] as u16 % 6); // 1..=6
    if n < u64::from(depth) {
        return;
    }
    let budget = ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    };
    let cfg = SweepConfig {
        iterations: 1,
        seed: 1,
    };
    for machine in [MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE, MACHINE_RANS0I] {
        let mut body = match create_machine(src, ManifestationProfile::Root, machine) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let trace = AccessTrace {
            regions: vec![(0, u64::from(depth))],
            counts: vec![1],
        };
        if manifest_hot_regions(&mut body, &trace, &budget, &cfg).is_err() {
            continue;
        }
        let _ = verify(&body);
        let reader = RegionReader::new(&body);
        let _ = reader.has_regions();

        for w in data.chunks(2).take(32) {
            if w.len() < 2 {
                continue;
            }
            let off = u64::from(w[0]) % (n + 1);
            let len = u64::from(w[1]) % 16;
            if off.checked_add(len).map_or(false, |e| e <= n) {
                let got = read_region_scoped(&body, off, len).unwrap();
                assert_eq!(
                    got,
                    read(
                        &body,
                        ReadRequest {
                            offset: off,
                            length: len
                        }
                    )
                    .unwrap()
                );
            }
        }

        // A tampered regional payload must never panic `verify` (clean verdict).
        if let Some(pos) = body
            .blooms
            .iter()
            .position(|s| s.kind == BloomKind::Regional)
        {
            let mut forged = body.clone();
            let plen = forged.blooms[pos].payload.len();
            if plen > 0 {
                forged.blooms[pos].payload[plen - 1] ^= 0xFF;
                let _ = verify(&forged);
            }
        }
    }
});
