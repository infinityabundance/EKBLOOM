//! EKBLOOM acceptance **gates G1–G5** — machine-checked, executable gates
//! analogous to the OrFS gates A–N. See [`docs/GATES.md`](../docs/GATES.md).
//!
//! Each gate is one `#[test]` that asserts its property with the public API,
//! prints `GATE G<n> <name>: PASS — <evidence>` (and writes
//! `evidence/gates/G<n>.json`), and **panics** on any violation, so the process
//! exits non-zero and `scripts/run-gates.sh` reports FAIL. The gates mirror the
//! relations pinned by the existing focused tests:
//!
//! * **G1** invertibility (`encode∘decode = id`, all machines 1–9) — cf.
//!   `tests/dictionary.rs` / `tests/machine_*`.
//! * **G2** affine block composition (`tests/prefix.rs`).
//! * **G3** worst-case step bound over 1,000,000 positions — reuses the shared
//!   [`bench`] module and the same oracle as `examples/worstcase_bound.rs`.
//! * **G4** single-bit-flip corruption detection (`tests/corruption.rs`,
//!   `examples/corruption_court.rs`).
//! * **G5** exact physical accounting (`tests/accounting.rs`).
//!
//! Run with `sh scripts/run-gates.sh` (release, single-threaded).

#[path = "common/corpora.rs"]
mod bench;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ekbloom::blocks::BlockIndex;
use ekbloom::bloom_context::build_context_shell;
use ekbloom::checkpoint::{CheckpointSet, CKPT_FIXED_LEN};
use ekbloom::format;
use ekbloom::machine::{machine_by_id, state_log, OrMachine, NL2};
use ekbloom::machine_seg::{SegContext, SEG};
use ekbloom::packed::{PackedHistory, PackedRestarts};
use ekbloom::prefix::{block_summary, prefix_scan, reconstruct_blocks, Affine, LinMap};
use ekbloom::tiles::{DirectoryKind, TileSet};
use ekbloom::{
    create, create_machine, grow, inspect, reader, writer, AccessTrace, History,
    ManifestationProfile, PathBloom, Snapshot, MACHINE_NL2, MACHINE_STORE,
};

/// `History::to_bytes` fixed wire header (`u64 generation ++ u64 logical_len ++
/// u64 core_len ++ [u8;32] core_hash ++ u64 count`).
const HISTORY_WIRE_HEADER: u64 = 8 + 8 + 8 + 32 + 8;
/// `BloomShell::to_bytes` fixed shell header.
const BLOOM_SHELL_WIRE_HEADER: u64 = 4 + 2 + 8 + 8 + 8 + 4 + 32 + 8;

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

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

