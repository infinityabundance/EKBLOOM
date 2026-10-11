//! Exactness, bounds and format tests for independently restartable blocks and
//! the compact checkpoint wire format.
//!
//! Pins: block `read_range` byte-exactness against the authoritative decode for
//! both machines at every block size (including boundary-crossing, partial-final
//! and zero-length reads), byte-exactness after serialize + reopen, clean `Err`
//! on crafted/truncated block sections, restart certification detecting tamper,
//! the exact bounded step count for a cold machine-2 read, the packed checkpoint
//! round-trip with legacy backward compatibility, the persisted `.ekb` block
//! section (in-memory and file-backed), and that machine 1 is unchanged.

use ekbloom::blocks::{BlockIndex, BLOCK_INDEX_MAGIC, BLOCK_INDEX_PREFIX_LEN};
use ekbloom::machine::{OrMachine, MACHINE_NL2, MACHINE_RANS0};
use ekbloom::machine_rans::RANS0;
use ekbloom::{
    create, create_machine, open, read, verify, write_body, AccessPath, AccessPlan, CheckpointSet,
    CourtConfig, FileBody, ManifestationProfile, Planner, ReadRequest,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Block sizes exercised (small sizes force many boundary crossings).
const BLOCK_SIZES: [u32; 8] = [1, 2, 3, 7, 64, 256, 1024, 4096];

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

/// A skewed source machine 2 compresses (`V < N`).
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

/// In-bounds `(offset, length)` probes including boundary crossings, a partial
/// final read and a zero-length read at the end.
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

#[test]
fn read_range_is_exact_for_both_machines_at_every_block_size() {
    // Machine 1 (V = N).
    let src1 = lcg(0x1111_2222_3333_4444, 5000);
    let core1 = ekbloom::machine::NL2.encode(&src1).unwrap();
    let n1 = src1.len() as u64;

    // Machine 2 (V < N).
    let src2 = skewed(6000);
    let core2 = RANS0.encode(&src2).unwrap();
    let n2 = src2.len() as u64;
    assert!(core2.len() < src2.len(), "machine 2 must compress");

    for &b in &BLOCK_SIZES {
        let i1 = BlockIndex::build(&core1, n1, b).unwrap();
        assert_eq!(i1.machine_id(), MACHINE_NL2);
        for &(off, len) in &probes(n1, b) {
            assert_eq!(
                i1.read_range(&core1, off, len).unwrap(),
                &src1[off as usize..(off + len) as usize],
                "machine1 B={b} ({off},{len})"
            );
        }

        let i2 = BlockIndex::build_machine(&core2, n2, b, MACHINE_RANS0).unwrap();
        for &(off, len) in &probes(n2, b) {
            assert_eq!(
                i2.read_range(&core2, off, len).unwrap(),
                &src2[off as usize..(off + len) as usize],
                "machine2 B={b} ({off},{len})"
            );
        }
    }
}

#[test]
fn reads_are_exact_after_serialize_reopen() {
    let src = skewed(4096);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    for &b in &[1u32, 7, 256, 1024] {
        let index = BlockIndex::build_machine(&core, n, b, MACHINE_RANS0).unwrap();
        let reopened = BlockIndex::from_bytes(&index.to_bytes()).unwrap();
        assert_eq!(reopened, index);
        assert!(reopened.verify());
        for &(off, len) in &probes(n, b) {
            assert_eq!(
                reopened.read_range(&core, off, len).unwrap(),
                &src[off as usize..(off + len) as usize],
                "reopened B={b} ({off},{len})"
            );
        }
    }
}

#[test]
fn crafted_and_truncated_block_sections_are_rejected() {
    let src = skewed(2048);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    let index = BlockIndex::build_machine(&core, n, 128, MACHINE_RANS0).unwrap();
    let bytes = index.to_bytes();
    assert_eq!(&bytes[..8], &BLOCK_INDEX_MAGIC);
    assert!(bytes.len() >= BLOCK_INDEX_PREFIX_LEN);

    // Every truncation is a clean error.
    for cut in 0..bytes.len() {
        assert!(
            BlockIndex::from_bytes(&bytes[..cut]).is_err(),
            "truncation accepted at {cut}"
        );
    }
    // Trailing bytes are rejected.
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(BlockIndex::from_bytes(&trailing).is_err());
    // Bad magic, version and out-of-range block size.
    let mut bad = bytes.clone();
    bad[0] ^= 0xFF;
    assert!(BlockIndex::from_bytes(&bad).is_err());
    let mut bad = bytes.clone();
    bad[8] = 99;
    assert!(BlockIndex::from_bytes(&bad).is_err());
    let mut bad = bytes.clone();
    bad[14..18].copy_from_slice(&0u32.to_le_bytes());
    assert!(BlockIndex::from_bytes(&bad).is_err());
    // A flipped restart state fails the content digest.
    let mut bad = bytes.clone();
    bad[BLOCK_INDEX_PREFIX_LEN + 1] ^= 0xFF;
    assert!(BlockIndex::from_bytes(&bad).is_err());
}

#[test]
fn restart_certification_detects_tamper() {
    let src = skewed(3000);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    let index = BlockIndex::build_machine(&core, n, 256, MACHINE_RANS0).unwrap();
    assert!(index.certify(&core).unwrap());
    assert!(index.verify_against(&core).unwrap());

    // A tampered restart state breaks both certification paths.
    let mut tampered = index.clone();
    tampered.entries[2].state ^= 0xFFFF;
    assert!(!tampered.certify(&core).unwrap());
    assert!(!tampered.verify_against(&core).unwrap());

    // A tampered cursor is likewise rejected.
    let mut tampered = index.clone();
    tampered.entries[3].cursor = tampered.entries[3].cursor.wrapping_add(1);
    assert!(!tampered.certify(&core).unwrap());

    // Against a different core the linkage fails closed.
    let other = RANS0.encode(&skewed(1000)).unwrap();
    assert!(!index.verify_against(&other).unwrap());
}

#[test]
fn bounded_step_count_for_cold_machine2_reads() {
    let src = skewed(8192);
    let core = RANS0.encode(&src).unwrap();
    let n = src.len() as u64;
    for &b in &[16u32, 64, 256, 1024] {
        let index = BlockIndex::build_machine(&core, n, b, MACHINE_RANS0).unwrap();
        let bb = u64::from(b);
        for off in [0u64, 1, b as u64 - 1, b as u64, b as u64 + 5, n / 2, n - 4] {
            let len = 4.min(n - off);
            let (got, steps) = index.read_range_counted(&core, off, len).unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
            // Exact: `offset % B` skip steps plus `len` reveals.
            assert_eq!(steps, (off % bb) + len, "steps B={b} off={off}");
            assert!(steps <= index.max_steps(off, len));
            assert!(steps <= bb - 1 + len, "exceeds B-1+len bound");
        }
    }
}

#[test]
fn packed_checkpoint_roundtrip_and_backward_compatibility() {
    let src = lcg(0x0BAD_F00D_1234_5678, 3000);
    let core = ekbloom::machine::NL2.encode(&src).unwrap();
    let n = src.len() as u64;

    for &k in &[1024u32, 64, 16] {
        let cps = CheckpointSet::build(&core, n, k).unwrap();
        assert!(cps.packed());
        assert_eq!(cps.entry_bytes(), 2);
        assert_eq!(cps.physical_bytes(), cps.count * 2);

        let bytes = cps.to_bytes();
        assert_eq!(&bytes[..8], &ekbloom::checkpoint::CKPT_MAGIC);
        assert_eq!(bytes.len() as u64, 22 + cps.count * 2);
        assert_eq!(CheckpointSet::from_bytes(&bytes).unwrap(), cps);

        // Legacy layout (no magic) still reads back to the same set.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&cps.interval.to_le_bytes());
        legacy.extend_from_slice(&cps.count.to_le_bytes());
        for s in &cps.resume {
            legacy.extend_from_slice(&s.pack());
        }
        assert_eq!(legacy.len() as u64, 12 + cps.count * 12);
        assert_eq!(CheckpointSet::from_bytes(&legacy).unwrap(), cps);

        // Truncation of the compact form is always an error.
        for cut in 0..bytes.len() {
            assert!(CheckpointSet::from_bytes(&bytes[..cut]).is_err());
        }
    }

    // Machine 2 keeps complete 12-byte (state, cursor) restarts.
    let src2 = skewed(2048);
    let core2 = RANS0.encode(&src2).unwrap();
    let cps2 = CheckpointSet::build_with(&RANS0, &core2, src2.len() as u64, 64).unwrap();
    assert!(!cps2.packed());
    assert_eq!(cps2.entry_bytes(), 12);
    let bytes2 = cps2.to_bytes();
    assert_eq!(
        CheckpointSet::from_bytes_with(&RANS0, &bytes2).unwrap(),
        cps2
    );
}

/// A fresh temp directory, removed on drop.
struct TempDir {
    path: PathBuf,
}
static COUNTER: AtomicU64 = AtomicU64::new(0);
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ekbloom-blocks-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
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

#[test]
fn persisted_block_section_opens_verifies_and_reads() {
    let dir = TempDir::new("persist");
    let src = skewed(16 * 1024);
    let n = src.len() as u64;
    let core = RANS0.encode(&src).unwrap();
    let index = BlockIndex::build_machine(&core, n, 256, MACHINE_RANS0).unwrap();

    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    body.blocks.push(index);
    assert!(verify(&body).unwrap().ok);
    let physical = body.physical_bytes();

    let path = dir.join("blocks.ekb");
    write_body(&path, &body).unwrap();

    // In-memory reopen: linkage, verify, and byte-exact reads.
    let opened = open(&path).unwrap();
    assert_eq!(opened.blocks.len(), 1);
    assert_eq!(opened.physical_bytes(), physical);
    assert!(verify(&opened).unwrap().ok);
    assert_eq!(
        read(
            &opened,
            ReadRequest {
                offset: 0,
                length: n
            }
        )
        .unwrap(),
        src
    );
    let (off, len) = (n / 3, 4096u64);
    assert_eq!(
        read(
            &opened,
            ReadRequest {
                offset: off,
                length: len
            }
        )
        .unwrap(),
        &src[off as usize..(off + len) as usize]
    );

    // File-backed reopen: bounded block reads match the source too.
    let file_body = FileBody::open(&path).unwrap();
    for &(off, len) in &[(0u64, n), (0, 1), (n - 1, 1), (n / 2, 2048), (n, 0)] {
        assert_eq!(
            file_body
                .read(ReadRequest {
                    offset: off,
                    length: len
                })
                .unwrap(),
            &src[off as usize..(off + len) as usize],
            "file backed ({off},{len})"
        );
    }
    assert_eq!(file_body.inspect().unwrap(), ekbloom::inspect(&opened));
}

#[test]
fn planner_selects_bounded_block_restart_for_cold_machine2() {
    let src = skewed(256 * 1024);
    let n = src.len() as u64;
    let core = RANS0.encode(&src).unwrap();
    let index = BlockIndex::build_machine(&core, n, 256, MACHINE_RANS0).unwrap();
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    body.blocks.push(index);

    let cfg = CourtConfig {
        reps: 1,
        interleave: false,
        seed: 0x0B10_C5EE_D000_0001,
    };
    let plan = AccessPlan::plan(&body, &cfg).unwrap();
    assert!(plan.all_certified());

    // A far region is served by the bounded block-restart path.
    let far = n - 100;
    assert_eq!(plan.path_for(far), AccessPath::BlockRestart);

    let planner = Planner::new(plan);
    let (off, len) = (far, 100u64);
    let got = planner
        .read(
            &body,
            ReadRequest {
                offset: off,
                length: len,
            },
        )
        .unwrap();
    assert_eq!(got, &src[off as usize..(off + len) as usize]);

    // The cold read is bounded by the block size, not the offset.
    let (_, steps) = body.blocks[0].read_range_counted(&core, off, len).unwrap();
    assert!(
        steps <= 256 - 1 + len,
        "cold read not bounded: {steps} steps"
    );
    assert_eq!(steps, (off % 256) + len);
}

#[test]
fn machine1_is_unchanged() {
    let src = lcg(0x5151_5151_0000_0001, 2048);
    let core = ekbloom::machine::NL2.encode(&src).unwrap();
    let n = src.len() as u64;

    // Machine-1 block index is packed (2 bytes/state, cursor derived).
    let index = BlockIndex::build(&core, n, 64).unwrap();
    assert!(index.packed());
    assert_eq!(index.entry_bytes(), 2);
    assert!(index.verify_against(&core).unwrap());

    // Checkpoints are the packed 2-byte form, and the in-memory reader still
    // returns the authoritative bytes at every position.
    let cps = CheckpointSet::build(&core, n, 64).unwrap();
    assert_eq!(cps.physical_bytes(), cps.count * 2);
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    assert_eq!(body.machine_id, MACHINE_NL2);
    for i in [0u64, 1, 63, 64, 65, n - 1] {
        assert_eq!(
            read(
                &body,
                ReadRequest {
                    offset: i,
                    length: 1
                }
            )
            .unwrap(),
            vec![src[i as usize]]
        );
    }
}
