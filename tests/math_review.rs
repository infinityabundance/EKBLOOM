//! Independent mathematical review of the EKBLOOM machine and bloom claims.
//!
//! This file re-derives the frozen `OrANS-NL2` semantics from scratch (its
//! reference outcomes are computed by an independently written step function,
//! not by the library's `reveal`/`phi_from_context`) and checks each claim in
//! `docs/PROOFS.md` by exhaustive or bounded enumeration. It is the executable
//! evidence for `docs/reviews/mathematics.md`.

use ekbloom::bloom::Certification;
use ekbloom::bloom_global::certify_global;
use ekbloom::machine::{state_log, OrMachine, NL2, SBOX};
use ekbloom::model_search::{phi_from_context, AbstractContext};
use ekbloom::state::Snapshot;
use ekbloom::sufficiency_sweep;
use std::collections::BTreeMap;

/// The frozen S-box formula, recomputed independently of the library table.
fn sbox_val(v: u8) -> u8 {
    v ^ (v << 1) ^ (v << 3)
}

/// One machine step: `(x, r) -> (s, x')`.
fn step(x: u32, r: u8) -> (u8, u32) {
    let h = (x >> 8) as u8;
    let l = (x & 0xFF) as u8;
    let s = sbox_val(h) ^ l ^ r;
    (s, ((x & 0xFF) << 8) | u32::from(s))
}

/// `Phi_D` for an abstract `(state, r_0..r_{D-1})` context.
fn phi(state: u16, ctx: &[u8]) -> (Vec<u8>, u32) {
    let mut x = u32::from(state);
    let mut symbols = Vec::with_capacity(ctx.len());
    for &r in ctx {
        let (s, nx) = step(x, r);
        symbols.push(s);
        x = nx;
    }
    (symbols, x)
}

/// Inverse of `phi`: the unique context that yields `target` from `state`.
fn invert_symbols(state: u16, target: &[u8]) -> Vec<u8> {
    let mut x = u32::from(state);
    let mut out = Vec::with_capacity(target.len());
    for &s in target {
        let h = (x >> 8) as u8;
        let l = (x & 0xFF) as u8;
        out.push(s ^ sbox_val(h) ^ l);
        x = ((x & 0xFF) << 8) | u32::from(s);
    }
    out
}

/// Run the real manifest machine from an arbitrary `(state, cursor)` on `core`.
fn machine_phi(core: &[u8], state: u32, cursor: u64, d: u16) -> (Vec<u8>, u32) {
    let mut omega = Snapshot::new(state, cursor);
    let mut symbols = Vec::with_capacity(usize::from(d));
    for _ in 0..d {
        symbols.push(NL2.reveal(core, &mut omega).unwrap());
    }
    (symbols, omega.state)
}

/// Deterministic pseudorandom bytes (same LCG family as the crate's evidence).
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

const STATE_SAMPLE: [u16; 12] = [
    0, 1, 2, 0x00FF, 0x0100, 0x1234, 0x7FFF, 0x8000, 0xABCD, 0xFFFE, 0xFFFF, 0x005A,
];

#[test]
fn sbox_formula_is_a_bijection_and_matches_library() {
    let mut seen = [false; 256];
    for v in 0u16..=255 {
        let s = sbox_val(v as u8);
        assert_eq!(
            SBOX[v as usize], s,
            "SBOX table disagrees with formula at {v}"
        );
        assert!(!seen[s as usize], "S-box not injective at {v}");
        seen[s as usize] = true;
    }
    assert!(seen.iter().all(|&b| b));
}

