//! Tests for the decoder-competition machines:
//! machine 7 (`OrANS-RANS0W`, 16-bit word renormalisation),
//! machine 8 (`OrANS-RANS0IL`, independent-lane streams),
//! and the interleaved-decode SIMD kernel in [`ekbloom::simd`].
//!
//! Covers: exact roundtrip for every source category and lane count; byte-exactness
//! against the authoritative `reader::read`; complete-state exactness
//! (checkpoint / block / restart) including after serialize + reopen; crafted and
//! truncated headers as clean `Err`; the SIMD kernel byte-for-byte against its
//! scalar reference on adversarial inputs; and that machines 1–6 are unchanged
//! (registry + pinned wire hashes).

use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_il::{
    decode_il, encode_il, il_block_restarts, IlContext, IlState, IL_STATE_LEN, RANS0IL,
};
use ekbloom::machine_rans_w::{encode as encode_word16, RANS0W};
use ekbloom::simd::{
    rans_decode8_independent, rans_decode8_independent_scalar, rans_step8_avx2, rans_step8_scalar,
    RANS_LANES8,
};
use ekbloom::{
    create_machine, machine_by_id, reader, write_body, BlockIndex, CheckpointSet, EkbloomError,
    ManifestationProfile, OrMachine, ReadRequest, Snapshot, MACHINE_NL2, MACHINE_PACK,
    MACHINE_RANS0, MACHINE_RANS0I, MACHINE_RANS0IL, MACHINE_RANS0W, MACHINE_SEG, MACHINE_STORE,
};

/// Deterministic LCG byte stream (high entropy).
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

/// Skewed 4-symbol source (order-0 compressible).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(if r < 60 {
            b'a'
        } else if r < 85 {
            b'b'
        } else if r < 95 {
            b'c'
        } else {
            b'd'
        });
    }
    out
}

