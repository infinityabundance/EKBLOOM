//! Adversarial round-4 review — independently break the newest round (R1–R3):
//! `blocks`, the unified `engine`, region-scoped machine-2 cursor handling,
//! the `checkpoint`/`packed` wire formats and the prepared `machine_rans` context.
//!
//! Every oracle is the authoritative machine (or `reader::read`), never the
//! structure under test. Hostile bodies are built by mutating a real body /
//! writer output in memory. See `docs/reviews/adversarial.md` "Revision 4".

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ekbloom::adapt::{Workload, WorkloadKind};
use ekbloom::blocks::BlockIndex;
use ekbloom::machine::{machine_by_id, OrMachine, MACHINE_NL2, MACHINE_RANS0};
use ekbloom::machine_rans::{RansContext, RANS0};
use ekbloom::packed::{PackedHistory, PackedRestarts};
use ekbloom::reader::open_bytes;
use ekbloom::state::Snapshot;
use ekbloom::{
    create, create_machine, manifest_hot_regions, read, read_region_scoped, state_from_hot, verify,
    verify_deep, write_body, AccessTrace, BloomKind, BloomShell, Certification, CheckpointSet,
    EkbloomBody, Engine, ManifestationBudget, ManifestationProfile, ReadRequest, SweepConfig,
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

/// A low-entropy corpus machine 2 compresses (`V < N`).
fn skewed(len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b" etaoinshrdlu,.ET\"'\n";
    const WEIGHTS: &[u32] = &[40, 25, 18, 15, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 3, 2, 2];
    let total: u32 = WEIGHTS.iter().sum();
    let mut s: u64 = 0x5EED_1234_ABCD_0001;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut pick = ((s >> 33) as u32) % total;
        for (i, &w) in WEIGHTS.iter().enumerate() {
            if pick < w {
                out.push(ALPHABET[i % ALPHABET.len()]);
                break;
            }
            pick -= w;
        }
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
        iterations: 3,
        seed: 0x0E61_0000_0000_0001,
    }
}

fn tiled_trace(n: u64, region_len: u64) -> AccessTrace {
    let mut regions = Vec::new();
    let mut counts = Vec::new();
    let mut start = 0u64;
    let mut count = 1000u64;
    while start < n {
        let len = region_len.min(n - start);
        regions.push((start, len));
        counts.push(count.max(1));
        count = count.saturating_sub(1);
        start += len;
    }
    AccessTrace { regions, counts }
}

/// In-bounds probes: boundaries, a partial final read, and zero-length at the end.
fn probes(n: u64, b: u32) -> Vec<(u64, u64)> {
    let mut v = vec![(0, 0), (0, 1), (0, n), (n, 0)];
    let bb = u64::from(b);
    for k in 1..4u64 {
        let boundary = (bb * k).min(n);
        if boundary > 0 {
            v.push((boundary - 1, 2.min(n - (boundary - 1))));
            v.push((boundary, bb.min(n - boundary)));
        }
    }
    if n >= 5 {
        v.push((n - 3, 3));
        v.push((n / 2, (n - n / 2).min(7)));
    }
    v
}

/// A minimal in-memory view of a persisted regional shell payload.
struct RegionMeta {
    depth: u16,
    start: u64,
    len: u64,
    entries: u64,
    /// Successor-cursor bytes per record (0 or 8).
    cursor_bytes: usize,
    table_len: u64,
    /// `(key, next_state, next_cursor)` per record.
    records: Vec<(u16, u32, u64)>,
}

fn region_meta(shell: &BloomShell) -> Option<RegionMeta> {
    if shell.kind != BloomKind::Regional {
        return None;
    }
    let p = &shell.payload;
    if p.len() < 46 {
        return None;
    }
    let u16le = |o: usize| u16::from_le_bytes([p[o], p[o + 1]]);
    let u32le = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    let u64le = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
    let depth = u16le(0);
    let start = u64le(2);
    let len = u64le(10);
    let entries = u64le(26);
    let table_len = u64le(38);
    let base = depth as usize + 6;
    let cursor_bytes = table_len
        .checked_div(entries)
        .map(|per| (per as usize).saturating_sub(base))
        .unwrap_or(0);
    let rec = base + cursor_bytes;
    let mut records = Vec::new();
    let mut off = 46usize;
    for _ in 0..entries {
        let key = u16le(off);
        let ns_off = off + 2 + depth as usize;
        let next_state = u32le(ns_off);
        let cursor = if cursor_bytes == 8 {
            u64le(ns_off + 4)
        } else {
            0
        };
        records.push((key, next_state, cursor));
        off += rec;
    }
    Some(RegionMeta {
        depth,
        start,
        len,
        entries,
        cursor_bytes,
        table_len,
        records,
    })
}