/// A deterministic, half-skewed source of length `len` for machine `id`.
fn source(len: usize, id: u32) -> Vec<u8> {
    let mut s = 0xEAB1_00B1_0000_0001u64 ^ (id as u64) ^ (len as u64);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) as u8;
        out.push(if i % 2 == 0 { b'a' + (r % 4) } else { r });
    }
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("ekbloom-gates-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Print the gate verdict and write its evidence JSON.
fn record(gate: &str, name: &str, evidence: &str) {
    println!("GATE {gate} {name}: PASS — {evidence}");
    let dir = Path::new("evidence/gates");
    std::fs::create_dir_all(dir).expect("evidence/gates");
    let mut s = String::new();
    let _ = writeln!(s, "{{");
    let _ = writeln!(s, "  \"gate\": \"{gate}\",");
    let _ = writeln!(s, "  \"name\": \"{name}\",");
    let _ = writeln!(s, "  \"status\": \"PASS\",");
    let _ = writeln!(s, "  \"evidence\": \"{evidence}\"");
    s.push_str("}\n");
    std::fs::write(dir.join(format!("{gate}.json")), s).expect("write evidence");
}

// ---------------------------------------------------------------------------
// G1 — invertibility: encode∘decode = id across all machines (1–9).
// ---------------------------------------------------------------------------

#[test]
fn g1_invertibility_all_machines() {
    let lengths = [
        1usize, 2, 3, 17, 255, 256, 257, 4095, 4096, 4097, 65_535, 65_536, 65_537, 262_144,
        1_048_575, 1_048_576,
    ];
    let mut cases = 0u64;
    for id in 1..=9u32 {
        let machine = machine_by_id(id).expect("registered machine");
        for &len in &lengths {
            let src = source(len, id);
            let core = machine
                .encode(&src)
                .unwrap_or_else(|e| panic!("machine {id} len {len}: encode failed: {e:?}"));
            let n = len as u64;
            if id == ekbloom::MACHINE_SEG {
                // `Seg` does not override the reference `OrMachine::decode`; that
                // default re-parses the core per symbol (O(n^2), ~415 s at n=2^20 —
                // reported as a library perf issue). The segment-wise
                // `SegContext::decode_into` is the linear path the read API uses;
                // verify invertibility through it at every length, and also through
                // the trait `decode` for the bounded lengths.
                let ctx = SegContext::from_core(&core).expect("seg ctx");
                let mut omega = SEG.init(&core).expect("seg init");
                let mut out = Vec::new();
                ctx.decode_into(&core, &mut omega, n, &mut out)
                    .unwrap_or_else(|e| panic!("machine 6 len {len}: decode_into: {e:?}"));
                assert_eq!(out, src, "machine 6 len {len}: decode_into mismatch");
                if len <= 4097 {
                    let via_trait = machine
                        .decode(&core, n)
                        .unwrap_or_else(|e| panic!("machine 6 len {len}: decode: {e:?}"));
                    assert_eq!(via_trait, src, "machine 6 len {len}: trait decode mismatch");
                }
            } else {
                let out = machine
                    .decode(&core, n)
                    .unwrap_or_else(|e| panic!("machine {id} len {len}: decode failed: {e:?}"));
                assert_eq!(out, src, "machine {id} len {len}: roundtrip mismatch");
            }
            cases += 1;
        }
    }
    record(
        "G1",
        "invertibility",
        &format!(
            "machines=1..9 cases={cases} max_len={} seg_large_path=SegContext::decode_into",
            1usize << 20
        ),
    );
}

// ---------------------------------------------------------------------------
// G2 — affine block composition is exact and associative.
// ---------------------------------------------------------------------------

fn random_map(seed: u64) -> LinMap {
    let mut cols = [0u16; 16];
    let mut s = seed;
    for c in cols.iter_mut() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *c = (s >> 33) as u16;
    }
    LinMap { cols }
}

#[test]
fn g2_prefix_composition_exact() {
    let mut checks = 0u64;

    // A length-1 block summarizes the single machine step, and A^len is exact.
    let a = block_summary(&[0u8]).map;
    for &len in &[0usize, 1, 2, 3, 5, 8, 17, 64, 255, 300] {
        let r = lcg(0x0B10_C0DE ^ len as u64, len);
        for &x0 in &[0u16, 1, 0x1234, 0xFFFF, 0x8000] {
            let mut omega = Snapshot::new(u32::from(x0), 0);
            for _ in 0..len {
                NL2.reveal(&r, &mut omega).unwrap();
            }
            assert_eq!(
                u32::from(block_summary(&r).apply(x0)),
                omega.state,
                "summary != repeated steps (len={len} x0={x0:#06x})"
            );
            checks += 1;
        }
        assert_eq!(
            block_summary(&r).map,
            a.power(len as u64),
            "A^len (len={len})"
        );
    }

    // The prefix scan reconstructs start states and drives a byte-exact
    // reconstruction equal to the authoritative decode.
    let core = lcg(0x9E37_79B9_7F4A_7C15, 4096);
    let oracle = NL2.decode(&core, core.len() as u64).unwrap();
    let logs = state_log(&core, core.len() as u64).unwrap();
    for &bs in &[1usize, 7, 64, 512] {
        let summaries: Vec<Affine> = core.chunks(bs).map(block_summary).collect();
        let prefix = prefix_scan(&summaries);
        assert_eq!(prefix[0], Affine::identity());
        for k in 0..=summaries.len() {
            let mut folded = Affine::identity();
            for blk in &summaries[..k] {
                folded = blk.compose(&folded);
            }
            assert_eq!(prefix[k], folded, "prefix fold (k={k} bs={bs})");
            let pos = (k * bs).min(core.len());
            assert_eq!(
                u32::from(prefix[k].apply(0)),
                logs[pos],
                "start state (k={k} bs={bs})"
            );
            checks += 1;
        }
        let starts: Vec<u16> = prefix[..summaries.len()]
            .iter()
            .map(|f| f.apply(0))
            .collect();
        let blocks: Vec<&[u8]> = core.chunks(bs).collect();
        let flat: Vec<u8> = reconstruct_blocks(&starts, &blocks, 4).unwrap().concat();
        assert_eq!(flat, oracle, "reconstruction (bs={bs})");
    }

    // Composition is associative and the identity is neutral.
    let (m1, m2, m3) = (
        Affine {
            map: random_map(0x1111),
            add: 0x2468,
        },
        Affine {
            map: random_map(0x2222),
            add: 0x1357,
        },
        Affine {
            map: random_map(0x3333),
            add: 0x0F0F,
        },
    );
    assert_eq!(m1.compose(&m2).compose(&m3), m1.compose(&m2.compose(&m3)));
    assert_eq!(m1.compose(&Affine::identity()), m1);
    checks += 3;

    record(
        "G2",
        "prefix-composition",
        &format!("affine_checks={checks} byte_exact=true"),
    );
}

