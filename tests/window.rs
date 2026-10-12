//! Bounded encoded-window reads for cursor-divergent machines (2 and 4).
//!
//! Pins: a file-backed read of a range served from a certified restart fetches
//! only a bounded encoded window (`<= 2 * (steps + symbols) + margin` bytes),
//! never the whole encoded core; the decoded bytes are byte-exact against both
//! the source and the authoritative `reader::read`, for every request size and
//! boundary, before and after serialize + reopen; the same holds through a
//! checkpoint restart; and machines 1 and 3 are unaffected.

use ekbloom::blocks::BlockIndex;
use ekbloom::machine::{OrMachine, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE};
use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_i::RANS0I;
use ekbloom::{
    create_machine, open, read, write_body, CheckpointSet, FileBody, ManifestationProfile,
    ReadRequest, WINDOW_MARGIN,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Skewed byte source that machines 2 and 4 compress (`V < N`).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(match r {
            0..=69 => b'a',
            70..=91 => b'b',
            _ => b'c',
        });
    }
    out
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

struct TempDir {
    path: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ekbloom-window-it-{}-{}-{}",
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

/// In-bounds `(offset, length)` probes including block boundaries, a partial
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

/// Build, write and reopen a block-indexed `.ekb` for `src` and `machine_id`.
fn write_blocks(dir: &TempDir, name: &str, src: &[u8], machine_id: u32, b: u32) -> (PathBuf, u64) {
    let machine = ekbloom::machine::machine_by_id(machine_id).unwrap();
    let core = machine.encode(src).unwrap();
    let n = src.len() as u64;
    let index = BlockIndex::build_machine(&core, n, b, machine_id).unwrap();
    let mut body = create_machine(src, ManifestationProfile::Root, machine_id).unwrap();
    body.blocks.push(index);
    let path = dir.join(name);
    write_body(&path, &body).unwrap();
    (path, n)
}

#[test]
fn windowed_block_reads_are_exact_and_bounded_for_machine2() {
    let dir = TempDir::new("m2");
    let src = skewed(0x0B10_C5EE_0000_0002, 128 * 1024);
    let n = src.len() as u64;
    let core = RANS0.encode(&src).unwrap();
    assert!(core.len() < src.len(), "machine 2 must compress");

    for &b in &[1u32, 64, 256, 4096] {
        let (path, _) = write_blocks(&dir, &format!("m2-{b}.ekb"), &src, MACHINE_RANS0, b);
        let reference = open(&path).unwrap();
        let body = FileBody::open(&path).unwrap();
        for &(off, len) in &probes(n, b) {
            let before = body.fetched_bytes();
            let got = body
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .unwrap();
            let fetched = body.fetched_bytes() - before;
            assert_eq!(
                got,
                &src[off as usize..(off + len) as usize],
                "B={b} ({off},{len})"
            );
            assert_eq!(
                got,
                read(
                    &reference,
                    ReadRequest {
                        offset: off,
                        length: len
                    }
                )
                .unwrap(),
                "reader mismatch B={b} ({off},{len})"
            );
            if len > 0 {
                let steps = (off % u64::from(b)) + len;
                assert!(
                    fetched <= 2 * steps + WINDOW_MARGIN,
                    "B={b} ({off},{len}) fetched {fetched} > 2*{steps}+{WINDOW_MARGIN}"
                );
                // A small read far from the core end must not read the whole core.
                if len <= 4096 && off < n / 2 && core.len() as u64 > 2 * (4096 + 4096) {
                    assert!(
                        fetched < core.len() as u64,
                        "B={b} ({off},{len}) fetched whole core"
                    );
                }
            }
        }
    }
}

#[test]
fn windowed_block_reads_are_exact_and_bounded_for_machine4() {
    let dir = TempDir::new("m4");
    let src = skewed(0x0B10_C5EE_0000_0004, 128 * 1024);
    let n = src.len() as u64;
    let core = RANS0I.encode(&src).unwrap();

    for &b in &[1u32, 64, 256, 4096] {
        let (path, _) = write_blocks(&dir, &format!("m4-{b}.ekb"), &src, MACHINE_RANS0I, b);
        let reference = open(&path).unwrap();
        let body = FileBody::open(&path).unwrap();
        for &(off, len) in &probes(n, b) {
            let before = body.fetched_bytes();
            let got = body
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .unwrap();
            let fetched = body.fetched_bytes() - before;
            assert_eq!(
                got,
                &src[off as usize..(off + len) as usize],
                "B={b} ({off},{len})"
            );
            assert_eq!(
                got,
                read(
                    &reference,
                    ReadRequest {
                        offset: off,
                        length: len
                    }
                )
                .unwrap(),
                "reader mismatch B={b} ({off},{len})"
            );
            if len > 0 {
                let steps = (off % u64::from(b)) + len;
                assert!(fetched <= 2 * steps + WINDOW_MARGIN, "B={b} ({off},{len})");
                if len <= 4096 && off < n / 2 && core.len() as u64 > 2 * (4096 + 4096) {
                    assert!(
                        fetched < core.len() as u64,
                        "B={b} ({off},{len}) fetched whole core"
                    );
                }
            }
        }
    }
}

#[test]
fn windowed_reads_survive_serialize_and_reopen() {
    let dir = TempDir::new("reopen");
    let src = skewed(0x0B10_C5EE_0000_0042, 32 * 1024);
    let n = src.len() as u64;
    for &machine_id in &[MACHINE_RANS0, MACHINE_RANS0I] {
        let (path, _) = write_blocks(
            &dir,
            &format!("reopen-{machine_id}.ekb"),
            &src,
            machine_id,
            256,
        );
        // Reopen from a fresh serialized generation and re-probe.
        let body = FileBody::open(&path).unwrap();
        assert_eq!(body.logical_len(), n);
        for &(off, len) in &probes(n, 256) {
            assert_eq!(
                body.read(ReadRequest {
                    offset: off,
                    length: len
                })
                .unwrap(),
                &src[off as usize..(off + len) as usize],
                "machine {machine_id} ({off},{len})"
            );
        }
    }
}

#[test]
fn windowed_checkpoint_reads_are_exact_and_bounded() {
    let dir = TempDir::new("ckpt");
    let src = skewed(0x0B10_C5EE_0000_0099, 64 * 1024);
    let n = src.len() as u64;
    for &machine_id in &[MACHINE_RANS0, MACHINE_RANS0I] {
        let machine = ekbloom::machine::machine_by_id(machine_id).unwrap();
        let core = machine.encode(&src).unwrap();
        let cp = CheckpointSet::build_with(machine, &core, n, 256).unwrap();
        let mut body = create_machine(
            &src,
            ManifestationProfile::Sparse { interval: 256 },
            machine_id,
        )
        .unwrap();
        body.checkpoints = vec![cp];
        let path = dir.join(&format!("ckpt-{machine_id}.ekb"));
        write_body(&path, &body).unwrap();
        let reference = open(&path).unwrap();
        let file_body = FileBody::open(&path).unwrap();

        for &(off, len) in &probes(n, 256) {
            let before = file_body.fetched_bytes();
            let got = file_body
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .unwrap();
            let fetched = file_body.fetched_bytes() - before;
            assert_eq!(
                got,
                &src[off as usize..(off + len) as usize],
                "machine {machine_id} ({off},{len})"
            );
            assert_eq!(
                got,
                read(
                    &reference,
                    ReadRequest {
                        offset: off,
                        length: len
                    }
                )
                .unwrap()
            );
            if len > 0 {
                let steps = (off % 256) + len;
                assert!(
                    fetched <= 2 * steps + WINDOW_MARGIN,
                    "machine {machine_id} ({off},{len}) fetched {fetched}"
                );
            }
        }
    }
}

#[test]
fn machines_one_and_three_are_unaffected() {
    let dir = TempDir::new("m13");
    let src = lcg(0x5151_5151_0000_0001, 16 * 1024);
    let n = src.len() as u64;
    for &(tag, machine_id, b) in &[("m1", MACHINE_NL2, 64u32), ("m3", MACHINE_STORE, 64)] {
        let (path, _) = write_blocks(&dir, &format!("{tag}.ekb"), &src, machine_id, b);
        let reference = open(&path).unwrap();
        let body = FileBody::open(&path).unwrap();
        assert_eq!(body.logical_len(), n);
        for &(off, len) in &probes(n, b) {
            let got = body
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .unwrap();
            assert_eq!(
                got,
                &src[off as usize..(off + len) as usize],
                "{tag} ({off},{len})"
            );
            assert_eq!(
                got,
                read(
                    &reference,
                    ReadRequest {
                        offset: off,
                        length: len
                    }
                )
                .unwrap()
            );
        }
    }
}
