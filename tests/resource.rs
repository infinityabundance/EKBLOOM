//! Resource-policy regressions: whole-object APIs must refuse a declared length
//! that would force an attacker-sized allocation, instead of attempting it.
//!
//! A tiny forged core can declare `u64::MAX` symbols (machines 5/6 carry the
//! declared length in the core itself; machines 2/4 take it from the caller).
//! Every whole-object entry point must answer with a typed
//! [`EkbloomError::AllocationTooLarge`] — never an abort, panic or OOM — while
//! legitimate sizes inside the (unchanged) default cap keep working.

use std::sync::Mutex;

use ekbloom::machine::prepare;
use ekbloom::machine_pack::{PackContext, PACK};
use ekbloom::machine_rans::{RansContext, RANS0};
use ekbloom::machine_rans_i::{encode_with_k, RansIContext, RANS0I};
use ekbloom::machine_seg::{SegContext, SEG, SEG_HEADER_BYTES};
use ekbloom::machine_store::STORE;
use ekbloom::{
    checked_capacity, create, max_single_allocation, read, EkbloomError, ManifestationProfile,
    OrMachine, ReadRequest, ResourcePolicy, ResourcePolicyGuard, Snapshot,
    DEFAULT_MAX_SINGLE_ALLOCATION,
};

/// Serialises the two tests that read or mutate the process-wide default ceiling.
static POLICY_LOCK: Mutex<()> = Mutex::new(());

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

fn assert_too_large<T: std::fmt::Debug>(result: Result<T, EkbloomError>, requested: u64) {
    match result {
        Err(EkbloomError::AllocationTooLarge {
            requested: r,
            limit,
        }) => {
            assert_eq!(r, requested, "requested length in the error");
            assert_eq!(limit, DEFAULT_MAX_SINGLE_ALLOCATION, "default limit");
        }
        other => panic!("expected AllocationTooLarge, got {other:?}"),
    }
}

/// A machine-5 constant core (`k == 1`, empty payload) that declares `n` symbols.
/// The payload is empty for a constant, so the whole declared length is free.
fn forged_constant_pack_core(n: u64) -> Vec<u8> {
    let mut core = vec![1u8, 1u8, 0xAB]; // bits = 1, k = 1, constant symbol 0xAB
    core.extend_from_slice(&n.to_le_bytes());
    core
}

/// A machine-6 fixed container with one constant segment covering `n` symbols.
///
/// The directory BLAKE3 is recomputed so the (self-consistent) forgery parses,
/// exactly the amplification vector: a ~150-byte core declaring an astronomical
/// logical length. Field offsets mirror `src/machine_seg.rs`.
fn forged_constant_seg_core(n: u64) -> Vec<u8> {
    const H_LOGICAL_LEN: usize = 24;
    const H_DIRECTORY_HASH: usize = 56;
    const E_LOGICAL_LEN: usize = 8;

    let mut core = SEG.encode(&[7u8; 4]).unwrap();
    let ctx = SegContext::from_core(&core).unwrap();
    assert_eq!(ctx.segment_count(), 1, "one constant segment");
    assert!(!ctx.variable(), "fixed form");

    core[H_LOGICAL_LEN..H_LOGICAL_LEN + 8].copy_from_slice(&n.to_le_bytes());
    let dir_off = SEG_HEADER_BYTES;
    core[dir_off + E_LOGICAL_LEN..dir_off + E_LOGICAL_LEN + 8].copy_from_slice(&n.to_le_bytes());
    let hash = blake3::hash(&core[dir_off..]);
    core[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32].copy_from_slice(hash.as_bytes());
    core
}

#[test]
fn forged_u64_max_pack_core_is_refused_before_allocating() {
    let core = forged_constant_pack_core(u64::MAX);
    // Sanity: the forged core is accepted and declares `u64::MAX`.
    let ctx = PackContext::from_core(&core).unwrap();
    assert_eq!(ctx.logical_len(), u64::MAX);

    assert_too_large(PACK.decode(&core, u64::MAX), u64::MAX);
    let mut out = Vec::new();
    assert_too_large(PACK.bulk_unpack(&core, &mut out), u64::MAX);
    assert_too_large(ctx.unpack_range(&core, 0, u64::MAX, &mut out), u64::MAX);
    assert_too_large(
        ctx.decode_into(&core, &mut Snapshot::new(0, 0), u64::MAX, &mut out),
        u64::MAX,
    );

    // The prepared whole-object path too.
    let prepared = prepare(&PACK, &core).unwrap();
    assert_too_large(
        prepared.decode_into(&core, &mut Snapshot::new(0, 0), u64::MAX, &mut out),
        u64::MAX,
    );
    // A huge-but-valid `u64` still exceeds the 1 GiB default.
    let one_tib = 1u64 << 40;
    let core_tib = forged_constant_pack_core(one_tib);
    assert_too_large(PACK.decode(&core_tib, one_tib), one_tib);
}

#[test]
fn forged_u64_max_seg_core_is_refused_before_allocating() {
    let core = forged_constant_seg_core(u64::MAX);
    let ctx = SegContext::from_core(&core).unwrap();
    assert_eq!(ctx.logical_len(), u64::MAX);

    assert_too_large(SEG.decode(&core, u64::MAX), u64::MAX);
    let mut omega = SEG.init(&core).unwrap();
    let mut out = Vec::new();
    assert_too_large(
        ctx.decode_into(&core, &mut omega, u64::MAX, &mut out),
        u64::MAX,
    );

    let prepared = prepare(&SEG, &core).unwrap();
    assert_too_large(
        prepared.decode_into(&core, &mut Snapshot::new(0, 0), u64::MAX, &mut out),
        u64::MAX,
    );
}

