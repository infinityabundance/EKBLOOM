//! Engine **core-identity binding** tests (P0 API-correctness).
//!
//! An [`Engine`] holds decoder contexts prepared from the core of the body it was
//! planned against (machine 2's fused table, machine 4's lane states). Before this
//! binding existed, [`Engine::read`] accepted *any* [`EkbloomBody`]: the block-restart
//! dispatcher could decode object B with a table prepared for object A and silently
//! return wrong bytes.
//!
//! These tests pin the fix: every read entry point verifies the bound
//! [`CoreIdentity`] first and rejects a different object — or a newer generation of
//! the same object — with [`EkbloomError::EngineBodyMismatch`], never decoding.
//! [`ReadyObject`] makes the mismatch impossible by construction, and reads
//! byte-for-byte identically to [`ekbloom::reader::read`].

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::engine::{CoreIdentity, ReadyObject};
use ekbloom::{
    create_machine, reader, BlockIndex, EkbloomBody, EkbloomError, Engine, ManifestationBudget,
    ManifestationProfile, ReadRequest,
};

const M1: u32 = 1;
const M2: u32 = 2;
const M3: u32 = 3;
const M4: u32 = 4;

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn workload4k() -> Workload {
    Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, 4096)],
    }
}

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

/// An order-0 skewed 4-symbol source (machines 2/4 compress it: `V < N`).
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

/// A body of `machine` carrying one covering block index of `block_bytes` — the
/// cold structure the plan will choose, so the engine prepares a decoder context.
fn body_with_blocks(source: &[u8], machine: u32, block_bytes: u32) -> EkbloomBody {
    let mut body = create_machine(source, ManifestationProfile::Root, machine).unwrap();
    let idx =
        BlockIndex::build_machine(&body.core, body.logical_len, block_bytes, machine).unwrap();
    body.blocks.push(idx);
    body.generation += 1;
    body
}