/// The authoritative `(state, cursor)` snapshot sequence for `core`.
fn snapshots(machine: &dyn OrMachine, core: &[u8], n: u64) -> Vec<Snapshot> {
    let mut omega = machine.init(core).unwrap();
    let mut out = vec![omega];
    for _ in 0..n {
        machine.reveal(core, &mut omega).unwrap();
        out.push(omega);
    }
    out
}

struct TempDir {
    path: PathBuf,
}
static COUNTER: AtomicU64 = AtomicU64::new(0);
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("ekbloom-r4-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Attack 1 — region_adapt machine-2 successor cursor.
// ---------------------------------------------------------------------------

/// The stored successor cursor (not just the state) must equal the authoritative
/// cursor at the end of every region window. This is what makes a machine-2 table
/// resumable at all: the state alone is not resumable.
#[test]
fn machine2_region_successor_cursor_is_stored_and_authoritative() {
    let src = lcg(0x0E61_0000_0000_00B2, 4096);
    let n = src.len() as u64;
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    let report =
        manifest_hot_regions(&mut body, &tiled_trace(n, 32), &generous(), &small_cfg()).unwrap();
    assert!(report.selected_regions > 0, "no machine-2 region certified");

    let machine = machine_by_id(MACHINE_RANS0).unwrap();
    let snap = snapshots(machine, &body.core, n);

    let mut checked = 0usize;
    for shell in &body.blooms {
        let Some(meta) = region_meta(shell) else {
            continue;
        };
        if meta.cursor_bytes != 8 {
            continue;
        }
        assert_eq!(shell.status, Certification::Certified);
        let d = meta.depth as usize;
        let windows = if meta.len >= d as u64 {
            (meta.len - d as u64 + 1) as usize
        } else {
            0
        };
        let base = meta.start as usize;
        let mut want: BTreeMap<u16, u64> = BTreeMap::new();
        for w in 0..windows {
            let i = base + w;
            let key = (snap[i].state & 0xFFFF) as u16;
            want.insert(key, snap[i + d].cursor);
        }
        for (key, _next_state, cursor) in &meta.records {
            assert_eq!(
                Some(cursor),
                want.get(key),
                "stored successor cursor disagrees with the authoritative pass"
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no cursor-carrying region table was materialised"
    );

    // The authoritative reader agrees (sanity), and the region path must too.
    assert_eq!(
        read_region_scoped(&body, 0, n).unwrap(),
        src,
        "region-scoped whole-object read must be byte-exact"
    );
}

/// Read across **every** regional boundary and the final partial region, compare
/// byte-for-byte with the authoritative decode, then repeat after serialize+reopen.
#[test]
fn machine2_region_reads_are_exact_across_boundaries_and_after_reopen() {
    let src = lcg(0x0E61_0000_0000_00C2, 4096);
    let n = src.len() as u64;
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    let trace = tiled_trace(n, 32);
    manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
    assert!(verify_deep(&body).unwrap().ok);

    let check = |body: &EkbloomBody, tag: &str| {
        for &(start, len) in &trace.regions {
            let whole = read_region_scoped(body, start, len).unwrap();
            assert_eq!(
                &whole,
                &src[start as usize..(start + len) as usize],
                "{tag}: region ({start},{len})"
            );
            // A span straddling the boundary exercises leap → leap and leap → reveal.
            if start + len + 8 <= n {
                let off = start + len - 24;
                let got = read_region_scoped(body, off, 48).unwrap();
                assert_eq!(
                    &got,
                    &src[off as usize..(off + 48) as usize],
                    "{tag}: boundary span ({off},48)"
                );
            }
            // Single bytes squarely on the final partial window of the region.
            for off in [start, start + 1, start + len - 1] {
                assert_eq!(
                    read_region_scoped(body, off, 1).unwrap(),
                    vec![src[off as usize]],
                    "{tag}: byte {off}"
                );
            }
        }
    };
    check(&body, "in-memory");

    let dir = TempDir::new("r4-m2");
    let path = dir.join("m2.ekb");
    write_body(&path, &body).unwrap();
    let reopened = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(reopened.machine_id, MACHINE_RANS0);
    assert!(verify_deep(&reopened).unwrap().ok);
    check(&reopened, "reopened");
}

/// Machine-1 regression: no cursor bytes are stored, and reads stay exact after
/// reopen — the cursor-column feature must not perturb machine 1's byte identity.
#[test]
fn machine1_region_tables_are_cursor_free_and_exact_after_reopen() {
    let src = lcg(0x0E61_0000_0000_00C1, 4096);
    let n = src.len() as u64;
    let mut body = create(&src, ManifestationProfile::Root).unwrap();
    let trace = tiled_trace(n, 32);
    let report = manifest_hot_regions(&mut body, &trace, &generous(), &small_cfg()).unwrap();
    assert!(report.selected_regions > 0);

    let mut seen = 0usize;
    for shell in &body.blooms {
        let Some(meta) = region_meta(shell) else {
            continue;
        };
        seen += 1;
        assert_eq!(meta.cursor_bytes, 0, "machine-1 table must store no cursor");
        assert_eq!(
            meta.table_len,
            meta.entries * (meta.depth as u64 + 6),
            "machine-1 table length must be the compact D+6 form"
        );
    }
    assert!(seen > 0, "no machine-1 region table was materialised");

    let dir = TempDir::new("r4-m1");
    let path = dir.join("m1.ekb");
    write_body(&path, &body).unwrap();
    let reopened = open_bytes(&std::fs::read(&path).unwrap()).unwrap();
    for &(start, len) in &trace.regions {
        assert_eq!(
            read_region_scoped(&reopened, start, len).unwrap(),
            &src[start as usize..(start + len) as usize]
        );
    }
    assert!(verify_deep(&reopened).unwrap().ok);
}

/// **Regression: verify is authoritative for the machine-2 region cursor.**
/// A region table whose `(symbols, next_state)` entries are all correct but whose
/// stored successor cursor is wrong must fail `verify_deep`: the deep verifier
/// re-derives the cursor from the authoritative machine pass. Every certificate is
/// re-linked so the cursor — not the receipt — is the only inconsistency. The
/// corroboration below shows `read_region_scoped` would consume that cursor and
/// diverge from the authoritative reader, which is exactly what verification now
/// prevents.
#[test]
fn verify_rejects_a_wrong_machine2_region_cursor_used_by_read() {
    let src = lcg(0x0E61_0000_0000_00B2, 4096);
    let n = src.len() as u64;
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    manifest_hot_regions(&mut body, &tiled_trace(n, 32), &generous(), &small_cfg()).unwrap();

    // Locate every cursor-carrying certified region table.
    let targets: Vec<(usize, RegionMeta)> = body
        .blooms
        .iter()
        .enumerate()
        .filter_map(|(i, shell)| {
            region_meta(shell)
                .and_then(|m| (m.cursor_bytes == 8 && m.entries > 0).then_some((i, m)))
        })
        .collect();
    assert!(
        !targets.is_empty(),
        "no cursor-carrying machine-2 region table was materialised"
    );

    let mut cursor_wrong = 0usize;
    for (idx, meta) in &targets {
        let old_hash = body.blooms[*idx].content_hash();
        let depth = meta.depth as usize;
        let rec = depth + 6 + 8;
        let mut off = 46usize;
        for _ in 0..meta.entries {
            let cursor_at = off + 2 + depth + 4;
            let stored = u64::from_le_bytes(
                body.blooms[*idx].payload[cursor_at..cursor_at + 8]
                    .try_into()
                    .unwrap(),
            );
            // Tamper every cursor to 0 (structurally valid; symbols/state intact).
            for b in &mut body.blooms[*idx].payload[cursor_at..cursor_at + 8] {
                *b = 0;
            }
            if stored != 0 {
                cursor_wrong += 1;
            }
            off += rec;
        }
        // Re-link the certificate so only the cursor is wrong (receipt recomputed).
        let cert = body
            .certificates
            .iter_mut()
            .find(|c| c.bloom_hash == old_hash)
            .expect("region table must have a linked certificate");
        cert.bloom_hash = body.blooms[*idx].content_hash();
        cert.receipt_hash = cert.compute_receipt_hash();
    }
    assert!(cursor_wrong > 0, "no nonzero successor cursor to tamper");

    // FIXED: the deep verifier re-derives the successor cursor and rejects a table
    // whose stored cursors disagree with the authoritative pass. The regional
    // check is the one that must fail.
    let report = verify_deep(&body).unwrap();
    assert!(
        !report.ok,
        "verify_deep must reject forged successor cursors"
    );
    assert!(
        report
            .checks
            .iter()
            .any(|c| !c.passed && c.name.ends_with(":regional")),
        "the regional check must fail: {:?}",
        report.checks
    );

    // Corroborate that `read` actually consumes the cursor: on a read whose tail
    // follows a table leap, the region path diverges from the authoritative reader.
    let mut diverged = false;
    for (_, meta) in &targets {
        for l in [meta.len - 1, meta.len - 2, meta.len - 3] {
            if l == 0 {
                continue;
            }
            let want = read(
                &body,
                ReadRequest {
                    offset: meta.start,
                    length: l,
                },
            )
            .unwrap();
            match read_region_scoped(&body, meta.start, l) {
                Ok(got) => diverged |= got != want,
                Err(_) => diverged = true,
            }
        }
    }
    assert!(
        diverged,
        "a forged successor cursor must change the region-scoped read"
    );
}

// ---------------------------------------------------------------------------
// Attack 2 — blocks: sizes, lies, exactness, bounds.
// ---------------------------------------------------------------------------

#[test]
fn block_sizes_including_extremes_are_validated_or_exact() {
    let src = skewed(4096);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;

    // 0 is always rejected; the max and max+1 are the documented boundary.
    for machine in [MACHINE_NL2, MACHINE_RANS0] {
        let (core, n) = if machine == MACHINE_NL2 {
            (ekbloom::machine::NL2.encode(&src).unwrap(), n)
        } else {
            (core.clone(), n)
        };
        assert!(BlockIndex::build_machine(&core, n, 0, machine).is_err());
        assert!(BlockIndex::build_machine(&core, n, (1u32 << 30) + 1, machine).is_err());
        for &b in &[1u32, 2, 3, 7, 1 << 30] {
            let idx = BlockIndex::build_machine(&core, n, b, machine).unwrap();
            assert_eq!(idx.read_range(&core, 0, 1).unwrap(), vec![src[0]]);
            assert_eq!(
                idx.read_range(&core, n - 1, 1).unwrap(),
                vec![src[(n - 1) as usize]]
            );
            // A block of 2^30 covers the whole object in a single restart.
            if b == 1 << 30 {
                assert_eq!(idx.block_count(), 1);
            }
        }
    }

    // A crafted prefix with an out-of-range block size is rejected without panicking.
    let idx = BlockIndex::build(&ekbloom::machine::NL2.encode(&src).unwrap(), n, 64).unwrap();
    let bytes = idx.to_bytes();
    for bad_size in [0u32, (1u32 << 30) + 1, u32::MAX] {
        let mut crafted = bytes.clone();
        crafted[14..18].copy_from_slice(&bad_size.to_le_bytes());
        assert!(BlockIndex::from_bytes(&crafted).is_err());
    }
}

#[test]
fn block_read_range_is_byte_exact_for_both_machines_and_after_reopen() {
    let src1 = lcg(0xAAAA_0000_0000_0001, 5000);
    let core1 = ekbloom::machine::NL2.encode(&src1).unwrap();
    let n1 = src1.len() as u64;
    let src2 = skewed(6000);
    let core2 = RANS0.encode(&src2).unwrap();
    let n2 = src2.len() as u64;

    for &b in &[1u32, 2, 3, 7, 64, 256, 1024, 4096] {
        let i1 = BlockIndex::build(&core1, n1, b).unwrap();
        let r1 = BlockIndex::from_bytes(&i1.to_bytes()).unwrap();
        assert_eq!(r1, i1);
        for &(off, len) in &probes(n1, b) {
            assert_eq!(
                r1.read_range(&core1, off, len).unwrap(),
                &src1[off as usize..(off + len) as usize],
                "m1 B={b} ({off},{len})"
            );
        }

        let i2 = BlockIndex::build_machine(&core2, n2, b, MACHINE_RANS0).unwrap();
        let r2 = BlockIndex::from_bytes(&i2.to_bytes()).unwrap();
        assert_eq!(r2, i2);
        for &(off, len) in &probes(n2, b) {
            assert_eq!(
                r2.read_range(&core2, off, len).unwrap(),
                &src2[off as usize..(off + len) as usize],
                "m2 B={b} ({off},{len})"
            );
        }
    }
}

#[test]
fn lying_block_restarts_are_rejected_by_certification_not_served() {
    let src = skewed(3000);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    let idx = BlockIndex::build_machine(&core, n, 256, MACHINE_RANS0).unwrap();
    assert!(idx.certify(&core).unwrap());
    assert!(idx.verify_against(&core).unwrap());

    // A wrong state and a wrong cursor are each caught by certification.
    let mut wrong_state = idx.clone();
    wrong_state.entries[2].state ^= 0xFFFF;
    assert!(!wrong_state.certify(&core).unwrap());
    assert!(!wrong_state.verify_against(&core).unwrap());

    let mut wrong_cursor = idx.clone();
    wrong_cursor.entries[3].cursor = wrong_cursor.entries[3].cursor.wrapping_add(1);
    assert!(!wrong_cursor.certify(&core).unwrap());
    assert!(!wrong_cursor.verify_against(&core).unwrap());

    // A body carrying a lying restart: `verify` rejects it (authoritative)...
    let mut forged = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    let mut lie = BlockIndex::build_machine(&forged.core, n, 256, MACHINE_RANS0).unwrap();
    lie.entries[0].state ^= 0xFFFF;
    forged.blocks.push(lie);
    assert!(
        !verify(&forged).unwrap().ok,
        "verify must reject a lying restart"
    );

    // ...but the engine admits it on the in-file geometry alone and would serve
    // wrong bytes (documented integrity-not-authenticity boundary; `verify` is the
    // authority, exactly as for a forged certified path bloom).
    if let Ok(engine) = Engine::plan(&forged, &generous(), &workload()) {
        if let Ok(got) = engine.read(
            &forged,
            ReadRequest {
                offset: 0,
                length: 4,
            },
        ) {
            assert_ne!(got, &src[..4], "a lying restart must not silently agree");
        }
    }
}

#[test]
fn block_read_step_count_is_actually_bounded() {
    let src = skewed(8192);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    for &b in &[16u32, 64, 256, 1024] {
        let idx = BlockIndex::build_machine(&core, n, b, MACHINE_RANS0).unwrap();
        let bb = u64::from(b);
        for off in [0u64, 1, bb - 1, bb, bb + 5, n / 2, n - 4] {
            let len = 4.min(n - off);
            let (got, steps) = idx.read_range_counted(&core, off, len).unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
            assert_eq!(steps, (off % bb) + len, "exact step count B={b} off={off}");
            assert!(steps <= idx.max_steps(off, len));
            assert!(steps <= bb - 1 + len, "exceeds the B-1+len bound");
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 3 — checkpoint / packed wire formats.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_legacy_and_compact_parse_and_fail_closed() {
    let src = lcg(0xB0B0_0000_0000_0001, 3000);
    let core = ekbloom::machine::NL2.encode(&src).unwrap();
    let n = src.len() as u64;
    let cps = CheckpointSet::build(&core, n, 64).unwrap();
    let bytes = cps.to_bytes();
    assert_eq!(CheckpointSet::from_bytes(&bytes).unwrap(), cps);

    // Legacy (magic-less) 12-byte layout still parses to the same set.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&cps.interval.to_le_bytes());
    legacy.extend_from_slice(&cps.count.to_le_bytes());
    for s in &cps.resume {
        legacy.extend_from_slice(&s.pack());
    }
    assert_eq!(CheckpointSet::from_bytes(&legacy).unwrap(), cps);

    // Bad version / format / interval / zero count / truncation / trailing bytes.
    let mut bad = bytes.clone();
    bad[8] = 99;
    assert!(CheckpointSet::from_bytes(&bad).is_err());
    let mut bad = bytes.clone();
    bad[9] = 99;
    assert!(CheckpointSet::from_bytes(&bad).is_err());
    let mut bad = bytes.clone();
    bad[10..14].copy_from_slice(&3u32.to_le_bytes());
    assert!(CheckpointSet::from_bytes(&bad).is_err());
    let mut bad = bytes.clone();
    bad[14..22].copy_from_slice(&0u64.to_le_bytes());
    assert!(CheckpointSet::from_bytes(&bad).is_err());
    for cut in 0..bytes.len() {
        assert!(
            CheckpointSet::from_bytes(&bytes[..cut]).is_err(),
            "cut {cut}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(CheckpointSet::from_bytes(&trailing).is_err());

    // No ambiguous parse: a payload beginning with the magic but a wrong version is
    // a clean `Err`, never silently reinterpreted as a legacy container.
    let mut magic_wrong_version = bytes.clone();
    magic_wrong_version[8] = 1;
    assert!(CheckpointSet::from_bytes(&magic_wrong_version).is_err());
    // A legacy interval that happens to spell "EKBC" is simply an invalid interval.
    let mut spell = Vec::new();
    spell.extend_from_slice(&0x4342_4B45u32.to_le_bytes()); // "EKBC"
    spell.extend_from_slice(&1u64.to_le_bytes());
    spell.extend_from_slice(&Snapshot::new(0, 0).pack());
    assert!(CheckpointSet::from_bytes(&spell).is_err());
}

#[test]
fn packed_wire_forms_reject_crafted_bytes_and_roundtrip() {
    let src = lcg(0xC0C0_0000_0000_0001, 2000);
    let core = ekbloom::machine::NL2.encode(&src).unwrap();
    let n = src.len() as u64;

    let hist = PackedHistory::build(&core, n).unwrap();
    let hb = hist.to_bytes();
    assert_eq!(PackedHistory::from_bytes(&hb).unwrap(), hist);
    for cut in 0..hb.len() {
        assert!(PackedHistory::from_bytes(&hb[..cut]).is_err());
    }

    let rests = PackedRestarts::build(&core, n, 5).unwrap();
    let rb = rests.to_bytes();
    assert_eq!(PackedRestarts::from_bytes(&rb).unwrap(), rests);
    // stride_shift 64.. is rejected; reserved bytes must be zero.
    let mut bad = rb.clone();
    bad[0] = 64;
    assert!(PackedRestarts::from_bytes(&bad).is_err());
    let mut bad = rb.clone();
    bad[1] = 1;
    assert!(PackedRestarts::from_bytes(&bad).is_err());
    let mut bad = rb.clone();
    bad[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(PackedRestarts::from_bytes(&bad).is_err());

    // `state_from_hot` is exact exactly when the surface covers the positions.
    let hot = &src[..];
    for i in 0..=n {
        let got = state_from_hot(hot, i).unwrap();
        let want = if i == 0 {
            0
        } else if i == 1 {
            u16::from(hot[0])
        } else {
            (u16::from(hot[i as usize - 2]) << 8) | u16::from(hot[i as usize - 1])
        };
        assert_eq!(got, want, "state_from_hot {i}");
    }
    assert!(state_from_hot(&[], 2).is_none());
}

// ---------------------------------------------------------------------------
// Attack 4 — engine.
// ---------------------------------------------------------------------------

fn workload() -> Workload {
    Workload {
        kind: WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, 4096)],
    }
}

#[test]
fn engine_read_is_byte_exact_and_bounded_for_adversarial_requests() {
    for machine in [MACHINE_NL2, MACHINE_RANS0] {
        let source = skewed(32 * 1024);
        let mut body = create_machine(&source, ManifestationProfile::Root, machine).unwrap();
        // A checkpoint with an interval that does not divide the length, so the
        // final partial region is exercised.
        let m = machine_by_id(machine).unwrap();
        let cp = CheckpointSet::build_with(m, &body.core, body.logical_len, 64).unwrap();
        body.checkpoints.push(cp);
        body.generation += 1;

        let engine = Engine::plan(&body, &generous(), &workload()).unwrap();
        assert!(engine.plan_ref().all_certified());
        let n = body.logical_len;

        let requests = [
            (0u64, 1u64),
            (0, n),
            (n - 1, 1),
            (63, 2),
            (64, 64),
            (4093, 9),
            (n / 3, 1000),
            (n - 4096, 4096),
        ];
        for (offset, length) in requests {
            let want = read(&body, ReadRequest { offset, length }).unwrap();
            assert_eq!(
                engine.read(&body, ReadRequest { offset, length }).unwrap(),
                want,
                "machine {machine} ({offset},{length})"
            );
            let (got, steps) = engine
                .read_counted(&body, ReadRequest { offset, length })
                .unwrap();
            assert_eq!(got, want);
            // The positional part is bounded by K-1, independent of length.
            if length == 1 {
                assert!(steps.saturating_sub(1) <= engine.worst_case_steps());
            }
        }

        // Degenerate and out-of-range requests mirror the reader.
        assert!(engine
            .read(
                &body,
                ReadRequest {
                    offset: n,
                    length: 0
                }
            )
            .unwrap()
            .is_empty());
        assert!(engine
            .read(
                &body,
                ReadRequest {
                    offset: n,
                    length: 1
                }
            )
            .is_err());
        assert!(engine
            .read(
                &body,
                ReadRequest {
                    offset: u64::MAX,
                    length: 2
                }
            )
            .is_err());
        assert!(engine
            .read(
                &body,
                ReadRequest {
                    offset: n - 1,
                    length: 2
                }
            )
            .is_err());
    }

    // A body with no bounded structure is rejected, never replayed from Omega_0.
    let body = create_machine(
        &lcg(0x0BAD_0000_0000_0001, 4096),
        ManifestationProfile::Root,
        MACHINE_RANS0,
    )
    .unwrap();
    assert!(Engine::plan(&body, &generous(), &workload()).is_err());
}

// ---------------------------------------------------------------------------
// Attack 5 — machine_rans prepared context.
// ---------------------------------------------------------------------------

#[test]
fn prepared_context_is_exact_rejects_bad_headers_and_is_not_shared() {
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x5A],
        (0u8..=255).collect(),
        vec![7u8; 300],
        skewed(4096),
        lcg(0xDEAD_BEEF_0000_0001, 4096),
    ];
    for src in cases {
        let core = RANS0.encode(&src).unwrap();
        let n = src.len() as u64;
        let ctx = RansContext::from_core(&core).unwrap();

        // The prepared context reproduces the per-symbol authoritative path exactly.
        let mut a = RANS0.init(&core).unwrap();
        let mut b = RANS0.init(&core).unwrap();
        for _ in 0..n {
            assert_eq!(ctx.peek(&core, &a).unwrap(), RANS0.peek(&core, &a).unwrap());
            let sa = ctx.reveal(&core, &mut a).unwrap();
            let sb = RANS0.reveal(&core, &mut b).unwrap();
            assert_eq!(sa, sb);
            assert_eq!(a, b, "prepared context diverged from the machine");
        }
    }

    // Invalid model headers are clean errors (truncated, bad sum, zero frequency).
    assert!(RansContext::from_core(&[0u8; 100]).is_err());
    assert!(RansContext::from_core(&[]).is_err());
    let mut bad_sum = vec![0u8; 512 + 4];
    for i in 0..256usize {
        let f = if i == 0 { 17u16 } else { 16u16 };
        bad_sum[i * 2..i * 2 + 2].copy_from_slice(&f.to_le_bytes());
    }
    assert!(RansContext::from_core(&bad_sum).is_err());
    let mut zero_freq = vec![0u8; 512 + 4];
    for i in 0..256usize {
        let f = if i == 0 {
            0u16
        } else if i == 1 {
            32u16
        } else {
            16u16
        };
        zero_freq[i * 2..i * 2 + 2].copy_from_slice(&f.to_le_bytes());
    }
    assert!(RansContext::from_core(&zero_freq).is_err());

    // Two independent contexts for two different cores do not interfere.
    let ca = RANS0.encode(&skewed(500)).unwrap();
    let cb = RANS0.encode(&lcg(1, 700)).unwrap();
    let xa = RansContext::from_core(&ca).unwrap();
    let xb = RansContext::from_core(&cb).unwrap();
    let ma = machine_by_id(MACHINE_RANS0).unwrap();
    let da = ma.decode(&ca, 500).unwrap();
    let db = ma.decode(&cb, 700).unwrap();
    let mut oa = RANS0.init(&ca).unwrap();
    let mut ob = RANS0.init(&cb).unwrap();
    for k in 0..700usize {
        if k < 500 {
            assert_eq!(xa.reveal(&ca, &mut oa).unwrap(), da[k]);
        }
        assert_eq!(xb.reveal(&cb, &mut ob).unwrap(), db[k]);
    }
}
