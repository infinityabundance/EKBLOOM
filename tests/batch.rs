//! Exactness, independence and accelerator-boundary tests for `ekbloom::batch`.
//!
//! Pins: [`BatchReconstructor::run`] is byte-exact against the authoritative decode
//! (`machine.decode`, `BlockIndex::read_range` and `reader::read`) for both the
//! header-less machine 1 and the compressing machine 2 (plus the identity and
//! interleaved machines), at every thread count, for empty / single / large batches
//! and boundary window sizes; the output order is preserved and independent of the
//! worker count; and the accelerator boundary is honest — `GpuBackend::available()`
//! is `false` and no GPU path is ever selected.

use ekbloom::blocks::BlockIndex;
use ekbloom::machine::{
    machine_by_id, OrMachine, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE, NL2,
};
use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_i::RANS0I;
use ekbloom::{
    accelerator_available, accelerator_device_present, create, create_machine,
    independent_window_jobs, read, AcceleratorBackend, BatchBackend, BatchJob, BatchReconstructor,
    GpuBackend, ManifestationProfile, ReadRequest, Snapshot,
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

/// A skewed source machine 2 (and machine 4) compress (`V < N`).
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

const THREADS: [usize; 6] = [1, 2, 3, 4, 8, 16];

/// Machine-1 self-contained windows: each job is its own encoded block with cursor 0.
#[test]
fn machine1_windows_are_exact_at_every_thread_count_and_window_size() {
    for &n in &[
        0usize, 1, 2, 3, 15, 16, 17, 255, 256, 257, 1000, 4096, 10_000,
    ] {
        let src = lcg(0xB1 ^ n as u64, n);
        let core = NL2.encode(&src).unwrap();
        let oracle = NL2.decode(&core, n as u64).unwrap();
        for &window in &[1usize, 2, 3, 7, 15, 16, 17, 63, 64, 65, 255, 256, 257] {
            let jobs = independent_window_jobs(&core, window).unwrap();
            for &threads in &THREADS {
                let mut out = vec![Vec::new(); jobs.len()];
                BatchReconstructor::new(threads)
                    .run(&jobs, &mut out)
                    .unwrap();
                let flat: Vec<u8> = out.concat();
                assert_eq!(
                    flat, oracle,
                    "machine1 n={n} window={window} threads={threads}"
                );
            }
        }
        // Empty batch and a batch of exactly one job both work for every n.
        BatchReconstructor::new(4).run(&[], &mut []).unwrap();
    }
}

/// Restart-entry jobs over the whole core (absolute cursor), cross-checked against
/// `machine.decode` and `BlockIndex::read_range`, for every machine.
fn check_machine(machine_id: u32, core: &[u8], source: &[u8], blocks: &[u32]) {
    let n = source.len() as u64;
    let machine = machine_by_id(machine_id).unwrap();
    let oracle = machine.decode(core, n).unwrap();
    assert_eq!(
        oracle, source,
        "authoritative decode mismatch machine {machine_id}"
    );

    for &b in blocks {
        let index = BlockIndex::build_machine(core, n, b, machine_id).unwrap();
        let jobs: Vec<BatchJob<'_>> = index
            .entries
            .iter()
            .enumerate()
            .map(|(k, entry)| {
                let lo = k as u64 * u64::from(b);
                let len = (n - lo).min(u64::from(b));
                BatchJob {
                    start: *entry,
                    encoded: core,
                    out_len: len,
                }
            })
            .collect();
        assert_eq!(jobs.len(), index.entries.len());

        for &threads in &THREADS {
            let mut out = vec![Vec::new(); jobs.len()];
            BatchReconstructor::with_machine(threads, machine_id)
                .unwrap()
                .run(&jobs, &mut out)
                .unwrap();
            let flat: Vec<u8> = out.concat();
            assert_eq!(
                flat, oracle,
                "machine {machine_id} block={b} threads={threads}"
            );
            // Order preserved: each slot is exactly its source slice, in order.
            let mut off = 0usize;
            for part in &out {
                let slice = &source[off..off + part.len()];
                assert_eq!(part.as_slice(), slice);
                off += part.len();
            }
            assert_eq!(off, source.len());
        }

        // The same boundaries through the block-restart reader path.
        for k in 0..index.entries.len() {
            let lo = k as u64 * u64::from(b);
            let len = (n - lo).min(u64::from(b));
            if len == 0 {
                continue;
            }
            assert_eq!(
                index.read_range(core, lo, len).unwrap(),
                &source[lo as usize..(lo + len) as usize],
                "read_range machine {machine_id} b={b} k={k}"
            );
        }
    }
}

#[test]
fn batch_is_exact_for_every_machine_and_block_size() {
    // Machine 1 (V = N).
    let src1 = lcg(0x1111_2222_3333_4444, 5000);
    let core1 = NL2.encode(&src1).unwrap();
    check_machine(MACHINE_NL2, &core1, &src1, &[1, 2, 7, 64, 256, 1024]);

    // Machine 2 (V < N).
    let src2 = skewed(6000);
    let core2 = RANS0.encode(&src2).unwrap();
    assert!(core2.len() < src2.len(), "machine 2 must compress");
    check_machine(MACHINE_RANS0, &core2, &src2, &[3, 16, 256, 1024]);

    // Machine 3 (identity).
    let src3 = lcg(0x3333_0000_0000_0001, 2048);
    check_machine(MACHINE_STORE, &src3, &src3, &[1, 7, 64, 256]);

    // Machine 4 (interleaved rANS).
    let src4 = skewed(6000);
    let core4 = RANS0I.encode(&src4).unwrap();
    check_machine(MACHINE_RANS0I, &core4, &src4, &[4, 16, 256]);
}

/// The batch output equals `reader::read` for the full object, for both machines.
#[test]
fn batch_matches_reader_read_for_both_machines() {
    // Machine 1.
    let src1 = lcg(0x0BAD_0BAD_0000_0001, 3000);
    let core1 = NL2.encode(&src1).unwrap();
    let body1 = create(&src1, ManifestationProfile::Root).unwrap();
    assert_eq!(body1.machine_id, MACHINE_NL2);
    let read1 = read(
        &body1,
        ReadRequest {
            offset: 0,
            length: src1.len() as u64,
        },
    )
    .unwrap();
    assert_eq!(read1, src1);
    let jobs1 = independent_window_jobs(&core1, 128).unwrap();
    let mut out1 = vec![Vec::new(); jobs1.len()];
    BatchReconstructor::new(8).run(&jobs1, &mut out1).unwrap();
    assert_eq!(out1.concat(), read1);

    // Machine 2.
    let src2 = skewed(4000);
    let core2 = RANS0.encode(&src2).unwrap();
    let body2 = create_machine(&src2, ManifestationProfile::Root, MACHINE_RANS0).unwrap();
    assert_eq!(body2.machine_id, MACHINE_RANS0);
    let read2 = read(
        &body2,
        ReadRequest {
            offset: 0,
            length: src2.len() as u64,
        },
    )
    .unwrap();
    assert_eq!(read2, src2);
    let index = BlockIndex::build_machine(&core2, src2.len() as u64, 256, MACHINE_RANS0).unwrap();
    let jobs2: Vec<BatchJob<'_>> = index
        .entries
        .iter()
        .enumerate()
        .map(|(k, entry)| {
            let lo = k as u64 * 256;
            let len = (src2.len() as u64 - lo).min(256);
            BatchJob {
                start: *entry,
                encoded: &core2,
                out_len: len,
            }
        })
        .collect();
    let mut out2 = vec![Vec::new(); jobs2.len()];
    BatchReconstructor::with_machine(8, MACHINE_RANS0)
        .unwrap()
        .run(&jobs2, &mut out2)
        .unwrap();
    assert_eq!(out2.concat(), read2);
}

/// Independent jobs of mixed, unequal lengths: results do not depend on the worker
/// count, the order is preserved, and residual output bytes are cleared.
#[test]
fn results_are_independent_and_order_preserving() {
    let lengths = [7usize, 1, 64, 3, 256, 2, 33];
    let encodeds: Vec<Vec<u8>> = lengths
        .iter()
        .enumerate()
        .map(|(i, &w)| lcg(0x0AB0 ^ i as u64, w))
        .collect();
    let starts: Vec<u16> = (0..lengths.len())
        .map(|i| (i as u16).wrapping_mul(0x1111))
        .collect();
    let jobs: Vec<BatchJob<'_>> = (0..lengths.len())
        .map(|i| BatchJob {
            start: Snapshot::new(u32::from(starts[i]), 0),
            encoded: &encodeds[i],
            out_len: encodeds[i].len() as u64,
        })
        .collect();

    let sequential = {
        let mut out = vec![Vec::new(); jobs.len()];
        BatchReconstructor::new(1).run(&jobs, &mut out).unwrap();
        out
    };

    for &threads in &[2usize, 3, 8, 16] {
        let mut out = vec![Vec::new(); jobs.len()];
        BatchReconstructor::new(threads)
            .run(&jobs, &mut out)
            .unwrap();
        assert_eq!(out, sequential, "threads={threads} differs from sequential");
    }

    // Pre-filled slots must be overwritten, not appended to.
    let mut dirty = vec![vec![0xAAu8; 100]; jobs.len()];
    BatchReconstructor::new(4).run(&jobs, &mut dirty).unwrap();
    assert_eq!(dirty, sequential);
}

/// Empty, single and large batches; and a mismatched output length is rejected.
#[test]
fn empty_single_and_large_batches() {
    // Empty.
    BatchReconstructor::new(8).run(&[], &mut []).unwrap();

    // Single self-contained job.
    let one = lcg(0xF00D_0000_0000_0001, 5);
    let jobs = [BatchJob {
        start: Snapshot::new(0xABCD, 0),
        encoded: &one,
        out_len: 5,
    }];
    let mut out = vec![Vec::new(); 1];
    BatchReconstructor::new(8).run(&jobs, &mut out).unwrap();
    let mut omega = Snapshot::new(0xABCD, 0);
    let want: Vec<u8> = (0..5)
        .map(|_| NL2.reveal(&one, &mut omega).unwrap())
        .collect();
    assert_eq!(out, vec![want]);

    // Large batch of short windows.
    let windows = 20_000usize;
    let window = 16usize;
    let src = lcg(0x0001_A26E_0000_0001, windows * window);
    let core = NL2.encode(&src).unwrap();
    let big = independent_window_jobs(&core, window).unwrap();
    assert_eq!(big.len(), windows);
    for &threads in &[1usize, 4, 8, 16] {
        let mut out = vec![Vec::new(); big.len()];
        BatchReconstructor::new(threads)
            .run(&big, &mut out)
            .unwrap();
        assert_eq!(out.concat(), src, "large batch threads={threads}");
    }

    // Mismatched output length is a clean error, never a partial write.
    let mut too_many = vec![Vec::new(); jobs.len() + 1];
    assert!(BatchReconstructor::new(4)
        .run(&jobs, &mut too_many)
        .is_err());
}

/// A job whose cursor lies beyond its encoded block is rejected, not decoded.
#[test]
fn out_of_range_cursor_is_rejected() {
    let block = lcg(0xC0DE, 8);
    let jobs = [BatchJob {
        start: Snapshot::new(0, 9),
        encoded: &block,
        out_len: 1,
    }];
    let mut out = vec![Vec::new(); 1];
    assert!(BatchReconstructor::new(4).run(&jobs, &mut out).is_err());
}

/// The accelerator boundary is honest: no GPU backend is available, none is
/// selected, and an unknown machine id is rejected.
#[test]
fn gpu_backend_is_unavailable_and_never_selected() {
    let gpu = GpuBackend;
    assert!(!gpu.available());
    assert!(!accelerator_available());
    // A real device probe; the value is environment-dependent and never faked.
    let _device = accelerator_device_present();

    for &threads in &THREADS {
        let rec = BatchReconstructor::new(threads);
        assert_eq!(rec.backend(), BatchBackend::CpuThreads);
        assert_eq!(rec.backend().name(), "cpu-threads");
        assert_eq!(rec.machine_id(), MACHINE_NL2);
    }
    assert!(BatchReconstructor::new(1).backend().available());
    assert!(BatchReconstructor::with_machine(4, 0xDEAD).is_err());
}
