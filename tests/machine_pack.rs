//! `OrANS-PACK` (`machine_id = 5`): the directly-addressable fixed-width packed
//! machine. Roundtrip for every corpus category and the constant special case,
//! byte-exact `O(1)` positional reads at every (small) and sampled (larger)
//! position, general bit widths, partial final bytes, alphabet-mapping
//! correctness, malformed-header rejection, `bulk_unpack` equality, and the
//! machines 1/2/3/4 non-regression.

use ekbloom::machine_pack::{Pack, PackContext};
use ekbloom::{
    create_machine, encode_with_k, machine_by_id, ManifestationProfile, OrMachine, MACHINE_NL2,
    MACHINE_PACK, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE, PACK, RANS0, STORE,
};

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

fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(alpha[idx]);
    }
    out
}

fn alternating(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 2) as u8).collect()
}

fn categories() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one", vec![0x5A]),
        ("all_bytes", (0u8..=255).collect()),
        ("repeating", b"abcabcabcabcabcabcabcabc".to_vec()),
        ("zero_run", vec![0u8; 300]),
        ("alternating", alternating(257)),
        ("skewed4", skewed(0x00AC_C000_0001, 4096)),
        ("skewed_small", skewed(0x00AC_C000_0002, 7)),
        ("random", lcg(0x00AC_C000_0003, 2048)),
    ]
}

#[test]
fn roundtrip_every_category_and_constant() {
    for (name, src) in categories() {
        let core = PACK.encode(&src).unwrap();
        let out = PACK.decode(&core, src.len() as u64).unwrap();
        assert_eq!(out, src, "roundtrip failed for {name}");
    }
    // Constant source: the zero-payload special case.
    let src = vec![0x9Cu8; 4096];
    let core = PACK.encode(&src).unwrap();
    // bits=1 ++ k=1 ++ one alphabet byte ++ logical_len(u64): no payload.
    assert_eq!(
        core.len(),
        2 + 1 + 8,
        "constant core must have zero payload"
    );
    assert_eq!(core[1], 1, "constant alphabet size must be 1");
    assert_eq!(core[2], 0x9C);
    let out = PACK.decode(&core, src.len() as u64).unwrap();
    assert_eq!(out, src);
}

#[test]
fn byte_at_and_reveal_are_exact_at_every_position() {
    for (name, src) in categories() {
        let core = PACK.encode(&src).unwrap();
        let ctx = PackContext::from_core(&core).unwrap();
        let machine = machine_by_id(MACHINE_PACK).unwrap();
        let mut omega = machine.init(&core).unwrap();
        assert_eq!(omega, ekbloom::Snapshot::new(0, 0), "{name} genesis");
        for (i, &expect) in src.iter().enumerate() {
            let i = i as u64;
            assert_eq!(ctx.byte_at(&core, i).unwrap(), expect, "{name} byte_at {i}");
            assert_eq!(
                machine.peek(&core, &omega).unwrap(),
                expect,
                "{name} peek {i}"
            );
            assert_eq!(
                machine.reveal(&core, &mut omega).unwrap(),
                expect,
                "{name} reveal {i}"
            );
            assert_eq!(omega.cursor, i + 1, "{name} cursor at {i}");
        }
        // One past the end is a clean error.
        assert!(ctx.byte_at(&core, src.len() as u64).is_err());
    }
}

#[test]
fn sampled_positions_on_a_larger_corpus() {
    let src = skewed(0x00AC_C000_1001, 1 << 20);
    let core = PACK.encode(&src).unwrap();
    let ctx = PackContext::from_core(&core).unwrap();
    // Every 4093rd position, plus both ends.
    let mut positions: Vec<u64> = (0..src.len() as u64).step_by(4093).collect();
    positions.push(src.len() as u64 - 1);
    for &i in &positions {
        assert_eq!(ctx.byte_at(&core, i).unwrap(), src[i as usize], "at {i}");
    }
}

#[test]
fn bulk_unpack_equals_sequential_and_direct() {
    for (name, src) in categories() {
        let core = PACK.encode(&src).unwrap();
        let mut bulk = Vec::new();
        PACK.bulk_unpack(&core, &mut bulk).unwrap();
        assert_eq!(bulk, src, "{name} bulk_unpack");
        let mut seq = Vec::new();
        let ctx = PackContext::from_core(&core).unwrap();
        let mut omega = ekbloom::Snapshot::new(0, 0);
        ctx.decode_into(&core, &mut omega, src.len() as u64, &mut seq)
            .unwrap();
        assert_eq!(seq, src, "{name} decode_into");
        assert_eq!(bulk, seq, "{name} bulk == sequential");
    }
}