#[test]
fn claim1_encode_decode_roundtrip_and_encode_bijection() {
    let empty = NL2.encode(&[]).unwrap();
    assert!(empty.is_empty());
    assert_eq!(NL2.decode(&empty, 0).unwrap(), Vec::<u8>::new());

    for s0 in 0u16..=255 {
        let src = [s0 as u8];
        let core = NL2.encode(&src).unwrap();
        assert_eq!(core.len(), 1);
        assert_eq!(NL2.decode(&core, 1).unwrap(), src.to_vec());
    }

    // Exhaustive length-2: roundtrip AND injectivity of `encode`.
    let mut seen = vec![false; 1usize << 16];
    for s0 in 0u16..=255 {
        for s1 in 0u16..=255 {
            let src = [s0 as u8, s1 as u8];
            let core = NL2.encode(&src).unwrap();
            assert_eq!(core.len(), 2);
            assert_eq!(NL2.decode(&core, 2).unwrap(), src.to_vec());
            let idx = usize::from(core[0]) << 8 | usize::from(core[1]);
            assert!(!seen[idx], "encode is not injective at {src:?}");
            seen[idx] = true;
        }
    }
    assert!(seen.iter().all(|&b| b));

    let src: Vec<u8> = (0..1000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let core = NL2.encode(&src).unwrap();
    assert_eq!(NL2.decode(&core, src.len() as u64).unwrap(), src);
}

#[test]
fn claim1_transition_bijections_and_the_literal_collision() {
    // For each fixed r, x -> x' permutes all 2^16 states.
    for r in 0u16..=255 {
        let r = r as u8;
        let mut seen = vec![false; 1usize << 16];
        for x in 0u32..=0xFFFF {
            let (_, nx) = step(x, r);
            assert!(!seen[nx as usize], "x->x' not injective: r={r}, x={x}");
            seen[nx as usize] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }

    // For a fixed state, r -> s is a bijection (XOR with a constant).
    let mut seen = [false; 256];
    for r in 0u16..=255 {
        let (s, _) = step(0x9E37, r as u8);
        assert!(!seen[s as usize]);
        seen[s as usize] = true;
    }
    assert!(seen.iter().all(|&b| b));

    // The literal map (x, r) -> (x', s) is NOT a bijection: x' already carries s
    // in its low byte, so the image has 2^16 elements and every image point has
    // 2^8 preimages. Explicit collision:
    let a = step(0x0100, 11);
    let b = step(0x0200, 22);
    assert_eq!(a, b);
    assert_eq!(a, (0u8, 0u32));

    // Given x and the revealed symbol s, r is recovered uniquely.
    for &x in &[0u32, 0x0100, 0xABCD, 0xFFFF] {
        for r in 0u16..=255 {
            let r = r as u8;
            let (s, _) = step(x, r);
            let h = (x >> 8) as u8;
            let l = (x & 0xFF) as u8;
            assert_eq!(s ^ sbox_val(h) ^ l, r);
        }
    }
}

#[test]
fn claim2_state_log_is_last_two_symbols() {
    for s0 in 0u16..=255 {
        for s1 in 0u16..=255 {
            let src = [s0 as u8, s1 as u8];
            let core = NL2.encode(&src).unwrap();
            let log = state_log(&core, 2).unwrap();
            assert_eq!(
                log,
                vec![
                    0u32,
                    u32::from(s0 as u8),
                    (u32::from(s0 as u8) << 8) | u32::from(s1 as u8)
                ]
            );
        }
    }

    let src: Vec<u8> = (0..257u32).map(|i| (i * 97 + 13) as u8).collect();
    let core = NL2.encode(&src).unwrap();
    let log = state_log(&core, src.len() as u64).unwrap();
    assert_eq!(log[0], 0);
    for i in 0..src.len() {
        let g = ((log[i] & 0xFF) << 8) | u32::from(src[i]);
        assert_eq!(log[i + 1], g, "G recurrence failed at {i}");
        if i >= 2 {
            assert_eq!(
                log[i],
                (u32::from(src[i - 2]) << 8) | u32::from(src[i - 1]),
                "two-symbol formula failed at {i}"
            );
        }
        if i == 0 {
            assert_eq!(log[1], u32::from(src[0]));
        }
    }
}

#[test]
fn claim3_outcome_depends_only_on_state_and_next_d_bytes() {
    for d in 1u16..=4 {
        let width = usize::from(d);
        for k in 0..width {
            for a in 0u16..=255 {
                let a = a as u8;
                let ctx: Vec<u8> = (0..width).map(|j| if j == k { a } else { 0 }).collect();
                for &state in STATE_SAMPLE.iter() {
                    let (symbols, next) = phi(state, &ctx);
                    let lib = phi_from_context(
                        &AbstractContext {
                            state,
                            r: ctx.clone(),
                        },
                        d,
                    )
                    .unwrap();
                    assert_eq!(symbols, lib.symbols);
                    assert_eq!(next, lib.next_state);

                    // Absolute cursor and surrounding bytes are irrelevant.
                    let mut core1 = vec![0xDE, 0xAD];
                    core1.extend_from_slice(&ctx);
                    core1.extend_from_slice(&[0xBE, 0xEF]);
                    let mut core2 = vec![0x11, 0x22, 0x33];
                    core2.extend_from_slice(&ctx);
                    core2.extend_from_slice(&[0x44]);
                    assert_eq!(
                        machine_phi(&core1, u32::from(state), 2, d),
                        (symbols.clone(), next)
                    );
                    assert_eq!(machine_phi(&core2, u32::from(state), 3, d), (symbols, next));
                }
            }
        }
    }

    // Dependence does exist on state and on each r_k (falsification fails).
    let base = phi(0x0000, &[0, 0, 0, 0]);
    let bumped = phi(0x0000, &[0, 0, 1, 0]);
    assert_ne!(base, bumped);
    assert_ne!(base.0[2], bumped.0[2]);
    assert_ne!(phi(0x0000, &[0, 0, 0, 0]), phi(0x0001, &[0, 0, 0, 0]));
}

#[test]
fn claim4_minimal_key_size_is_exactly_depth_bytes() {
    // (a) D=2: all 2^16 outcomes are achieved (fixed state).
    let st = 0x1234u32;
    let mut seen2 = vec![false; 1usize << 16];
    for r0 in 0u16..=255 {
        let s0 = sbox_val((st >> 8) as u8) ^ (st & 0xFF) as u8 ^ r0 as u8;
        let x1 = ((st & 0xFF) << 8) | u32::from(s0);
        for r1 in 0u16..=255 {
            let s1 = sbox_val((x1 >> 8) as u8) ^ (x1 & 0xFF) as u8 ^ r1 as u8;
            seen2[(usize::from(s0) << 8) | usize::from(s1)] = true;
        }
    }
    assert!(seen2.iter().all(|&b| b));

    // (b) D=3: same, exhaustively over all 2^24 contexts (fixed state 0).
    let mut seen3 = vec![false; 1usize << 24];
    for r0 in 0u16..=255 {
        let s0 = r0 as u8; // state 0: S[0] = 0, l = 0
        let x1 = u32::from(s0);
        for r1 in 0u16..=255 {
            let s1 = sbox_val((x1 >> 8) as u8) ^ (x1 & 0xFF) as u8 ^ r1 as u8;
            let x2 = ((x1 & 0xFF) << 8) | u32::from(s1);
            for r2 in 0u16..=255 {
                let s2 = sbox_val((x2 >> 8) as u8) ^ (x2 & 0xFF) as u8 ^ r2 as u8;
                let idx = (usize::from(s0) << 16) | (usize::from(s1) << 8) | usize::from(s2);
                seen3[idx] = true;
            }
        }
    }
    assert!(seen3.iter().all(|&b| b));

    // (c) D=4: constructive surjectivity (2^32 too many to enumerate).
    for &state in &[0u16, 1, 0x1234, 0xFFFF] {
        for t in 0u32..256 {
            let target = [
                t.wrapping_mul(7) as u8,
                t.wrapping_mul(13).wrapping_add(1) as u8,
                t.wrapping_mul(29).wrapping_add(2) as u8,
                t.wrapping_mul(31).wrapping_add(3) as u8,
            ];
            let ctx = invert_symbols(state, &target);
            let (symbols, next) = phi(state, &ctx);
            assert_eq!(symbols, target.to_vec());
            assert_eq!(next, (u32::from(target[2]) << 8) | u32::from(target[3]));
        }
    }

    // (d) Counting lower bound (rigorous): the full single-state domain of (b)
    // has exactly 2^24 distinct outcomes, so ANY key with at most 2 bytes of
    // values (< 2^24) must conflict by pigeonhole. Hence no key of fewer than
    // D = 3 bytes is universally sufficient, whatever its shape.
    assert!(256usize.pow(2) < 1usize << 24);

    // Corroboration with explicitly different-shaped keys of size < D. Each of
    // these leaves a byte free that provably changes the outcome, so on any
    // adequate domain each must exhibit a conflict (none is sufficient).
    let states3 = [0u16, 1, 0x0100, 0x1234, 0xABCD];
    let bytes3 = [0u8, 1, 2];
    for shape in 0..3u8 {
        let mut map: BTreeMap<Vec<u8>, (Vec<u8>, u32)> = BTreeMap::new();
        let mut conflict = false;
        for &st in &states3 {
            for &r0 in &bytes3 {
                for &r1 in &bytes3 {
                    for &r2 in &bytes3 {
                        let (s0, x1) = step(u32::from(st), r0);
                        let (s1, x2) = step(x1, r1);
                        let (s2, x3) = step(x2, r2);
                        let key = match shape {
                            0 => vec![r0, r1],
                            1 => vec![s0, s1],
                            _ => vec![r2, s0],
                        };
                        let out = (vec![s0, s1, s2], x3);
                        match map.get(&key) {
                            Some(prev) => {
                                if !(prev.0 == out.0 && prev.1 == out.1) {
                                    conflict = true;
                                }
                            }
                            None => {
                                map.insert(key, out);
                            }
                        }
                    }
                }
            }
        }
        assert!(
            conflict,
            "different-shape key shape={shape} was sufficient (D=3)"
        );
    }

    // The prefix family over the D = 4 sample: (state, r_0..r_{k-1}) is
    // sufficient exactly for k >= D.
    let d = 4usize;
    let states = [0u16, 1, 2, 3, 4, 5, 6, 7];
    let bytes = [0u8, 1, 2];
    let mut contexts: Vec<(Vec<u8>, Vec<u8>, u32)> = Vec::new();
    for &st in &states {
        for &r0 in &bytes {
            for &r1 in &bytes {
                for &r2 in &bytes {
                    for &r3 in &bytes {
                        let ctx = [r0, r1, r2, r3];
                        let (symbols, next) = phi(st, &ctx);
                        let h = (st >> 8) as u8;
                        let l = (st & 0xFF) as u8;
                        let coords = vec![
                            h, l, r0, r1, r2, r3, symbols[0], symbols[1], symbols[2], symbols[3],
                        ];
                        contexts.push((coords, symbols, next));
                    }
                }
            }
        }
    }

    // (e) Prefix family: (state, r_0..r_{k-1}) is sufficient exactly for k >= D.
    for k in 0..=d {
        let mut map: BTreeMap<Vec<u8>, (Vec<u8>, u32)> = BTreeMap::new();
        let mut conflict = false;
        for (coords, symbols, next) in &contexts {
            let mut key: Vec<u8> = vec![coords[0], coords[1]];
            key.extend_from_slice(&coords[2..2 + k]);
            match map.get(&key) {
                Some(prev) => {
                    if !(prev.0 == *symbols && prev.1 == *next) {
                        conflict = true;
                    }
                }
                None => {
                    map.insert(key, (symbols.clone(), *next));
                }
            }
        }
        assert_eq!(conflict, k < d, "prefix key k={k} sufficiency mismatch");
    }

    // (f) A different-shape key of exactly D bytes exists: the D symbols.
    let mut symbol_map: BTreeMap<Vec<u8>, (Vec<u8>, u32)> = BTreeMap::new();
    for (_, symbols, next) in &contexts {
        match symbol_map.get(symbols) {
            Some(prev) => {
                assert_eq!(prev.0, *symbols);
                assert_eq!(prev.1, *next);
            }
            None => {
                symbol_map.insert(symbols.clone(), (symbols.clone(), *next));
            }
        }
    }
}

#[test]
fn claim5_global_condition_and_shipped_counterexample() {
    // Exact condition in action: constant fills have a well-defined map -> certified.
    for fill in [0u8, 0xAB] {
        let core = NL2.encode(&[fill; 4096]).unwrap();
        let cert = certify_global(&core, 4096, 4).unwrap();
        assert_eq!(cert.status, Certification::Certified, "fill {fill:#x}");
        assert_eq!(cert.conflicts, 0);
        assert!(cert.counterexample.is_none());
    }

    // Pseudorandom -> rejected; recompute the shipped witness from scratch.
    let src = lcg(0x1234_5678_9ABC_DEF0, 4096);
    let core = NL2.encode(&src).unwrap();
    let states = state_log(&core, 4096).unwrap();
    let cert = certify_global(&core, 4096, 4).unwrap();
    assert_eq!(cert.status, Certification::Rejected);
    let cex = cert.counterexample.as_ref().unwrap();
    assert_eq!(cex.key, 175);
    assert_eq!(cex.pos_a, 2106);
    assert_eq!(cex.pos_b, 3976);

    for (pos, out) in [(cex.pos_a, &cex.outcome_a), (cex.pos_b, &cex.outcome_b)] {
        let i = pos as usize;
        assert_eq!((states[i] & 0xFFFF) as u16, 175);
        let (symbols, next) = phi((states[i] & 0xFFFF) as u16, &core[i..i + 4]);
        assert_eq!(symbols, out.symbols);
        assert_eq!(next, out.next_state);
    }
    assert_ne!(cex.outcome_a, cex.outcome_b);

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/evidence/counterexamples/bloom-key-0001.json"
    );
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("\"key\": 175"));
    assert!(text.contains("\"pos_a\": 2106"));
    assert!(text.contains("\"pos_b\": 3976"));
    assert!(text.contains("\"symbols\":[211,47,53,193]"));
    assert!(text.contains("\"symbols\":[63,203,50,238]"));
}