/// The reviewer's exact counterexample: two **different** machine-4 bodies with the
/// same logical length and block geometry. A plan built for A must reject B — never
/// silently decode B with A's prepared interleaved-rANS context.
#[test]
fn engine_rejects_a_different_machine4_body_of_identical_geometry() {
    let n = 64 * 1024;
    let src_a = skewed(0x11, n);
    let src_b = skewed(0x22, n);
    let body_a = body_with_blocks(&src_a, M4, 1024);
    let body_b = body_with_blocks(&src_b, M4, 1024);

    // Same logical length and block geometry; different content.
    assert_eq!(body_a.logical_len, body_b.logical_len);
    assert_eq!(body_a.blocks.len(), body_b.blocks.len());
    assert_eq!(body_a.blocks[0].block_bytes, body_b.blocks[0].block_bytes);
    assert_ne!(body_a.core_hash, body_b.core_hash, "bodies must differ");

    let engine = Engine::plan(&body_a, &generous(), &workload4k()).unwrap();
    let req = ReadRequest {
        offset: 4096,
        length: 4096,
    };

    // Same body + generation: accepted and byte-exact.
    assert_eq!(
        engine.read(&body_a, req).unwrap(),
        reader::read(&body_a, req).unwrap()
    );

    // Pre-fix, reading B through A's engine returned `Ok` with wrong bytes: the
    // two bodies decode differently, which is exactly what made the mismatch
    // undetectable at the call site.
    assert_ne!(
        reader::read(&body_a, req).unwrap(),
        reader::read(&body_b, req).unwrap(),
        "the counterexample bodies must decode to different bytes"
    );

    // Every read entry point now rejects before decoding anything.
    assert!(matches!(
        engine.read(&body_b, req),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
    assert!(matches!(
        engine.read_counted(&body_b, req),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
    assert!(matches!(
        engine.read_counted_steps(&body_b, req),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
    assert!(matches!(
        engine.byte_at(&body_b, 0),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
    let mut out = vec![0u8; 4096];
    assert!(matches!(
        engine.read_into(&body_b, req, &mut out),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
    // `read_into` verified before writing, so the destination is untouched.
    assert!(out.iter().all(|&b| b == 0));
    // A mismatched body exposes no resident surface either.
    assert!(engine.resident_surface(&body_b).is_none());

    // The error names the differing identity field.
    let err = engine.read(&body_b, req).unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("core_hash") || text.contains("machine_id"),
        "mismatch detail should name the identity field: {text}"
    );
}

/// A **new generation** of the *same* object (same core, appended structure) is a
/// different byte-image and must be rejected too.
#[test]
fn engine_rejects_a_new_generation_of_the_same_object() {
    let src = skewed(0x33, 32 * 1024);
    let mut body = body_with_blocks(&src, M4, 1024);
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    let req = ReadRequest {
        offset: 100,
        length: 64,
    };
    assert_eq!(
        engine.read(&body, req).unwrap(),
        reader::read(&body, req).unwrap()
    );

    let planned = *engine.identity();
    assert_eq!(CoreIdentity::of(&body), planned);

    // Mutate the body to a new generation: same core, extra block index.
    let idx = BlockIndex::build_machine(&body.core, body.logical_len, 2048, M4).unwrap();
    body.blocks.push(idx);
    body.generation += 1;
    assert_eq!(body.core_hash, planned.core_hash, "same core");
    assert_ne!(body.generation, planned.generation, "new generation");

    assert!(matches!(
        engine.read(&body, req),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
}

/// The same binding holds for machines 1 and 2 (and their prepared/serial contexts).
#[test]
fn engine_binding_holds_for_machines_one_and_two() {
    let n = 32 * 1024;
    for machine in [M1, M2] {
        let src_a = skewed(0x40 + u64::from(machine), n);
        let src_b = skewed(0x80 + u64::from(machine), n);
        let body_a = body_with_blocks(&src_a, machine, 1024);
        let body_b = body_with_blocks(&src_b, machine, 1024);
        assert_ne!(body_a.core_hash, body_b.core_hash);

        let engine = Engine::plan(&body_a, &generous(), &workload4k()).unwrap();
        let req = ReadRequest {
            offset: 5000,
            length: 100,
        };
        assert_eq!(
            engine.read(&body_a, req).unwrap(),
            reader::read(&body_a, req).unwrap()
        );
        assert!(matches!(
            engine.read(&body_b, req),
            Err(EkbloomError::EngineBodyMismatch { .. })
        ));

        // A new generation of A is rejected as well.
        let mut newer = body_a.clone();
        newer.generation += 1;
        assert!(matches!(
            engine.read(&newer, req),
            Err(EkbloomError::EngineBodyMismatch { .. })
        ));
    }
}

/// A body of a *different machine* (hence a different core) is rejected.
#[test]
fn engine_rejects_a_body_of_a_different_machine() {
    let n = 16 * 1024;
    let src = lcg(0x51, n);
    let a = body_with_blocks(&src, M1, 1024);
    let b = body_with_blocks(&src, M2, 1024);
    let engine = Engine::plan(&a, &generous(), &workload4k()).unwrap();
    let req = ReadRequest {
        offset: 0,
        length: 16,
    };
    let err = engine.read(&b, req).unwrap_err();
    assert!(matches!(err, EkbloomError::EngineBodyMismatch { .. }));
    assert!(err.to_string().contains("machine_id"));
}

/// `Engine::from_plan` keeps the core identity, so a reconstructed engine still
/// rejects a foreign body (and still reads its own body byte-exactly).
#[test]
fn from_plan_preserves_the_core_identity() {
    let src = skewed(0x60, 16 * 1024);
    let a = body_with_blocks(&src, M2, 1024);
    let engine = Engine::plan(&a, &generous(), &workload4k()).unwrap();
    let rebuilt = Engine::from_plan(engine.plan_ref().clone());
    assert_eq!(rebuilt.identity(), engine.identity());

    let req = ReadRequest {
        offset: 10,
        length: 10,
    };
    assert_eq!(
        rebuilt.read(&a, req).unwrap(),
        reader::read(&a, req).unwrap()
    );

    let mut b = a.clone();
    b.generation += 1;
    assert!(matches!(
        rebuilt.read(&b, req),
        Err(EkbloomError::EngineBodyMismatch { .. })
    ));
}

/// [`ReadyObject`] binds a body to its engine: reads are byte-exact vs
/// `reader::read` on every machine, and there is no way to pass a foreign body.
#[test]
fn ready_object_reads_are_byte_exact_vs_reader() {
    let n = 40 * 1024;
    for machine in [M1, M2, M3, M4] {
        let src = skewed(0x55 + u64::from(machine), n);
        let body = body_with_blocks(&src, machine, 1024);
        let ready = ReadyObject::prepare(body).unwrap();
        assert_eq!(ready.logical_len(), src.len() as u64);

        let probes = [
            (0u64, 1u64),
            (1, 1),
            (1023, 2),
            (1024, 4096),
            (n as u64 / 2, 1234),
            (n as u64 - 7, 7),
            (0, n as u64),
        ];
        for (offset, length) in probes {
            if offset + length > n as u64 {
                continue;
            }
            let req = ReadRequest { offset, length };
            let want = reader::read(ready.body(), req).unwrap();

            assert_eq!(
                ready.read(req).unwrap(),
                want,
                "machine {machine} ({offset},{length})"
            );
            let (got, steps) = ready.read_counted_steps(req).unwrap();
            assert_eq!(got, want);
            assert_eq!(steps.reveals, length);
            assert!(steps.positioning <= ready.worst_case_steps_for(req));

            assert_eq!(ready.byte_at(offset).unwrap(), want[0]);
            if length > 0 {
                let mut out = vec![0u8; length as usize];
                ready.read_into(req, &mut out).unwrap();
                assert_eq!(out, want, "read_into machine {machine} ({offset},{length})");
            }
        }
    }
}
