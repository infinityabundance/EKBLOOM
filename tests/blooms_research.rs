//! Integration tests for the research blooms: universal `T_D[x]`, contextual
//! `T_D[x, c]`, and regional `T_D^{(reg)}[x]`.
//!
//! These exercise only the public API and assert agreement with the authoritative
//! machine. `Phi_D` is only defined at positions with a full `D`-step window
//! (`i + D <= N`); the tests below compare the blooms against it there.

use ekbloom::bloom::{BloomKind, Certification};
use ekbloom::bloom_context::{
    build_context_shell, certify_context, minimal_context_bytes, ContextBloom,
};
use ekbloom::bloom_global::{build_global_shell, certify_global, GlobalBloom, UNIVERSE_KEYS};
use ekbloom::bloom_key::key_from_state;
use ekbloom::bloom_region::{build_regional, certify_regional, RegionTable};
use ekbloom::machine::{state_log, OrMachine, NL2};
use ekbloom::state::Snapshot;
use std::path::Path;

const DEPTHS: [u16; 4] = [1, 2, 4, 8];

/// Deterministic pseudorandom bytes from a small LCG (no new dependencies).
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

/// Number of full `D`-step windows in `n` positions.
fn windows(n: u64, d: u16) -> usize {
    if n < u64::from(d) {
        0
    } else {
        usize::try_from(n - u64::from(d) + 1).unwrap()
    }
}

/// The authoritative `Phi_D` at position `i`: the `D` symbols and successor state.
fn phi_d(core: &[u8], states: &[u32], i: usize, depth: u16) -> (Vec<u8>, u32) {
    let mut omega = Snapshot::new(states[i], i as u64);
    let mut syms = Vec::with_capacity(usize::from(depth));
    for _ in 0..depth {
        syms.push(NL2.reveal(core, &mut omega).unwrap());
    }
    (syms, omega.state)
}

#[test]
fn global_constant_certified_and_matches_machine() {
    for &fill in &[0x00u8, 0xAB] {
        let src = [fill; 4096];
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let states = state_log(&core, n).unwrap();
        for &d in DEPTHS.iter() {
            let cert = certify_global(&core, n, d).unwrap();
            assert_eq!(
                cert.status,
                Certification::Certified,
                "fill={fill:#x} D={d}"
            );
            assert_eq!(cert.entries, u64::from(UNIVERSE_KEYS));
            assert_eq!(cert.conflicts, 0);
            assert!(cert.counterexample.is_none());
            assert_eq!(
                cert.physical_bytes,
                u64::from(UNIVERSE_KEYS) * (u64::from(d) + 4)
            );

            let shell = build_global_shell(&core, n, d, 1).unwrap();
            let bloom = GlobalBloom::from_shell(&shell).unwrap();
            for (i, &x) in states.iter().enumerate().take(windows(n, d)) {
                let (syms, next) = phi_d(&core, &states, i, d);
                let out = bloom.step(x).unwrap();
                assert_eq!(out.symbols, syms, "fill={fill:#x} D={d} i={i}");
                assert_eq!(out.next_state, next);
            }
        }
    }
}

#[test]
fn global_random_rejected_with_valid_counterexample() {
    let src = lcg(0x1234_5678_9ABC_DEF0, 4096);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    let states = state_log(&core, n).unwrap();
    for &d in DEPTHS.iter() {
        let cert = certify_global(&core, n, d).unwrap();
        assert_eq!(cert.status, Certification::Rejected, "D={d}");
        assert!(cert.conflicts >= 1);
        let cex = cert.counterexample.clone().expect("random input conflicts");
        assert_ne!(cex.pos_a, cex.pos_b);
        let xa = states[cex.pos_a as usize];
        let xb = states[cex.pos_b as usize];
        assert_eq!(key_from_state(xa), cex.key);
        assert_eq!(key_from_state(xb), cex.key);
        assert_eq!(key_from_state(xa), key_from_state(xb));

        // Recompute Phi_D at both witness positions and match the recorded outcomes.
        let (sa, na) = phi_d(&core, &states, cex.pos_a as usize, d);
        let (sb, nb) = phi_d(&core, &states, cex.pos_b as usize, d);
        assert_eq!(cex.outcome_a.symbols, sa);
        assert_eq!(cex.outcome_a.next_state, na);
        assert_eq!(cex.outcome_b.symbols, sb);
        assert_eq!(cex.outcome_b.next_state, nb);
        assert_ne!(cex.outcome_a, cex.outcome_b);
    }
}

#[test]
fn global_physical_bytes_follow_depth() {
    let core = NL2.encode(&lcg(42, 300)).unwrap();
    for &d in DEPTHS.iter() {
        let cert = certify_global(&core, 300, d).unwrap();
        assert_eq!(cert.physical_bytes, 65536u64 * (u64::from(d) + 4));
    }
}

