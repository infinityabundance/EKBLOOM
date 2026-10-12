//! Machine-4 (`OrANS-RANS0I`, interleaved rANS) complete-restart regression.
//!
//! Machine 4's complete resumable state is `(lanes:[u32; K], cursor, lane)`;
//! `(state, cursor)` alone is **not** resumable. Every successor/restart record
//! (regional table, block index, checkpoint) must therefore persist the complete
//! wide [`Snapshot`]. These tests build a machine-4 body, force a certified
//! regional-table leap, then read **beyond** the leap (so a machine step consumes
//! the reconstructed state), and compare every byte with the authoritative
//! machine decode — before and after serialize + reopen. The same is done for
//! blocks and checkpoints. Machine 1/2/3 keep their compact byte widths.

use std::path::{Path, PathBuf};

use ekbloom::blocks::BlockIndex;
use ekbloom::bloom::BloomShell;
use ekbloom::bloom_region::RegionTable;
use ekbloom::machine::{machine_by_id, prepare, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I};
use ekbloom::machine_rans_i::RANS0I;
use ekbloom::reader::open_bytes;
use ekbloom::state::Snapshot;
use ekbloom::{
    create_machine, manifest_hot_regions, read, read_region_scoped, verify_deep, write_body,
    AccessTrace, CheckpointSet, EkbloomError, ManifestationBudget, ManifestationProfile, OrMachine,
    ReadRequest, RegionReader, SweepConfig,
};

/// Order-0 skewed source (machine 4/2 compress it).
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

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn small_cfg() -> SweepConfig {
    SweepConfig {
        iterations: 1,
        seed: 1,
    }
}

/// Authoritative complete resumable state `Omega_i` for `machine` over `core`.
fn omega_at(machine: &dyn ekbloom::OrMachine, core: &[u8], i: u64) -> Snapshot {
    let ctx = prepare(machine, core).unwrap();
    let mut omega = machine.init(core).unwrap();
    ctx.skip(core, &mut omega, i).unwrap();
    omega
}

/// The complete wide snapshot stored in the first regional record of `shell`
/// (`prefix(46) ++ key(2) ++ symbols[D] ++ pack_wide`).
fn first_wide_snapshot(shell: &BloomShell) -> Snapshot {
    let depth = u16::from_le_bytes([shell.payload[0], shell.payload[1]]) as usize;
    let off = 46 + 2 + depth;
    Snapshot::unpack_wide(&shell.payload[off..]).expect("wide snapshot present")
}