#[test]
fn machine2_and_machine4_whole_object_decodes_are_refused() {
    let src = lcg(0x00A0_7001, 300);

    let c2 = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    assert_too_large(RANS0.decode(&c2, u64::MAX), u64::MAX);
    assert_too_large(RANS0.state_log(&c2, u64::MAX), u64::MAX);
    let ctx2 = RansContext::from_core(&c2).unwrap();
    let mut out = Vec::new();
    assert_too_large(
        ctx2.decode_into(&c2, &mut Snapshot::new(0, 0), u64::MAX, &mut out),
        u64::MAX,
    );
    // The bounded request still works.
    assert_eq!(RANS0.decode(&c2, n).unwrap(), src);

    let c4 = encode_with_k(&src, 3).unwrap();
    assert_too_large(RANS0I.decode(&c4, u64::MAX), u64::MAX);
    assert_too_large(RANS0I.state_log(&c4, u64::MAX), u64::MAX);
    let ctx4 = RansIContext::from_core(&c4).unwrap();
    let mut omega = RANS0I.init(&c4).unwrap();
    assert_too_large(
        ctx4.decode_into(&c4, &mut omega, u64::MAX, &mut out),
        u64::MAX,
    );
    assert_eq!(RANS0I.decode(&c4, n).unwrap(), src);
}

#[test]
fn legitimate_sizes_within_the_cap_still_work() {
    let src = lcg(0x00A0_7002, 256);
    let n = src.len() as u64;

    // Machine 1 (the authoritative path) and machine 3 (identity).
    let c1 = ekbloom::NL2.encode(&src).unwrap();
    assert_eq!(ekbloom::NL2.decode(&c1, n).unwrap(), src);
    let c3 = STORE.encode(&src).unwrap();
    assert_eq!(STORE.decode(&c3, n).unwrap(), src);

    // Machines 2 and 4.
    assert_eq!(RANS0.decode(&RANS0.encode(&src).unwrap(), n).unwrap(), src);
    assert_eq!(
        RANS0I.decode(&encode_with_k(&src, 2).unwrap(), n).unwrap(),
        src
    );

    // Machines 5 and 6: the whole-object bulk paths decode exactly.
    let c5 = PACK.encode(&src).unwrap();
    assert_eq!(PACK.decode(&c5, n).unwrap(), src);
    let mut bulk = Vec::new();
    PACK.bulk_unpack(&c5, &mut bulk).unwrap();
    assert_eq!(bulk, src);

    let c6 = SEG.encode(&src).unwrap();
    assert_eq!(SEG.decode(&c6, n).unwrap(), src);

    // And the `.ekb` read path is unchanged.
    let body = create(&src, ManifestationProfile::Root).unwrap();
    let out = read(
        &body,
        ReadRequest {
            offset: 0,
            length: n,
        },
    )
    .unwrap();
    assert_eq!(out, src);
}

#[test]
fn default_policy_is_one_gib_and_boundary_is_inclusive() {
    let _lock = POLICY_LOCK.lock().unwrap();
    assert_eq!(max_single_allocation(), DEFAULT_MAX_SINGLE_ALLOCATION);
    assert_eq!(DEFAULT_MAX_SINGLE_ALLOCATION, 1 << 30);

    let policy = ResourcePolicy::new(4096);
    assert!(checked_capacity(4096, policy).is_ok());
    assert!(matches!(
        checked_capacity(4097, policy),
        Err(EkbloomError::AllocationTooLarge {
            requested: 4097,
            limit: 4096
        })
    ));
}

#[test]
fn a_lowered_cap_is_enforced_and_restored() {
    let _lock = POLICY_LOCK.lock().unwrap();
    let src = vec![0x5Au8; 2048];
    let core = STORE.encode(&src).unwrap();

    {
        let guard = ResourcePolicyGuard::set(1024);
        // 2048 > 1024: refused by the whole-object guard.
        assert!(matches!(
            STORE.decode(&core, 2048),
            Err(EkbloomError::AllocationTooLarge {
                requested: 2048,
                limit: 1024
            })
        ));
        // 512 < 1024: still served.
        assert_eq!(STORE.decode(&core, 512).unwrap(), vec![0x5Au8; 512]);
        drop(guard);
    }

    // The guard restored the default ceiling.
    assert_eq!(max_single_allocation(), DEFAULT_MAX_SINGLE_ALLOCATION);
    assert_eq!(STORE.decode(&core, 2048).unwrap(), src);
}

/// The two guarded constants the `.ekb` loader constructs whole (history and
/// checkpoints) reject a forged declared count without allocating.
#[test]
fn forged_history_and_checkpoint_counts_are_clean_errors() {
    // A history header declaring `u64::MAX` positions with no state bytes.
    let mut history = vec![0u8; 64];
    history[8..16].copy_from_slice(&u64::MAX.to_le_bytes()); // logical_len
    history[56..64].copy_from_slice(&u64::MAX.to_le_bytes()); // count == logical_len + 1
    assert!(ekbloom::History::from_bytes(&history).is_err());

    // A compact checkpoint header with `u64::MAX` records and no payload.
    let mut ckpt = Vec::new();
    ckpt.extend_from_slice(b"EKBCKPT\0");
    ckpt.push(2); // version
    ckpt.push(1); // format: packed (2-byte) records
    ckpt.extend_from_slice(&1u32.to_le_bytes()); // interval
    ckpt.extend_from_slice(&u64::MAX.to_le_bytes()); // count
    assert!(ekbloom::CheckpointSet::from_bytes(&ckpt).is_err());
}