#[test]
fn claim5_no_16bit_key_is_universally_sufficient_for_d4() {
    // D=4 has exactly 256^4 = 2^32 outcomes; a 16-bit key has only 2^16 values.
    assert_eq!(256u64.pow(4), 1u64 << 32);
    assert!(65_536u64 < 256u64.pow(4));
    // The pigeonhole impossibility starts at D=3: 256^d > 2^16 iff d >= 3.
    for d in 3u32..=8 {
        assert!(256u128.pow(d) > 65_536, "256^{d} must exceed 2^16");
    }
    assert_eq!(256u64.pow(2), 65_536);

    // D=2 is exactly at the boundary: 256^2 == 2^16, and the 16-bit key formed
    // from the two symbols IS sufficient (the successor x_2 is determined by
    // them). D=1 likewise, with the 16-bit key (l, s_0). So a 16-bit universal
    // key exists for D <= 2 and is impossible for D >= 3.
    let mut table = vec![None; 1usize << 16];
    for x in 0u32..=0xFFFF {
        for r in 0u16..=255 {
            let (s, next) = step(x, r as u8);
            let l = (x & 0xFF) as u8;
            let key = (usize::from(l) << 8) | usize::from(s);
            let out = (s, next);
            match table[key] {
                Some(prev) => assert_eq!(prev, out, "D=1 key (l,s_0) conflicts"),
                None => table[key] = Some(out),
            }
        }
    }
}

