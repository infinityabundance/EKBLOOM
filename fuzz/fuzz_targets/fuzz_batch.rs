#![no_main]
//! Fuzz the batched independent reconstructor (`batch::BatchReconstructor`).
//!
//! The batch API must never panic on arbitrary jobs, must be byte-exact to the
//! authoritative per-machine `reveal` for machine 1 (`OrANS-NL2`) and machine 3
//! (`OrANS-Store`), must preserve order, and must produce identical output for
//! every worker count. `accelerator_available()` must stay `false` so no
//! accelerator path is silently selected.

use ekbloom::batch::{accelerator_available, Backend, BatchJob, BatchReconstructor};
use ekbloom::machine::{machine_by_id, MACHINE_NL2, MACHINE_STORE};
use ekbloom::state::Snapshot;
use libfuzzer_sys::fuzz_target;

/// Upper bound on the number of windows decoded per campaign iteration.
const MAX_JOBS: usize = 24;

fuzz_target!(|data: &[u8]| {
    // The accelerator boundary is honest: never available here.
    assert!(!accelerator_available());

    let m = data.len().min(256);
    let src = &data[..m];

    // 1. Machine 3 (verbatim): core == source, so a window's restart cursor is
    //    its start position and its output is the corresponding source slice.
    {
        let machine = machine_by_id(MACHINE_STORE).unwrap();
        let core = match machine.encode(src) {
            Ok(c) => c,
            Err(_) => return,
        };
        let window = 1 + (data.first().copied().unwrap_or(0) as usize % 32);
        let mut jobs: Vec<BatchJob> = Vec::new();
        let mut off = 0usize;
        while off < core.len() && jobs.len() < MAX_JOBS {
            let end = (off + window).min(core.len());
            jobs.push(BatchJob {
                start: Snapshot::new(0, off as u64),
                encoded: &core,
                out_len: (end - off) as u64,
            });
            off = end;
        }
        let covered = off;
        if !jobs.is_empty() {
            for threads in [1usize, 3, 8] {
                let mut out = vec![Vec::new(); jobs.len()];
                let rec = BatchReconstructor::with_machine(threads, MACHINE_STORE).unwrap();
                assert_eq!(rec.backend(), Backend::CpuThreads);
                rec.run(&jobs, &mut out).unwrap();
                let joined: Vec<u8> = out.iter().flatten().copied().collect();
                assert_eq!(joined, &src[..covered], "machine3 order/thread {threads}");
            }
        }
    }

    // 2. Machine 1 (NL2): each window carries the restart state at its start
    //    position; the cursor is the window offset into the whole core.
    let nl2 = machine_by_id(MACHINE_NL2).unwrap();
    let core = match nl2.encode(src) {
        Ok(c) => c,
        Err(_) => return,
    };
    let window = 1 + (data.first().copied().unwrap_or(0) as usize % 32);
    let mut jobs: Vec<BatchJob> = Vec::new();
    let mut starts: Vec<Snapshot> = Vec::new();
    let mut off = 0usize;
    while off < core.len() && jobs.len() < MAX_JOBS {
        let mut omega = nl2.init(&core).unwrap();
        for _ in 0..off {
            nl2.reveal(&core, &mut omega).unwrap();
        }
        let end = (off + window).min(core.len());
        jobs.push(BatchJob {
            start: omega,
            encoded: &core,
            out_len: (end - off) as u64,
        });
        starts.push(omega);
        off = end;
    }
    let covered = off;
    if !jobs.is_empty() {
        for threads in [1usize, 2, 5, 16] {
            let mut out = vec![Vec::new(); jobs.len()];
            BatchReconstructor::new(threads)
                .run(&jobs, &mut out)
                .unwrap();
            let joined: Vec<u8> = out.iter().flatten().copied().collect();
            assert_eq!(joined, &src[..covered], "machine1 order/thread {threads}");
            for (i, job) in jobs.iter().enumerate() {
                let mut omega = starts[i];
                let mut want = Vec::new();
                for _ in 0..job.out_len {
                    want.push(nl2.reveal(&core, &mut omega).unwrap());
                }
                assert_eq!(out[i], want, "machine1 per-job/thread {threads}");
            }
        }
    }

    // 3. A job whose cursor is beyond its encoded block is a clean `Err`.
    let bad = BatchJob {
        start: Snapshot::new(0, core.len() as u64 + 1),
        encoded: &core,
        out_len: 1,
    };
    let mut out = vec![Vec::new()];
    assert!(BatchReconstructor::new(2).run(&[bad], &mut out).is_err());

    // 4. A mismatched output length is a clean `Err`.
    let one = BatchJob {
        start: Snapshot::new(0, 0),
        encoded: &core,
        out_len: 0,
    };
    let mut two = vec![Vec::new(); 2];
    assert!(BatchReconstructor::new(2).run(&[one], &mut two).is_err());
});