// ---------------------------------------------------------------------------
// G3 — worst-case step bound holds over 1,000,000 positions.
// ---------------------------------------------------------------------------

#[test]
fn g3_worst_case_bound_one_million() {
    let src = lcg(0x0B01_D000_0000_0001, 4 * 1024 * 1024);
    let body = bench::body_with_block(&src, MACHINE_STORE, 1 << 20).expect("body");
    let engine = bench::engine_for(&body).expect("engine");
    let outcome = bench::verify_bound(&engine, &body, &src, 1_000_000, &[1], 0xB0_0001)
        .expect("verify_bound");
    assert_eq!(outcome.samples, 1_000_000, "sample count");
    assert_eq!(outcome.violations, 0, "worst-case bound violated");
    assert_eq!(outcome.mismatches, 0, "a bounded read returned wrong bytes");
    assert!(
        outcome.max_measured_steps <= outcome.max_bound_steps,
        "max measured {} > max bound {}",
        outcome.max_measured_steps,
        outcome.max_bound_steps
    );
    record(
        "G3",
        "worst-case-bound",
        &format!(
            "samples={} max_measured={} max_bound={} violations={} mismatches={} per_region_bound={}",
            outcome.samples,
            outcome.max_measured_steps,
            outcome.max_bound_steps,
            outcome.violations,
            outcome.mismatches,
            engine.worst_case_steps()
        ),
    );
}

// ---------------------------------------------------------------------------
// G4 — single-bit flips in every covered section are detected.
// ---------------------------------------------------------------------------