#[test]
fn claim6_sweep_counts_are_combinatorially_forced() {
    let d = 4u64;
    let states = 64u64;
    let bytes = 4u64;
    let rows = sufficiency_sweep(4, 64, 4).unwrap();
    assert_eq!(rows.len(), 5);
    for (k, row) in rows.iter().enumerate() {
        let k = k as u64;
        assert_eq!(
            row.keys,
            states * bytes.pow(k as u32),
            "keys not S*B^k at k={k}"
        );
        if k < d {
            assert_eq!(row.conflicts, row.keys, "k<D must conflict for every key");
        } else {
            assert_eq!(row.conflicts, 0);
        }
    }
    let keys: Vec<u64> = rows.iter().map(|r| r.keys).collect();
    let conflicts: Vec<u64> = rows.iter().map(|r| r.conflicts).collect();
    assert_eq!(keys, vec![64, 256, 1024, 4096, 16384]);
    assert_eq!(conflicts, vec![64, 256, 1024, 4096, 0]);
}

#[test]
fn claim6_canonical_state_only_counterexample_is_valid() {
    let cex = ekbloom::canonical_state_only_counterexample(4).unwrap();
    assert_eq!(cex.a.state, cex.b.state);
    assert_ne!(cex.a.r[0], cex.b.r[0]);
    assert_ne!(cex.outcome_a, cex.outcome_b);
    let (sa, na) = phi(cex.a.state, &cex.a.r);
    let (sb, nb) = phi(cex.b.state, &cex.b.r);
    assert_eq!(
        (sa, na),
        (cex.outcome_a.symbols.clone(), cex.outcome_a.next_state)
    );
    assert_eq!(
        (sb, nb),
        (cex.outcome_b.symbols.clone(), cex.outcome_b.next_state)
    );
}
