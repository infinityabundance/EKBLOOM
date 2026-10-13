//! Adversarial / security tests for the O1–O2 round:
//! * machines 7 (`OrANS-RANS0W`) and 8 (`OrANS-RANS0IL`),
//! * the unwired SIMD rANS decode kernel in [`ekbloom::simd`],
//! * the optimiser footprint gate,
//! * OrFS framing.
//!
//! Every crafted input must be a clean typed `Err` — never a panic, never a wrong
//! byte. In particular an attacker-controlled **cursor** (a restart/checkpoint the
//! reader loads verbatim, or a crafted [`Snapshot`]) must not trip an unchecked
//! `cursor + 1` in a `CoreTooShort` diagnostic.

use ekbloom::machine_rans_il::{encode_il, il_block_restarts, IlContext, IlState};
use ekbloom::machine_rans_w::{encode as encode_word16, RansWContext, RANS0W};
use ekbloom::simd::{rans_decode8_independent, rans_decode8_independent_scalar, RANS_LANES8};
use ekbloom::{
    reader, AccessTrace, CheckpointSet, CourtConfig, EkbloomError, ManifestationBudget,
    ManifestationProfile, OptimizedPlan, OrMachine, ReadRequest, Snapshot, MACHINE_RANS0IL,
    MACHINE_RANS0W, MACHINE_STORE,
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

/// The fused `slot -> (freq, cum, symbol)` table the kernel expects: 4096 slots,
/// 256 symbols each with frequency 16.
fn fused_table() -> Vec<u32> {
    let mut table = vec![0u32; 4096];
    let mut cum = 0u32;
    for s in 0..256u32 {
        let f = 16u32;
        let packed = f | (cum << 12) | (s << 24);
        for slot in &mut table[cum as usize..(cum + f) as usize] {
            *slot = packed;
        }
        cum += f;
    }
    table
}

// ---------------------------------------------------------------------------
// Attack 1 — machine 7's verbatim, unvalidated checkpoint cursor.
// ---------------------------------------------------------------------------

/// A machine-7 checkpoint whose stored cursor is `u64::MAX` is accepted by the
/// parser (only machine 1's implicit cursor is enforced) and then handed to
/// `RansWContext`, whose `read_word` builds `CoreTooShort { needed: p + 1 }` — an
/// unchecked add. The decode must be a clean typed error, not a panic.
///
/// This mirrors the reader's `read_via_checkpoint`, which loads the entry cursor
/// verbatim and calls `skip`/`decode_into` on it (machine 7 has no retained
/// prepared context, so it is not on the cursor-bounded windowed path).
#[test]
fn machine7_bogus_checkpoint_cursor_is_a_typed_error() {
    let src = skewed(0x0B01_0007, 2048);
    let core = encode_word16(&src);
    let n = src.len() as u64;
    let cps = CheckpointSet::build_with(&RANS0W, &core, n, 64).unwrap();

    // Entry 0 begins at CKPT_FIXED_LEN (22); `pack()` = u32 state ++ u64 cursor.
    let mut bytes = cps.to_bytes();
    let cursor_at = 22 + 4;
    bytes[cursor_at..cursor_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let forged = CheckpointSet::from_bytes_with(&RANS0W, &bytes).unwrap();

    // `restore_with` at an exact checkpoint boundary does not advance, so the
    // forged cursor reaches the decode untouched.
    let mut omega = forged.restore_with(&RANS0W, &core, 0).unwrap();
    assert_eq!(
        omega.cursor,
        u64::MAX,
        "forged cursor must survive the parse"
    );
    let ctx = RansWContext::from_core(&core).unwrap();
    let mut out = Vec::new();
    let err = ctx.decode_into(&core, &mut omega, 4, &mut out).unwrap_err();
    assert!(
        matches!(
            err,
            EkbloomError::CoreTooShort { .. } | EkbloomError::BadSection { .. }
        ),
        "expected a typed error, got {err:?}"
    );
}

/// The same defect through the public machine trait with a hand-built snapshot
/// (the boundary already accepted for machines 1/3 in Revision 7).
#[test]
fn machine7_reveal_with_max_cursor_is_a_typed_error() {
    let src = skewed(0x0B01_0008, 512);
    let core = encode_word16(&src);
    // A legal word-renorm state (`RANS_W_L = 1 << 15`) but a bogus cursor.
    let mut omega = Snapshot::new(1 << 15, u64::MAX);
    let err = RANS0W.reveal(&core, &mut omega).unwrap_err();
    assert!(matches!(err, EkbloomError::CoreTooShort { .. }));
}

// ---------------------------------------------------------------------------
// Attack 1 — machine 8's per-lane cursor.
// ---------------------------------------------------------------------------

#[test]
fn machine8_reveal_with_max_lane_cursor_is_a_typed_error() {
    let src = skewed(0x0C01_0008, 512);
    let core = encode_il(&src, 4).unwrap();
    let ctx = IlContext::from_core(&core).unwrap();
    // Valid lane states (`RANS_L = 1 << 23`) but cursors at `u64::MAX`.
    let mut s = IlState {
        lanes: [1 << 23; ekbloom::machine_rans_il::MAX_IL_LANES],
        cursors: [u64::MAX; ekbloom::machine_rans_il::MAX_IL_LANES],
        lane: 0,
    };
    let err = ctx.reveal(&core, &mut s).unwrap_err();
    assert!(matches!(err, EkbloomError::CoreTooShort { .. }));
}

/// The complete-state block restart API is `Err`-clean for every lane count and
/// resumes exactly after a serialize/reopen-free round trip.
#[test]
fn machine8_block_restarts_boundaries_are_exact() {
    for k in 1usize..=8 {
        let src = skewed(0x0C01_0010 + k as u64, 1000);
        let core = encode_il(&src, k).unwrap();
        let n = src.len() as u64;
        let restarts = il_block_restarts(&core, n, 128).unwrap();
        let ctx = IlContext::from_core(&core).unwrap();
        for &(off, len) in &[(0u64, 1u64), (127, 2), (128, 128), (999, 1), (1, 998)] {
            let idx = (off / 128) as usize;
            let start = idx as u64 * 128;
            let mut s = restarts[idx];
            ctx.skip(&core, &mut s, off - start).unwrap();
            let mut out = vec![0u8; len as usize];
            ctx.decode_into_slice(&core, &mut s, &mut out).unwrap();
            assert_eq!(
                out,
                &src[off as usize..(off + len) as usize],
                "k={k} off={off}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 2 — the SIMD kernel must be total for adversarial cursors/boundaries.
// ---------------------------------------------------------------------------

#[test]
fn simd_kernel_max_cursor_is_a_typed_error() {
    let table = fused_table();
    let core = lcg(0x5111_0001, 64 * RANS_LANES8);
    let mut states = [1 << 23; RANS_LANES8];
    let ends = [core.len() as u64; RANS_LANES8];
    let mut out = vec![0u8; RANS_LANES8];

    // Scalar reference: a cursor at `u64::MAX` must be a typed error, not a panic.
    let mut cursors = [u64::MAX; RANS_LANES8];
    let err =
        rans_decode8_independent_scalar(&table, &core, &mut states, &mut cursors, &ends, &mut out)
            .unwrap_err();
    assert!(matches!(err, EkbloomError::CoreTooShort { .. }));

    // The dispatched (AVX2 when available) path must behave identically.
    let mut states = [1 << 23; RANS_LANES8];
    let mut cursors = [u64::MAX; RANS_LANES8];
    let err = rans_decode8_independent(&table, &core, &mut states, &mut cursors, &ends, &mut out)
        .unwrap_err();
    assert!(matches!(err, EkbloomError::CoreTooShort { .. }));
}

// ---------------------------------------------------------------------------
// Attack 1 — crafted / truncated headers stay typed errors.
// ---------------------------------------------------------------------------

#[test]
fn machine7_and_8_crafted_headers_are_clean_errors() {
    // Machine 7: empty, too-short header, zero-frequency model.
    assert!(RansWContext::from_core(&[]).is_err());
    for len in [1usize, 100, 515] {
        assert!(
            RansWContext::from_core(&vec![0u8; len]).is_err(),
            "m7 len {len}"
        );
    }

    // Machine 8: short fixed header, out-of-range K, and a declared lane length
    // that overruns / under-fills the core.
    let core = encode_il(&skewed(0x0D01_0008, 300), 4).unwrap();
    for len in [0usize, 8, 16, 515] {
        assert!(
            IlContext::from_core(&core[..len.min(core.len())]).is_err(),
            "m8 len {len}"
        );
    }
    let mut bad_k = core.clone();
    bad_k[512..516].copy_from_slice(&0u32.to_le_bytes());
    assert!(IlContext::from_core(&bad_k).is_err(), "K=0");
    bad_k[512..516].copy_from_slice(&9u32.to_le_bytes());
    assert!(IlContext::from_core(&bad_k).is_err(), "K=9");

    // A single lane length that overruns the reported core length.
    let mut overrun = core.clone();
    overrun[516..520].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(IlContext::from_core(&overrun).is_err(), "lane overrun");
    // A truncated payload (declared total < core) is rejected too.
    assert!(
        IlContext::from_core(&core[..core.len() - 1]).is_err(),
        "short payload"
    );
}

// ---------------------------------------------------------------------------
// Attack 3 — the footprint gate must keep uniform/incompressible plans <= raw.
// ---------------------------------------------------------------------------

/// A skewed access trace over the whole object with one heavily-read region.
fn hot_trace(n: u64) -> AccessTrace {
    let region = 16 * 1024u64;
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    let mut pos = 0u64;
    while pos + region <= n {
        regions.push((pos, region));
        counts.push(4 + (pos % 9));
        pos += region;
    }
    let hot = ((n / 4) * 3).min(n.saturating_sub(region));
    regions.push((hot, region.min(n - hot)));
    counts.push(4000);
    AccessTrace { regions, counts }
}

fn court(seed: u64) -> CourtConfig {
    CourtConfig {
        reps: 3,
        interleave: true,
        seed,
    }
}

/// With the budget set to exactly the raw size, an incompressible corpus must
/// still admit (and choose) the verbatim base — never a plan that expands past
/// raw.
#[test]
fn optimizer_uniform_tight_budget_stays_leq_raw() {
    let src = lcg(0x0F01_0001, 64 * 1024);
    let n = src.len() as u64;
    let trace = hot_trace(n);
    let budget = ManifestationBudget {
        maximum_physical_bytes: n,
    };
    let (plan, receipts) = OptimizedPlan::optimize(&src, &trace, &budget, &court(7)).unwrap();
    assert_eq!(
        plan.machine_id(),
        MACHINE_STORE,
        "uniform data must use machine 3"
    );
    assert!(
        plan.committed_bytes() <= n,
        "committed {} must be <= raw {n}",
        plan.committed_bytes()
    );
    assert!(plan.tile_shift().is_none() && plan.hot_regions().is_empty());

    let body = plan.build(&src).unwrap();
    assert_eq!(body.physical_bytes(), plan.committed_bytes());
    assert!(ekbloom::verify_deep(&body).unwrap().ok);
    let engine = plan.engine(&body).unwrap();
    assert!(engine.plan_ref().all_certified());
    assert!(
        receipts
            .iter()
            .any(|r| r.reason.contains("footprint floor")),
        "the base-representation floor must be recorded"
    );
    for &(off, len) in &[(0u64, 1u64), (1, 4095), (n - 1, 1), (0, n)] {
        let got = engine
            .read(
                &body,
                ReadRequest {
                    offset: off,
                    length: len,
                },
            )
            .unwrap();
        assert_eq!(got, &src[off as usize..(off + len) as usize]);
    }
}

/// A heavily compressible (constant) corpus must not expand past raw even though
/// the gate is inert for a compressing auto machine.
#[test]
fn optimizer_constant_corpus_does_not_expand_past_raw() {
    let src = vec![0x5Au8; 32 * 1024];
    let n = src.len() as u64;
    let budget = ManifestationBudget {
        maximum_physical_bytes: 2 * n,
    };
    let (plan, _) = OptimizedPlan::optimize(&src, &hot_trace(n), &budget, &court(9)).unwrap();
    assert!(
        plan.committed_bytes() < n,
        "committed {} must stay below raw {n}",
        plan.committed_bytes()
    );
    let body = plan.build(&src).unwrap();
    assert_eq!(body.physical_bytes(), plan.committed_bytes());
    assert!(ekbloom::verify_deep(&body).unwrap().ok);
    let engine = plan.engine(&body).unwrap();
    assert!(engine.plan_ref().all_certified());
}

// ---------------------------------------------------------------------------
// Machines 7/8 through the authoritative reader, before and after reopen.
// ---------------------------------------------------------------------------

#[test]
fn machines7_and_8_reader_roundtrip_all_boundaries() {
    let src = lcg(0x0E01_0008, 4097);
    let n = src.len() as u64;
    let dir = std::env::temp_dir().join(format!("ekbloom-adv-o-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for &machine in &[MACHINE_RANS0W, MACHINE_RANS0IL] {
        let body = ekbloom::create_machine(&src, ManifestationProfile::Root, machine).unwrap();
        assert_eq!(body.machine_id, machine);
        let path = dir.join(format!("m{machine}.ekb"));
        ekbloom::write_body(&path, &body).unwrap();
        let reopened = reader::open(&path).unwrap();
        for &(off, len) in &[
            (0u64, 1u64),
            (1, 4095),
            (4096, 1),
            (n - 1, 1),
            (0, n),
            (n, 0),
        ] {
            let got = reader::read(
                &reopened,
                ReadRequest {
                    offset: off,
                    length: len,
                },
            )
            .unwrap();
            assert_eq!(
                got,
                &src[off as usize..(off + len) as usize],
                "m{machine} {off}+{len}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