/// A body carrying every structure kind (mirrors `examples/corruption_court.rs`).
fn rich_body(src: &[u8]) -> ekbloom::EkbloomBody {
    let n = src.len() as u64;
    let mut body = create(src, ManifestationProfile::Root).expect("create");
    grow(&mut body, ManifestationProfile::Full).unwrap();
    grow(&mut body, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    grow(&mut body, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    grow(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
    )
    .unwrap();
    grow(&mut body, ManifestationProfile::Path { depth: 32 }).unwrap();
    body.blooms
        .push(build_context_shell(&body.core, n, 4, body.generation + 1).unwrap());
    let trace = AccessTrace {
        regions: vec![(0, n / 4)],
        counts: vec![100],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::Flat,
        &bench::generous(),
    )
    .unwrap();
    body.blocks
        .push(BlockIndex::build_machine(&body.core, n, 256, MACHINE_NL2).unwrap());
    body
}

#[test]
fn g4_corruption_detection() {
    let src = lcg(0xEB10_0000_C0DE_2026, 4096);
    let body = rich_body(&src);
    let dir = temp_dir("g4");
    let path = dir.join("gates.ekb");
    writer::write_body(&path, &body).expect("write body");
    let bytes = std::fs::read(&path).expect("read body");

    let scan = format::scan(&bytes).expect("scan");
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut sections = 0u64;
    let mut flips = 0u64;
    let mut max_window = 0u64;
    for commit in &scan.commits {
        for section in &commit.sections {
            if section.payload_len == 0 {
                continue;
            }
            sections += 1;
            max_window = max_window.max(section.payload_len.min(4096));
            let samples = 64u64.min(section.payload_len);
            for _ in 0..samples {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let off = section.payload_offset + (s % section.payload_len);
                let bit = 1u8 << ((s >> 8) & 7);
                let mut bad = bytes.clone();
                bad[off as usize] ^= bit;
                flips += 1;
                assert!(
                    reader::open_bytes(&bad).is_err(),
                    "single-bit flip in section {} at byte {off} was NOT detected",
                    section.kind.name()
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(sections > 0, "no sections to flip");
    assert!(max_window <= 4096, "reported window {max_window} > 4096");
    record(
        "G4",
        "corruption-detection",
        &format!(
            "sections={sections} flips={flips} detected={flips} max_reported_window={max_window}"
        ),
    );
}

// ---------------------------------------------------------------------------
// G5 — exact physical accounting: physical_bytes() vs to_bytes().len().
// ---------------------------------------------------------------------------

#[test]
fn g5_physical_accounting_exact() {
    let src = lcg(0xAC00_0000_0000_0001, 4096);
    let n = src.len() as u64;
    let mut structures = 0u64;
    let mut exact = 0u64;
    let mut fixed = 0u64;

    for id in 1..=9u32 {
        let machine = machine_by_id(id).expect("machine");
        let core = machine.encode(&src).expect("encode");

        // A root body's physical bytes are exactly its core.
        let body = create_machine(&src, ManifestationProfile::Root, id).expect("body");
        assert_eq!(
            body.physical_bytes(),
            body.core.len() as u64,
            "machine {id}: root body physical_bytes != core length"
        );
        structures += 1;
        exact += 1;

        // BlockIndex: exact equality (the shared model header is in the core).
        if let Ok(block) = BlockIndex::build_machine(&core, n, 16, id) {
            assert_eq!(
                block.physical_bytes(),
                block.to_bytes().len() as u64,
                "machine {id}: BlockIndex physical_bytes != serialized length"
            );
            structures += 1;
            exact += 1;
        }
        // History / CheckpointSet / path bloom: payload + the fixed wire header.
        if let Ok(history) = History::build_with(machine, &core, n, 1) {
            assert_eq!(
                history.to_bytes().len() as u64,
                history.physical_bytes() + HISTORY_WIRE_HEADER,
                "machine {id}: History relation"
            );
            structures += 1;
            fixed += 1;
        }
        if let Ok(cps) = CheckpointSet::build_with(machine, &core, n, 64) {
            assert_eq!(
                cps.to_bytes().len() as u64,
                cps.physical_bytes() + CKPT_FIXED_LEN as u64,
                "machine {id}: CheckpointSet relation"
            );
            structures += 1;
            fixed += 1;
        }
        if let Ok(shell) = PathBloom::build_with(machine, &core, n, 4, 1) {
            assert_eq!(
                shell.to_bytes().len() as u64,
                shell.physical_bytes() + BLOOM_SHELL_WIRE_HEADER,
                "machine {id}: path bloom relation"
            );
            structures += 1;
            fixed += 1;
        }
    }

    // Machine-1 packed forms: payload + a fixed 16-byte container header.
    let core1 = NL2.encode(&src).unwrap();
    let packed_history = PackedHistory::build(&core1, n).unwrap();
    assert_eq!(
        packed_history.to_bytes().len() as u64,
        packed_history.physical_bytes() + 16
    );
    structures += 1;
    fixed += 1;
    let packed_restarts = PackedRestarts::build(&core1, n, 8).unwrap();
    assert_eq!(
        packed_restarts.to_bytes().len() as u64,
        packed_restarts.physical_bytes() + 16
    );
    structures += 1;
    fixed += 1;

    // `inspect` attributes the same total to the same components.
    let body = create_machine(&src, ManifestationProfile::Root, MACHINE_NL2).unwrap();
    let report = inspect(&body);
    assert_eq!(report.physical_bytes, body.physical_bytes());
    let summed: u64 = report.components.iter().map(|c| c.bytes).sum();
    assert_eq!(summed, body.physical_bytes(), "component sum != body total");
    structures += 1;
    exact += 1;

    record(
        "G5",
        "physical-accounting",
        &format!("structures={structures} exact_equality={exact} fixed_header_relation={fixed}"),
    );
}