#[test]
fn contextual_exact_all_depths() {
    let src = lcg(0x0102_0304_0506_0708, 4096);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    let states = state_log(&core, n).unwrap();
    for &d in DEPTHS.iter() {
        assert_eq!(minimal_context_bytes(d), d);
        let cert = certify_context(&core, n, d).unwrap();
        assert_eq!(cert.status, Certification::Certified, "D={d}");
        assert_eq!(cert.conflicts, 0);
        assert!(cert.counterexample.is_none());
        assert!(cert.entries > 0);

        let shell = build_context_shell(&core, n, d, 3).unwrap();
        let bloom = ContextBloom::from_shell(&shell).unwrap();
        let width = usize::from(d);
        for (i, &x) in states.iter().enumerate().take(windows(n, d)) {
            let st = key_from_state(x);
            let ctx = &core[i..i + width];
            let out = bloom.look(st, ctx).expect("observed context present");
            let (syms, next) = phi_d(&core, &states, i, d);
            assert_eq!(out.symbols, syms, "D={d} i={i}");
            assert_eq!(out.next_state, next);
        }
        let rebuilt = bloom.to_shell(shell.generation, shell.core_hash);
        assert_eq!(rebuilt, shell);
    }
}

#[test]
fn regional_constant_all_certified_with_exact_lookups() {
    for &fill in &[0x00u8, 0x5A] {
        let src = [fill; 4096];
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let states = state_log(&core, n).unwrap();
        for &d in DEPTHS.iter() {
            for &rb in &[512u32, 1024, 4096] {
                let cert = certify_regional(&core, n, d, rb).unwrap();
                assert_eq!(cert.conflicts, 0, "fill={fill:#x} D={d} R={rb}");
                assert_eq!(cert.certified_regions, cert.regions);
                assert!(cert.first_counterexample.is_none());

                let shells = build_regional(&core, n, d, rb, 2).unwrap();
                assert_eq!(shells.len() as u64, cert.regions);
                let mut sum = 0u64;
                for shell in &shells {
                    assert_eq!(shell.kind, BloomKind::Regional);
                    assert_eq!(shell.status, Certification::Certified);
                    let table = RegionTable::from_shell(shell).unwrap();
                    assert_eq!(table.status, Certification::Certified);
                    sum += table.physical_bytes();

                    let w = windows(table.logical_len, d);
                    let start_idx = usize::try_from(table.logical_start).unwrap();
                    for (k, &x) in states.iter().enumerate().skip(start_idx).take(w) {
                        let out = table.look(x).expect("reachable state present");
                        let (syms, next) = phi_d(&core, &states, k, d);
                        assert_eq!(out.symbols, syms, "fill={fill:#x} D={d} R={rb} k={k}");
                        assert_eq!(out.next_state, next);
                    }
                }
                assert_eq!(sum, cert.physical_bytes);
            }
        }
    }
}

#[test]
fn regional_random_reports_counts_consistently() {
    let src = lcg(0xCAFE_BABE_0000_0001, 4096);
    let core = NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    for &d in DEPTHS.iter() {
        let cert = certify_regional(&core, n, d, 1024).unwrap();
        let shells = build_regional(&core, n, d, 1024, 2).unwrap();
        assert_eq!(shells.len() as u64, cert.regions);

        let mut sum = 0u64;
        let mut certified = 0u64;
        for shell in &shells {
            let table = RegionTable::from_shell(shell).unwrap();
            if table.status == Certification::Certified {
                certified += 1;
            }
            sum += table.physical_bytes();
        }
        assert_eq!(sum, cert.physical_bytes);
        assert_eq!(certified, cert.certified_regions);
    }
}

#[test]
fn writes_deterministic_global_counterexample() {
    let src = lcg(0x1234_5678_9ABC_DEF0, 4096);
    let core = NL2.encode(&src).unwrap();
    let cert = certify_global(&core, src.len() as u64, 4).unwrap();
    assert_eq!(cert.status, Certification::Rejected);
    let cex = cert.counterexample.expect("global key insufficient");
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("evidence/counterexamples/bloom-key-0001.json");
    cex.write_json(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(&format!("\"key\": {}", cex.key)),
        "missing key: {text}"
    );
    assert!(text.contains("\"outcome_a\""));
    assert!(text.contains("\"outcome_b\""));
    assert!(text.contains(&format!("\"next_state\":{}", cex.outcome_a.next_state)));
    assert!(text.contains(&format!("\"next_state\":{}", cex.outcome_b.next_state)));
}