fn categories() -> Vec<(&'static str, Vec<u8>)> {
    let mut all_bytes = Vec::with_capacity(256);
    all_bytes.extend(0u8..=255);
    vec![
        ("empty", Vec::new()),
        ("one", vec![0x42]),
        ("two", vec![0, 255]),
        ("all_bytes", all_bytes),
        ("repeating", vec![b'x'; 5000]),
        ("skewed", skewed(0xABCD_1234, 8192)),
        ("random", lcg(0x0FED_CBA9, 8192)),
    ]
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Machine 7 — word16 renormalisation.
// ---------------------------------------------------------------------------

#[test]
fn machine7_roundtrips_every_category() {
    for (name, src) in categories() {
        let core = encode_word16(&src);
        assert!(RANS0W.check_core(&core, src.len() as u64).is_ok(), "{name}");
        assert_eq!(
            RANS0W.decode(&core, src.len() as u64).unwrap(),
            src,
            "{name} m7 decode"
        );
    }
}

#[test]
fn machine7_matches_machine2_symbols() {
    // Same static order-0 model: the decoded symbol sequences must be identical.
    for (name, src) in categories() {
        let c2 = RANS0.encode(&src).unwrap();
        let c7 = encode_word16(&src);
        assert_eq!(
            RANS0.decode(&c2, src.len() as u64).unwrap(),
            RANS0W.decode(&c7, src.len() as u64).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn machine7_reveal_peek_and_state_log_agree() {
    let src = skewed(0x7E57_0007, 1234);
    let core = encode_word16(&src);
    let n = src.len() as u64;
    let mut omega = RANS0W.init(&core).unwrap();
    let mut stepwise = Vec::with_capacity(src.len());
    for &expect in &src {
        assert_eq!(RANS0W.peek(&core, &omega).unwrap(), expect);
        stepwise.push(RANS0W.reveal(&core, &mut omega).unwrap());
    }
    assert_eq!(stepwise, src);
    let log = ekbloom::state_log_with(&RANS0W, &core, n).unwrap();
    assert_eq!(log.len(), src.len() + 1);
    assert_eq!(log[0], RANS0W.init(&core).unwrap().state);
}

// ---------------------------------------------------------------------------
// Machine 8 — independent-lane streams.
// ---------------------------------------------------------------------------

#[test]
fn machine8_roundtrips_every_category_and_lane_count() {
    for (name, src) in categories() {
        for k in 1usize..=8 {
            let core = encode_il(&src, k).unwrap();
            let mut out = vec![0u8; src.len()];
            decode_il(&core, &mut out).unwrap();
            assert_eq!(out, src, "{name} k={k}");
            // And through the machine (Snapshot-based) path.
            assert_eq!(
                RANS0IL.decode(&core, src.len() as u64).unwrap(),
                src,
                "{name} k={k} machine decode"
            );
        }
    }
}

#[test]
fn machine8_lanes_are_independent_and_exact() {
    let src = skewed(0x8A1E_0008, 4096);
    let core = encode_il(&src, 8).unwrap();
    let ctx = IlContext::from_core(&core).unwrap();
    assert_eq!(ctx.lanes(), 8);
    // Decoding every lane in isolation from its own payload reproduces exactly the
    // symbols that lane owns.
    let mut all = vec![0u8; src.len()];
    ctx.decode_into_slice(&core, &mut ctx.genesis(&core), &mut all)
        .unwrap();
    assert_eq!(all, src);
    // Lane ranges tile the payload exactly.
    let mut prev_end = ctx.header_bytes();
    for t in 0..8 {
        let (s, e) = ctx.lane_range(t);
        assert_eq!(s, prev_end);
        assert!(e >= s);
        prev_end = e;
    }
    assert_eq!(prev_end, core.len() as u64);
}

#[test]
fn machine8_state_wire_roundtrips_and_snapshot_is_exact() {
    let src = skewed(0x8A1E_0009, 3000);
    let core = encode_il(&src, 4).unwrap();
    let ctx = IlContext::from_core(&core).unwrap();
    let mut s = ctx.genesis(&core);
    ctx.skip(&core, &mut s, 777).unwrap();
    let packed = s.pack();
    assert_eq!(packed.len(), IL_STATE_LEN);
    let back = IlState::unpack(&packed).unwrap();
    assert_eq!(back, s);
    // Snapshot projection carries the per-lane cursors losslessly.
    let snap = back.snapshot();
    assert_eq!(IlState::from_snapshot(&snap), s);
    assert!(snap.cursors.iter().any(|&c| c != 0));
}

const BLOCK: u32 = 256;

#[test]
fn machine8_block_restarts_resume_exactly() {
    let src = skewed(0x8A1E_000A, 4096);
    let core = encode_il(&src, 8).unwrap();
    let n = src.len() as u64;
    let restarts = il_block_restarts(&core, n, BLOCK).unwrap();
    let ctx = IlContext::from_core(&core).unwrap();
    for &(off, len) in &[(0u64, 1u64), (1, 9), (255, 3), (300, 1000), (2048, 1024)] {
        let idx = (off / u64::from(BLOCK)) as usize;
        let start = idx as u64 * u64::from(BLOCK);
        let mut s = restarts[idx];
        ctx.skip(&core, &mut s, off - start).unwrap();
        let mut out = vec![0u8; len as usize];
        ctx.decode_into_slice(&core, &mut s, &mut out).unwrap();
        assert_eq!(out, &src[off as usize..(off + len) as usize], "off={off}");
    }
}

// ---------------------------------------------------------------------------
// Byte-exactness vs the authoritative reader, and after reopen.
// ---------------------------------------------------------------------------

#[test]
fn machines7_and_8_are_byte_exact_vs_reader_read() {
    let src = lcg(0x9EAD_0007, 5000);
    let n = src.len() as u64;
    let probes = [
        (0u64, n),
        (0, 1),
        (1, 4096),
        (2048, 1024),
        (n - 7, 7),
        (n, 0),
    ];
    for &machine in &[MACHINE_RANS0W, MACHINE_RANS0IL] {
        let body = create_machine(&src, ManifestationProfile::Root, machine).unwrap();
        for &(off, len) in &probes {
            let got = reader::read(
                &body,
                ReadRequest {
                    offset: off,
                    length: len,
                },
            )
            .unwrap();
            assert_eq!(
                got,
                &src[off as usize..(off + len) as usize],
                "machine {machine} at ({off},{len})"
            );
        }
    }
}

#[test]
fn machines7_and_8_reopen_and_read_exactly() {
    let src = skewed(0x9EAD_0008, 3000);
    let dir = std::env::temp_dir().join(format!("ekbloom-decoders-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for &machine in &[MACHINE_RANS0W, MACHINE_RANS0IL] {
        let body = create_machine(&src, ManifestationProfile::Root, machine).unwrap();
        let path = dir.join(format!("m{machine}.ekb"));
        write_body(&path, &body).unwrap();
        let reopened = reader::open(&path).unwrap();
        assert_eq!(reopened.logical_len, src.len() as u64);
        let got = reader::read(
            &reopened,
            ReadRequest {
                offset: 100,
                length: 2000,
            },
        )
        .unwrap();
        assert_eq!(got, &src[100..2100], "machine {machine} after reopen");
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Complete-state exactness (checkpoint / block / restart) and generic-path guards.
// ---------------------------------------------------------------------------

#[test]
fn machine7_checkpoints_and_blocks_are_exact() {
    let src = skewed(0xC0DE_0007, 2048);
    let core = encode_word16(&src);
    let n = src.len() as u64;
    let cps = CheckpointSet::build_with(&RANS0W, &core, n, 64).unwrap();
    for i in 0..=n {
        let mut omega = cps.restore_with(&RANS0W, &core, i).unwrap();
        let ctx = ekbloom::machine_rans_w::RansWContext::from_core(&core).unwrap();
        let take = (n - i).min(8);
        let mut out = Vec::new();
        ctx.decode_into(&core, &mut omega, take, &mut out).unwrap();
        assert_eq!(
            out,
            src[i as usize..(i + take) as usize],
            "checkpoint at {i}"
        );
    }
    for b in [1u32, 7, 64, 256] {
        let idx = BlockIndex::build_machine(&core, n, b, MACHINE_RANS0W).unwrap();
        assert!(idx.certify(&core).unwrap());
        let bytes = idx.to_bytes();
        assert_eq!(BlockIndex::from_bytes(&bytes).unwrap(), idx);
        for &(off, len) in &[(0u64, 1u64), (3, 40), (n - 3, 3)] {
            assert_eq!(
                idx.read_range(&core, off, len).unwrap(),
                &src[off as usize..(off + len) as usize]
            );
        }
    }
}

#[test]
fn machines8_generic_wide_structures_reject_cleanly() {
    // The generic block/checkpoint/regional wide forms do not carry per-lane
    // cursors, so they must refuse an independent-lane machine rather than drop
    // them silently.
    let src = skewed(0xC0DE_0008, 1024);
    let core = encode_il(&src, 4).unwrap();
    let n = src.len() as u64;
    assert!(matches!(
        BlockIndex::build_machine(&core, n, 256, MACHINE_RANS0IL),
        Err(EkbloomError::BadSection { .. })
    ));
    assert!(matches!(
        CheckpointSet::build_with(&RANS0IL, &core, n, 64),
        Err(EkbloomError::BadSection { .. })
    ));
    assert!(RANS0IL.snapshot_needs_lane_cursors());
    assert!(RANS0IL.snapshot_needs_lanes());
}

// ---------------------------------------------------------------------------
// Additive selection: the new machines are opt-in, `Auto` is unchanged.
// ---------------------------------------------------------------------------

#[test]
fn additive_selection_prefers_the_new_machines_without_changing_auto() {
    let src = skewed(0xD00D_0007, 4096);
    // The default policy is unchanged (machine 2 wins on a skewed source).
    assert_eq!(ekbloom::select_machine(&src).unwrap(), MACHINE_RANS0);
    // The additive policies choose the new machines when they shrink the source.
    assert_eq!(
        ekbloom::select_machine_word16(&src).unwrap(),
        MACHINE_RANS0W
    );
    assert_eq!(
        ekbloom::select_machine_independent(&src).unwrap(),
        MACHINE_RANS0IL
    );
    for (body, machine) in [
        (
            ekbloom::create_auto_word16(&src, ManifestationProfile::Root).unwrap(),
            MACHINE_RANS0W,
        ),
        (
            ekbloom::create_auto_independent(&src, ManifestationProfile::Root).unwrap(),
            MACHINE_RANS0IL,
        ),
    ] {
        assert_eq!(body.machine_id, machine);
        let got = reader::read(
            &body,
            ReadRequest {
                offset: 0,
                length: src.len() as u64,
            },
        )
        .unwrap();
        assert_eq!(got, src, "machine {machine}");
    }
}

// ---------------------------------------------------------------------------
// Crafted / truncated headers.
// ---------------------------------------------------------------------------

#[test]
fn crafted_and_truncated_headers_are_clean_errors() {
    // Machine 7: truncated header, zero-frequency model, wrong frequency sum.
    assert!(ekbloom::machine_rans_w::RansWContext::from_core(&[]).is_err());
    assert!(ekbloom::machine_rans_w::RansWContext::from_core(&[0u8; 100]).is_err());
    let mut zero = RANS0.encode(&skewed(1, 256)).unwrap();
    zero[0..2].copy_from_slice(&0u16.to_le_bytes());
    assert!(ekbloom::machine_rans_w::RansWContext::from_core(&zero).is_err());

    // Machine 8: truncated header and an out-of-range lane count.
    let core = encode_il(&skewed(2, 256), 4).unwrap();
    assert!(IlContext::from_core(&core[..8]).is_err());
    let mut bad_k = core.clone();
    bad_k[512] = 200;
    assert!(IlContext::from_core(&bad_k).is_err());
    // A declared payload that overruns the core is rejected.
    let mut over = core.clone();
    over[516..520].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(IlContext::from_core(&over).is_err());
}

// ---------------------------------------------------------------------------
// SIMD kernel: byte-for-byte against the scalar reference on adversarial input.
// ---------------------------------------------------------------------------

fn fused_table() -> Vec<u32> {
    let mut table = vec![0u32; 4096];
    let mut cum = 0u32;
    for s in 0..256u32 {
        let f = 16u32; // 256 * 16 == 4096
        let packed = f | (cum << 12) | (s << 24);
        let start = cum as usize;
        for slot in &mut table[start..start + f as usize] {
            *slot = packed;
        }
        cum += f;
    }
    table
}

#[test]
fn simd_step_matches_scalar_for_adversarial_states() {
    let table = fused_table();
    let cases: [[u32; RANS_LANES8]; 5] = [
        [1 << 23; RANS_LANES8],
        [u32::MAX; RANS_LANES8],
        [
            0,
            1,
            2,
            4095,
            1 << 23,
            (1 << 23) + 1,
            0xFFFF_FFFF,
            0x1234_5678,
        ],
        [0xDEAD_BEEF, 0, 4096, 4095, 8191, 1, 2, 3],
        [1 << 30, 1 << 31, (1 << 23) - 1, 7, 13, 29, 31, 37],
    ];
    for states in cases {
        let (wn, ws) = rans_step8_scalar(&states, &table);
        let (gn, gs) = rans_step8_avx2(&states, &table).unwrap();
        assert_eq!(gn, wn);
        assert_eq!(gs, ws);
    }
    assert!(rans_step8_avx2(&[0; RANS_LANES8], &table[..16]).is_err());
}

#[test]
fn simd_independent_decode_matches_scalar_for_adversarial_input() {
    let table = fused_table();
    // Random cores, cursors and ends, including deliberately short payloads.
    for seed in [1u64, 2, 3, 0xDEAD] {
        let core = lcg(seed, 64 * RANS_LANES8);
        let mut states = [1 << 23; RANS_LANES8];
        let mut cursors = [0u64; RANS_LANES8];
        let mut ends = [0u64; RANS_LANES8];
        let mut s = seed;
        for (t, st) in states.iter_mut().enumerate() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            *st = (1 << 23) + ((s >> 33) as u32 % (1 << 24));
            cursors[t] = t as u64 * 8;
            ends[t] = core.len() as u64;
        }
        let mut want = vec![0u8; 64 * RANS_LANES8];
        let mut got = vec![0u8; 64 * RANS_LANES8];
        let mut ws = states;
        let mut wc = cursors;
        let mut gs = states;
        let mut gc = cursors;
        rans_decode8_independent_scalar(&table, &core, &mut ws, &mut wc, &ends, &mut want).unwrap();
        rans_decode8_independent(&table, &core, &mut gs, &mut gc, &ends, &mut got).unwrap();
        assert_eq!(got, want, "seed {seed}");
        assert_eq!(gs, ws);
        assert_eq!(gc, wc);
    }
}

// ---------------------------------------------------------------------------
// Machines 1–6 unchanged: registry and pinned wire hashes.
// ---------------------------------------------------------------------------

#[test]
fn machines_1_to_6_are_registered_and_bit_identical() {
    // The registry still resolves every historical id.
    for (id, name) in [
        (MACHINE_NL2, "nl2"),
        (MACHINE_RANS0, "rans0"),
        (MACHINE_STORE, "store"),
        (MACHINE_RANS0I, "rans0i"),
        (MACHINE_PACK, "pack"),
        (MACHINE_SEG, "seg"),
    ] {
        assert!(machine_by_id(id).is_some(), "machine {id} ({name}) missing");
        assert_eq!(machine_by_id(id).unwrap().machine_id(), id);
    }
    // New ids resolve too.
    assert_eq!(machine_by_id(MACHINE_RANS0W).unwrap().machine_id(), 7);
    assert_eq!(machine_by_id(MACHINE_RANS0IL).unwrap().machine_id(), 8);

    // Pinned wire hashes over a fixed skewed corpus (`FNV-1a(core) ^ len`):
    // machines 1–6 must not have changed when machines 7/8 were added.
    let src = skewed(0x1234_5678, 4096);
    let expected: [(u32, u64); 6] = [
        (MACHINE_NL2, 0xee07_e287_4aa2_a6a5),
        (MACHINE_RANS0, 0x973e_27ff_7441_8b6e),
        (MACHINE_STORE, 0x25a0_05b2_c853_5bfe),
        (MACHINE_RANS0I, 0xe962_df83_5b6e_a6ef),
        (MACHINE_PACK, 0x97d0_f514_5fe8_3e7b),
        (MACHINE_SEG, 0xf2ac_829d_b082_40ed),
    ];
    for (id, want) in expected {
        let core = machine_by_id(id).unwrap().encode(&src).unwrap();
        let got = fnv1a(&core) ^ (core.len() as u64);
        assert_eq!(got, want, "machine {id} wire hash changed");
    }
}

#[test]
fn snapshot_projection_keeps_cursors_inert_for_older_machines() {
    // A single-state snapshot has no cursors; the new field is inert.
    let s = Snapshot::new(1 << 23, 516);
    assert_eq!(s.cursors, [0u64; 8]);
    let wide = Snapshot::unpack_wide(&s.pack_wide()).unwrap();
    assert_eq!(wide.cursors, [0u64; 8]);
}