#[test]
fn general_and_power_of_two_bit_widths() {
    // Alphabet size 5 packs at a general 3-bit width; sizes 2/4/16 at 1/2/4 bits;
    // the identity rendering at 8 bits.
    let cases: [(&[u8], u8); 4] = [
        (&[10u8, 20, 30, 40, 50], 3),
        (&[7u8, 9], 1),
        (&[1u8, 2, 3, 4], 2),
        (&[1u8, 2, 3, 4, 5, 6, 7, 8], 4),
    ];
    for (alpha, bits) in cases {
        let mut src = Vec::new();
        for i in 0..1000usize {
            src.push(alpha[i % alpha.len()]);
        }
        for &b in &[bits, 8] {
            let core = PACK.encode_with_bits(&src, b).unwrap();
            let ctx = PackContext::from_core(&core).unwrap();
            assert_eq!(ctx.bits(), b, "requested width stored");
            for (i, &expect) in src.iter().enumerate() {
                assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), expect, "bits={b}");
            }
            let out = PACK.decode(&core, src.len() as u64).unwrap();
            assert_eq!(out, src, "bits={b}");
        }
    }
    // A width that cannot fit the alphabet is a clean error.
    let src: Vec<u8> = (0u8..16).collect();
    assert!(PACK.encode_with_bits(&src, 3).is_err(), "k=16 > 2^3");
    assert!(PACK.encode_with_bits(&src, 0).is_err(), "bits=0");
    assert!(PACK.encode_with_bits(&src, 9).is_err(), "bits=9");
}

#[test]
fn partial_final_byte_roundtrips() {
    // N * bits is not a multiple of 8 for these, so the last byte is partial.
    for (src, bits) in [
        (vec![1u8, 2, 3, 4, 5], 4u8),     // 20 bits = 2.5 bytes
        (vec![1u8, 0, 1, 0, 1], 2),       // 10 bits = 1.25 bytes
        (vec![1u8, 0, 1, 1, 1, 0, 1], 1), // 7 bits
        (vec![1u8, 2, 3, 4, 5, 6, 7], 3), // 21 bits
    ] {
        let core = PACK.encode_with_bits(&src, bits).unwrap();
        // Payload byte count is ceil(N*bits/8).
        let payload = (src.len() * bits as usize).div_ceil(8);
        let got = PACK.decode(&core, src.len() as u64).unwrap();
        assert_eq!(got, src, "bits={bits} partial final byte");
        let ctx = PackContext::from_core(&core).unwrap();
        assert_eq!(ctx.byte_at(&core, 0).unwrap(), src[0]);
        assert_eq!(
            ctx.byte_at(&core, (src.len() - 1) as u64).unwrap(),
            src[src.len() - 1],
            "last symbol of a partial final byte (payload {payload} B)"
        );
    }
}

#[test]
fn alphabet_mapping_is_the_sorted_distinct_set() {
    // A source large enough that packing beats raw (a tiny source falls back to
    // the identity rendering, where the header would dominate).
    let mut src = Vec::new();
    for i in 0..1200usize {
        src.push([30u8, 10, 20][i % 3]);
    }
    let core = PACK.encode(&src).unwrap();
    // bits=2, k=3, alphabet ascending {10, 20, 30}.
    assert_eq!(core[0], 2);
    assert_eq!(core[1], 3);
    assert_eq!(&core[2..5], &[10, 20, 30]);
    let ctx = PackContext::from_core(&core).unwrap();
    assert_eq!(ctx.alphabet_len(), 3);
    for (i, &expect) in src.iter().enumerate() {
        assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), expect);
    }
    // The identity mapping for a full-alphabet source: symbol == index.
    let all: Vec<u8> = (0u8..=255).collect();
    let core = PACK.encode(&all).unwrap();
    assert_eq!(core[0], 8);
    assert_eq!(core[1], 0, "identity alphabet sentinel");
    let ctx = PackContext::from_core(&core).unwrap();
    assert_eq!(ctx.alphabet_len(), 0);
    for (i, &expect) in all.iter().enumerate() {
        assert_eq!(ctx.byte_at(&core, i as u64).unwrap(), expect);
    }
}