fn region_tmp(name: &str) -> PathBuf {
    let dir = Path::new("target/machine4-test").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Machine 4 regional tables.
// ---------------------------------------------------------------------------

/// A machine-4 region table certified over `[0, D)` and read **past** the leap:
/// the reconstructed successor (with its lanes) is what continues the decode.
#[test]
fn machine4_region_leap_then_read_beyond_is_byte_exact() {
    let src = skewed(0x0E61_0000_0000_0044, 4096);
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0I).unwrap();
    assert_eq!(body.machine_id, MACHINE_RANS0I);

    // A single region of exactly the bloom depth `D = 4` has one window, so it
    // always certifies. Reads that start at 0 leap once, then reveal beyond.
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    let report = manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
    assert!(
        report.selected_regions > 0,
        "machine-4 region did not certify: {report:?}"
    );
    assert!(
        RegionReader::new(&body).has_regions(),
        "machine-4 wide region table was rejected by the reader"
    );

    // The materialised table must be in the wide form (D + 2 + WIDE_LEN per entry).
    let shell = body
        .blooms
        .iter()
        .find(|s| s.kind == ekbloom::BloomKind::Regional)
        .expect("a regional shell was materialised");
    let table = RegionTable::from_shell(shell).expect("wide table parses");
    assert!(
        table.wide,
        "machine-4 table must be stored in the wide form"
    );
    assert_eq!(table.cursor_bytes, 0);

    // The stored successor carries non-trivial lane state.
    let stored = first_wide_snapshot(shell);
    assert!(
        stored.lanes.iter().any(|&x| x != 0),
        "the wide successor must carry the lane states"
    );

    // Force the leap (offset 0, region covers [0, D)) and read far beyond it.
    for (off, len) in [(0u64, 64u64), (0, 4096), (0, 1), (3, 5)] {
        let got = read_region_scoped(&body, off, len).unwrap();
        assert_eq!(
            &got,
            &src[off as usize..(off + len) as usize],
            "machine-4 region read ({off},{len}) disagrees"
        );
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

    // Serialize + reopen: the wide records round-trip and reads stay byte-exact.
    let dir = region_tmp("m4-region");
    let path = dir.join("m4.ekb");
    write_body(&path, &body).unwrap();
    let reopened = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert!(verify_deep(&reopened).unwrap().ok);
    for (off, len) in [(0u64, 64u64), (0, 4096)] {
        assert_eq!(
            read_region_scoped(&reopened, off, len).unwrap(),
            &src[off as usize..(off + len) as usize],
            "reopened machine-4 region read ({off},{len}) disagrees"
        );
    }
}

/// The pre-fix bug, made explicit: reconstructing the successor as
/// `(state, cursor)` (lanes/lane zeroed) is **not** resumable — it decodes
/// different bytes. The region-scoped reader (which uses the stored wide
/// snapshot) does not.
#[test]
fn machine4_zeroing_lanes_would_produce_wrong_bytes() {
    let src = skewed(0x0E61_0000_0000_0045, 4096);
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0I).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();

    let shell = body
        .blooms
        .iter()
        .find(|s| s.kind == ekbloom::BloomKind::Regional)
        .expect("a regional shell was materialised");
    let stored = first_wide_snapshot(shell);

    // The pre-fix reconstruction discards every lane.
    let zeroed = Snapshot::new(stored.state, stored.cursor);
    assert_ne!(
        zeroed.lanes, stored.lanes,
        "the stored successor has non-zero lanes; zeroing them is lossy"
    );

    let machine = machine_by_id(MACHINE_RANS0I).unwrap();
    let ctx = prepare(machine, &body.core).unwrap();
    let mut full = stored;
    let mut truncated = zeroed;
    let mut want = Vec::new();
    let mut got_prefix = Vec::new();
    for _ in 0..16 {
        want.push(ctx.reveal(&body.core, &mut full).unwrap());
        got_prefix.push(ctx.reveal(&body.core, &mut truncated).unwrap());
    }
    assert_eq!(want, &src[4..20], "the full snapshot resumes exactly");
    assert_ne!(
        got_prefix, want,
        "dropping the lanes must change the decode (pre-fix would be wrong)"
    );
}

// ---------------------------------------------------------------------------
// Machine 4 block indexes.
// ---------------------------------------------------------------------------

#[test]
fn machine4_block_restarts_are_wide_and_byte_exact_across_boundaries_and_reopen() {
    let src = skewed(0x0E61_0000_0000_0046, 4096);
    let n = src.len() as u64;
    let core = RANS0I.encode(&src).unwrap();
    let index = BlockIndex::build_machine(&core, n, 16, MACHINE_RANS0I).unwrap();
    assert_eq!(index.entry_bytes(), Snapshot::WIDE_LEN as u64);
    assert!(
        index
            .entries
            .iter()
            .any(|s| s.lanes.iter().any(|&x| x != 0)),
        "block restarts must store the lane states"
    );

    // Every read starts from the covering restart, skips to the offset (using the
    // reconstructed lanes) and decodes beyond — byte-exact against the core.
    for (off, len) in [
        (0u64, 16u64),
        (5, 20),
        (15, 2),
        (16, 16),
        (17, 64),
        (4095, 1),
        (0, 4096),
    ] {
        assert_eq!(
            index.read_range(&core, off, len).unwrap(),
            &src[off as usize..(off + len) as usize],
            "machine-4 block read ({off},{len}) disagrees"
        );
    }

    let bytes = index.to_bytes();
    let back = BlockIndex::from_bytes(&bytes).unwrap();
    assert_eq!(back, index);
    assert_eq!(back.entry_bytes(), Snapshot::WIDE_LEN as u64);
    assert_eq!(back.read_range(&core, 7, 100).unwrap(), &src[7..107]);
}

// ---------------------------------------------------------------------------
// Machine 4 checkpoints.
// ---------------------------------------------------------------------------

#[test]
fn machine4_checkpoints_are_wide_roundtrip_and_restore_exactly() {
    let src = skewed(0x0E61_0000_0000_0047, 4096);
    let n = src.len() as u64;
    let core = RANS0I.encode(&src).unwrap();
    let machine = machine_by_id(MACHINE_RANS0I).unwrap();
    let cp = CheckpointSet::build_with(machine, &core, n, 64).unwrap();

    assert!(cp.wide(), "machine-4 checkpoints must use the wide form");
    assert!(!cp.packed());
    assert_eq!(cp.entry_bytes(), Snapshot::WIDE_LEN as u64);
    assert!(
        cp.resume.iter().any(|s| s.lanes.iter().any(|&x| x != 0)),
        "checkpoint resume points must store the lane states"
    );

    for i in [0u64, 1, 63, 64, 65, 127, n - 1] {
        let restored = cp.restore_with(machine, &core, i).unwrap();
        assert_eq!(
            restored,
            omega_at(machine, &core, i),
            "restore at {i} lost lane state"
        );
    }

    // Round-trip through the wide container.
    let bytes = cp.to_bytes();
    let back = CheckpointSet::from_bytes_with(machine, &bytes).unwrap();
    assert_eq!(back, cp);

    // A whole-object read after reopen stays byte-exact (the engine/reader pick
    // the wide checkpoint).
    let mut body = create_machine(
        &src,
        ManifestationProfile::Sparse { interval: 64 },
        MACHINE_RANS0I,
    )
    .unwrap();
    body.checkpoints = vec![cp.clone()];
    let dir = region_tmp("m4-ckpt");
    let path = dir.join("m4c.ekb");
    write_body(&path, &body).unwrap();
    let reopened = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert!(verify_deep(&reopened).unwrap().ok);
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: n
            }
        )
        .unwrap(),
        src
    );
}

// ---------------------------------------------------------------------------
// Machine 1/2/3 stay compact (byte-identity regression).
// ---------------------------------------------------------------------------