#[test]
fn malformed_headers_are_clean_errors() {
    // A genuine packed core (via the explicit-width entry point, which never
    // applies the raw fallback): alphabet {1, 2, 3} at width 2.
    let src: Vec<u8> = (0..300usize).map(|i| (i % 3) as u8 + 1).collect();
    let good = PACK.encode_with_bits(&src, 2).unwrap();
    assert_eq!(good[0], 2);
    assert_eq!(good[1], 3);

    // Bad bit width (0 and 9).
    let mut bad = good.clone();
    bad[0] = 0;
    assert!(PackContext::from_core(&bad).is_err(), "bits=0");
    bad[0] = 9;
    assert!(PackContext::from_core(&bad).is_err(), "bits=9");

    // k > 2^bits.
    let mut bad = good.clone();
    bad[0] = 1;
    bad[1] = 3;
    assert!(PackContext::from_core(&bad).is_err(), "k=3 > 2^1");

    // Identity sentinel with a non-8 width.
    let mut bad = good.clone();
    bad[1] = 0;
    assert!(
        PackContext::from_core(&bad).is_err(),
        "identity with bits=2"
    );

    // Duplicated alphabet entry.
    let mut bad = good.clone();
    bad[3] = bad[2];
    assert!(PackContext::from_core(&bad).is_err(), "duplicate alphabet");

    // Truncated header (below the 2-byte minimum).
    assert!(PackContext::from_core(&[]).is_err(), "empty");
    assert!(PackContext::from_core(&[2]).is_err(), "one byte");
    // Truncated logical_len.
    assert!(PackContext::from_core(&good[..5]).is_err(), "short len");

    // Short payload.
    assert!(
        PackContext::from_core(&good[..good.len() - 1]).is_err(),
        "short payload"
    );
    // Trailing bytes.
    let mut bad = good.clone();
    bad.push(0);
    assert!(PackContext::from_core(&bad).is_err(), "trailing byte");

    // check_core rejects a length mismatch.
    let machine = machine_by_id(MACHINE_PACK).unwrap();
    assert!(machine.check_core(&good, src.len() as u64).is_ok());
    assert!(machine.check_core(&good, src.len() as u64 + 1).is_err());
}

#[test]
fn constant_source_has_zero_payload_and_is_exact() {
    // The constant rendering is selected whenever it beats raw (a one-byte source
    // still falls back to the identity rendering, where the header dominates).
    let big = vec![0xFFu8; 100_000];
    let core = PACK.encode(&big).unwrap();
    let ctx = PackContext::from_core(&core).unwrap();
    assert_eq!(ctx.alphabet_len(), 1);
    assert_eq!(core.len(), 2 + 1 + 8, "constant payload is empty");
    assert_eq!(core[0], 1, "constant width 1");
    assert_eq!(core[1], 1, "constant alphabet size 1");
    assert_eq!(core[2], 0xFF);
    assert_eq!(ctx.byte_at(&core, 0).unwrap(), 0xFF);
    assert_eq!(ctx.byte_at(&core, 99_999).unwrap(), 0xFF);
    assert_eq!(PACK.decode(&core, big.len() as u64).unwrap(), big);
    let mut bulk = Vec::new();
    PACK.bulk_unpack(&core, &mut bulk).unwrap();
    assert_eq!(bulk, big);
    // A one-byte constant is still a faithful roundtrip (identity rendering).
    let small = vec![0x00u8; 1];
    let core = PACK.encode(&small).unwrap();
    assert_eq!(PACK.decode(&core, 1).unwrap(), small);
}

#[test]
fn machines_one_through_four_are_unaffected() {
    // Machine ids and a byte-exact roundtrip for each pre-existing machine.
    assert_eq!(MACHINE_NL2, 1);
    assert_eq!(MACHINE_RANS0, 2);
    assert_eq!(MACHINE_STORE, 3);
    assert_eq!(MACHINE_RANS0I, 4);
    assert_eq!(MACHINE_PACK, 5);

    let src = skewed(0x00AC_C000_2001, 4096);

    let core = machine_by_id(MACHINE_NL2).unwrap().encode(&src).unwrap();
    assert_eq!(
        machine_by_id(MACHINE_NL2)
            .unwrap()
            .decode(&core, src.len() as u64)
            .unwrap(),
        src
    );
    let core = RANS0.encode(&src).unwrap();
    assert_eq!(RANS0.decode(&core, src.len() as u64).unwrap(), src);
    let core = STORE.encode(&src).unwrap();
    assert_eq!(core, src);
    assert_eq!(STORE.decode(&core, src.len() as u64).unwrap(), src);
    let core = encode_with_k(&src, 4).unwrap();
    assert_eq!(
        machine_by_id(MACHINE_RANS0I)
            .unwrap()
            .decode(&core, src.len() as u64)
            .unwrap(),
        src
    );

    // Machine 5 builds a body for every profile like the others.
    let body = create_machine(&src, ManifestationProfile::Root, MACHINE_PACK).unwrap();
    assert_eq!(body.machine_id, MACHINE_PACK);
    assert_eq!(body.logical_len, src.len() as u64);
    for (i, &expect) in src.iter().enumerate() {
        assert_eq!(body.symbol_at(i as u64).unwrap(), expect);
    }
}

#[test]
fn pack_new_and_static_agree() {
    let machine = Pack::new();
    assert_eq!(machine.machine_id(), MACHINE_PACK);
    assert_eq!(PACK.machine_id(), MACHINE_PACK);
}