#[test]
fn machines_one_two_three_restart_forms_are_unchanged() {
    let src = skewed(0x0E61_0000_0000_0048, 4096);
    let n = src.len() as u64;

    // Block indexes.
    let m1 = create_machine(&src, ManifestationProfile::Root, MACHINE_NL2).unwrap();
    let b1 = BlockIndex::build_machine(&m1.core, n, 16, MACHINE_NL2).unwrap();
    assert_eq!(b1.entry_bytes(), 2);
    assert!(b1.packed());
    let b1_bytes = b1.to_bytes();
    // Directory + one 2-byte record per block.
    assert_eq!(
        b1_bytes.len() as u64,
        ekbloom::blocks::BLOCK_INDEX_PREFIX_LEN as u64 + b1.block_count() as u64 * 2
    );

    let m2 = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    let b2 = BlockIndex::build_machine(&m2.core, n, 16, MACHINE_RANS0).unwrap();
    assert_eq!(b2.entry_bytes(), 12);
    assert!(!b2.packed());

    // Checkpoints: machine 1 packs to 2 bytes, machine 2 keeps 12.
    let c1 =
        CheckpointSet::build_with(machine_by_id(MACHINE_NL2).unwrap(), &m1.core, n, 64).unwrap();
    assert!(c1.packed() && !c1.wide());
    assert_eq!(c1.entry_bytes(), 2);
    let c2 =
        CheckpointSet::build_with(machine_by_id(MACHINE_RANS0).unwrap(), &m2.core, n, 64).unwrap();
    assert!(!c2.packed() && !c2.wide());
    assert_eq!(c2.entry_bytes(), 12);

    // Regional records: machine 1 stores no cursor, machine 2 a `u64` cursor,
    // machine 4 the complete wide snapshot.
    for (machine_id, want_wide, want_cursor) in [
        (MACHINE_NL2, false, 0usize),
        (MACHINE_RANS0, false, 8usize),
        (MACHINE_RANS0I, true, 0usize),
    ] {
        let mut body = create_machine(&src, ManifestationProfile::Root, machine_id).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, 4)],
            counts: vec![1000],
        };
        let report = manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
        assert!(
            report.selected_regions > 0,
            "machine {machine_id}: no region"
        );
        let shell = body
            .blooms
            .iter()
            .find(|s| s.kind == ekbloom::BloomKind::Regional)
            .unwrap();
        let table = RegionTable::from_shell(shell).unwrap();
        assert_eq!(table.wide, want_wide, "machine {machine_id} wide form");
        assert_eq!(
            table.cursor_bytes, want_cursor,
            "machine {machine_id} cursor bytes"
        );
    }
}

// ---------------------------------------------------------------------------
// Crafted / truncated wide records fail cleanly.
// ---------------------------------------------------------------------------

#[test]
fn crafted_or_truncated_wide_records_error_cleanly() {
    // A short wide snapshot never panics.
    assert!(matches!(
        Snapshot::unpack_wide(&[0u8; Snapshot::WIDE_LEN - 1]),
        Err(EkbloomError::Truncated { .. })
    ));

    let src = skewed(0x0E61_0000_0000_0049, 2048);
    let n = src.len() as u64;
    let core = RANS0I.encode(&src).unwrap();

    // A truncated wide block index is a clean error.
    let index = BlockIndex::build_machine(&core, n, 16, MACHINE_RANS0I).unwrap();
    let bytes = index.to_bytes();
    assert!(BlockIndex::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    // A wide layout claiming a non-machine-4 machine is rejected by the prefix.
    let mut forged = bytes.clone();
    forged[10..14].copy_from_slice(&MACHINE_NL2.to_le_bytes());
    assert!(BlockIndex::from_bytes(&forged).is_err());

    // A truncated wide checkpoint container is a clean error.
    let machine = machine_by_id(MACHINE_RANS0I).unwrap();
    let cp = CheckpointSet::build_with(machine, &core, n, 64).unwrap();
    let cp_bytes = cp.to_bytes();
    assert!(CheckpointSet::from_bytes_with(machine, &cp_bytes[..cp_bytes.len() - 1]).is_err());
    // Trailing bytes are rejected too.
    let mut trailing = cp_bytes.clone();
    trailing.push(0);
    assert!(CheckpointSet::from_bytes_with(machine, &trailing).is_err());

    // A truncated wide regional table fails the structural parser cleanly.
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0I).unwrap();
    let trace = AccessTrace {
        regions: vec![(0, 4)],
        counts: vec![1000],
    };
    manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
    let idx = body
        .blooms
        .iter()
        .position(|s| s.kind == ekbloom::BloomKind::Regional)
        .unwrap();
    let full = body.blooms[idx].clone();
    let mut short = full.clone();
    short.payload.pop();
    assert!(RegionTable::from_shell(&short).is_err());
    // The reader ignores the malformed shell rather than trusting it.
    body.blooms[idx] = short;
    assert!(!RegionReader::new(&body).has_regions());
    // And a read still returns the authoritative bytes.
    assert_eq!(read_region_scoped(&body, 0, 64).unwrap(), &src[..64]);
}
