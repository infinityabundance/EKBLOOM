//! Batched independent reconstruction with an explicit accelerator boundary.
//!
//! Every EKBLOOM machine step is an **irreducible serial dependent chain within one
//! window** — symbol `i` cannot be revealed before symbol `i - 1` (and, for
//! `OrANS-NL2`, before `i - 2`). There is, however, a second axis that is fully
//! parallel: **across windows**. Given each window's exact restart state and its
//! own encoded bytes, every window reconstructs on its own, with no cross-window
//! dependency whatsoever. A batch of `N` such jobs is therefore embarrassingly
//! parallel, and — crucially — each job is a *fixed-size, self-contained, already
//! resident* input/output pair, which is exactly the shape a GPU accelerates.
//!
//! [`BatchReconstructor`] runs such a batch on the **CPU-threads** backend, byte-exact
//! to the authoritative machine: machine 1 (the default) uses the sanctioned scalar
//! NL2 step, and the other machines use their own prepared
//! [`crate::machine::Prepared`] step. The accelerator boundary is made
//! explicit by the [`AcceleratorBackend`] trait: a future GPU backend would
//! implement it and receive the same [`BatchJob`] slice. **None is implemented and
//! none is faked** — [`GpuBackend::available`] is `false` in this environment, and
//! [`BatchReconstructor::run`] never silently takes an accelerator path.
//!
//! See `docs/GPU.md` for the honest keep/reject verdict and
//! `evidence/bench/batch.json` for the measured throughput and scaling.

use std::thread;

use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, prepare, OrMachine, MACHINE_NL2};
use crate::state::Snapshot;

/// The processing backend a [`BatchReconstructor`] runs on.
///
/// Exactly one backend is implemented: safe CPU threads. The solver is deliberately
/// an enum (rather than a bare flag) so a future accelerator can be added as a
/// variant without changing the public surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Safe, scoped CPU threads (`std::thread::scope`); one disjoint job range per
    /// worker. Byte-identical for every thread count.
    CpuThreads,
}

impl Backend {
    /// Stable, human-readable backend label.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::CpuThreads => "cpu-threads",
        }
    }

    /// Whether this backend can execute on the current host. The CPU backend always
    /// can.
    pub fn available(&self) -> bool {
        match self {
            Backend::CpuThreads => true,
        }
    }
}

/// The contract a future accelerator (GPU) backend would implement.
///
/// It exists to make the boundary explicit: [`BatchReconstructor::run`] hands a
/// backend an ordered slice of independent [`BatchJob`]s and receives results in
/// the same order, so a GPU backend would be a drop-in for the CPU-threads backend
/// without touching the semantics.
pub trait AcceleratorBackend {
    /// Stable, human-readable backend label.
    fn name(&self) -> &'static str;
    /// Whether this backend can execute here. A real backend returns `true` only
    /// when it has a usable device and runtime; it must never over-report.
    fn available(&self) -> bool;
}

/// The (unimplemented) GPU backend.
///
/// This type exists only to **state the contract** and to report honestly that no
/// accelerator path is taken. It is intentionally not wired into
/// [`BatchReconstructor::run`]: the batch API is the boundary such a backend would
/// plug into, not a place where an untested path is silently selected.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuBackend;

impl AcceleratorBackend for GpuBackend {
    fn name(&self) -> &'static str {
        "gpu (unimplemented)"
    }

    /// `false`: no accelerator backend is implemented, so even on a host with a
    /// device this path could not execute (and none is present here). This is the
    /// honest answer, not a stub that would flip to `true`.
    fn available(&self) -> bool {
        false
    }
}

/// The single process-wide GPU backend handle (always [`GpuBackend::available`] `=`
/// `false`).
pub static GPU_BACKEND: GpuBackend = GpuBackend;

/// Report whether the GPU backend is available in this environment.
///
/// `false`: no accelerator backend is implemented and no device/runtime is
/// reachable. Callers must not treat this as a promise of future availability.
pub fn accelerator_available() -> bool {
    GPU_BACKEND.available()
}

/// Best-effort detection of an accelerator **device** in this environment.
///
/// This is a real probe of the common device nodes, not a hard-coded stub; in the
/// pinned, capability-dropped container none exists, so it returns `false`. A
/// device being present would not by itself make [`GPU_BACKEND`] available — the
/// backend is unimplemented — so this is reported for context only.
pub fn accelerator_device_present() -> bool {
    const CANDIDATES: [&str; 4] = [
        "/dev/nvidiactl",
        "/dev/nvidia0",
        "/dev/dri/renderD128",
        "/dev/kfd",
    ];
    CANDIDATES
        .iter()
        .any(|path| std::path::Path::new(path).exists())
}

/// One independent reconstruction job: a known restart state and the encoded bytes
/// it consumes.
///
/// `start.cursor` is the byte offset into `encoded` at which the first symbol is
/// read, and `out_len` is the number of symbols to reveal. For a fresh window or
/// block the cursor is `0` and `encoded` is the window's own bytes — a
/// self-contained job. For the model-carrying machines (`OrANS-RANS0`,
/// `OrANS-RANS0I`) `encoded` must begin at the machine's model header (pass the
/// whole core with an absolute cursor), because the prepared decoder parses that
/// header once per job. Jobs are independent: no job observes another's state.
#[derive(Clone, Copy, Debug)]
pub struct BatchJob<'a> {
    /// The exact resumable state at which this job starts.
    pub start: Snapshot,
    /// The encoded bytes this job reveals from (`start.cursor` indexes into it).
    pub encoded: &'a [u8],
    /// The number of symbols to reveal.
    pub out_len: u64,
}

/// Reconstruct MANY INDEPENDENT `(start_state, encoded-block)` windows in parallel.
///
/// Results are independent and their order is preserved: `out[i]` is the
/// reconstruction of `jobs[i]` for every thread count. The default machine is the
/// authoritative `OrANS-NL2`; [`BatchReconstructor::with_machine`] selects another.
pub struct BatchReconstructor {
    backend: Backend,
    threads: usize,
    machine_id: u32,
    machine: &'static dyn OrMachine,
}

impl BatchReconstructor {
    /// Construct a CPU-threads reconstructor over the authoritative `OrANS-NL2`
    /// machine. `threads` is clamped to `1 ..= jobs.len()` at run time.
    pub fn new(threads: usize) -> Self {
        let machine = machine_by_id(MACHINE_NL2).expect("machine 1 is always registered");
        BatchReconstructor {
            backend: Backend::CpuThreads,
            threads,
            machine_id: MACHINE_NL2,
            machine,
        }
    }

    /// Construct a CPU-threads reconstructor over the machine with `machine_id`.
    ///
    /// This is the machine-general form of [`BatchReconstructor::new`]; it lets the
    /// same batch API reconstruct `OrANS-RANS0` (`2`), `OrANS-Store` (`3`) and
    /// `OrANS-RANS0I` (`4`) jobs. An unknown id is rejected.
    pub fn with_machine(threads: usize, machine_id: u32) -> Result<Self> {
        let machine =
            machine_by_id(machine_id).ok_or(EkbloomError::UnknownMachine { id: machine_id })?;
        Ok(BatchReconstructor {
            backend: Backend::CpuThreads,
            threads,
            machine_id,
            machine,
        })
    }

    /// The selected backend. Always [`Backend::CpuThreads`]: no accelerator path is
    /// ever taken.
    #[inline]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The configured worker count (clamped to the batch size at run time).
    #[inline]
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// The machine identifier this reconstructor decodes with.
    #[inline]
    pub fn machine_id(&self) -> u32 {
        self.machine_id
    }

    /// Reconstruct every job in `jobs`, writing the results into `out` with the
    /// order preserved (`out[i]` for `jobs[i]`).
    ///
    /// `out.len()` must equal `jobs.len()`. Each `out[i]` is cleared and filled with
    /// exactly `jobs[i].out_len` symbols; the output is byte-exact to the
    /// authoritative machine for the job's restart state. Workers decode disjoint
    /// contiguous job ranges, so no state is shared and the result does not depend
    /// on `threads` (or on scheduling).
    pub fn run(&self, jobs: &[BatchJob], out: &mut [Vec<u8>]) -> Result<()> {
        if jobs.len() != out.len() {
            return Err(EkbloomError::InvalidArgument {
                what: format!("batch jobs {} != output slots {}", jobs.len(), out.len()),
            });
        }
        let n = jobs.len();
        if n == 0 {
            return Ok(());
        }
        let workers = self.threads.max(1).min(n);
        if workers == 1 {
            // Avoid a scoped-thread spawn for the single-worker case; the spawn is
            // pure overhead here.
            for (job, dst) in jobs.iter().zip(out.iter_mut()) {
                decode_job(self.machine_id, self.machine, job, dst)?;
            }
            return Ok(());
        }
        let chunk = n.div_ceil(workers);
        let machine = self.machine;
        let machine_id = self.machine_id;
        thread::scope(|scope| -> Result<()> {
            let handles: Vec<_> = jobs
                .chunks(chunk)
                .zip(out.chunks_mut(chunk))
                .map(|(job_chunk, out_chunk)| {
                    scope.spawn(move || -> Result<()> {
                        for (job, dst) in job_chunk.iter().zip(out_chunk.iter_mut()) {
                            decode_job(machine_id, machine, job, dst)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            for handle in handles {
                handle.join().map_err(|_| EkbloomError::InvalidArgument {
                    what: "batch reconstruction worker panicked".to_string(),
                })??;
            }
            Ok(())
        })
    }
}

/// Decode one job into `dst`.
///
/// Machine 1 (`OrANS-NL2`) is header-less and its cursor equals the logical
/// position, so it is decoded with the sanctioned scalar step
/// [`crate::simd::nl2_step`] — byte-identical to [`crate::machine::Nl2::reveal`] but
/// statically dispatched. Every other machine is decoded with its own prepared
/// [`crate::machine::Prepared`] step, exactly as the authoritative reader would.
fn decode_job(
    machine_id: u32,
    machine: &dyn OrMachine,
    job: &BatchJob,
    dst: &mut Vec<u8>,
) -> Result<()> {
    dst.clear();
    let cursor = job.start.cursor;
    if cursor > job.encoded.len() as u64 {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "job cursor {cursor} is beyond the encoded block of {} bytes",
                job.encoded.len()
            ),
        });
    }
    if machine_id == MACHINE_NL2 {
        let n = usize::try_from(job.out_len).map_err(|_| EkbloomError::Overflow {
            what: "batch job length",
        })?;
        let start = cursor as usize;
        let end = start.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "batch job end",
        })?;
        let bytes = job
            .encoded
            .get(start..end)
            .ok_or(EkbloomError::CoreTooShort {
                needed: end as u64,
                available: job.encoded.len() as u64,
            })?;
        dst.reserve(n);
        let mut x = job.start.state as u16;
        for &b in bytes {
            let (s, next) = crate::simd::nl2_step(x, b);
            dst.push(s);
            x = next;
        }
        return Ok(());
    }
    let ctx = prepare(machine, job.encoded)?;
    let mut omega = job.start;
    ctx.decode_into(job.encoded, &mut omega, job.out_len, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::NL2;

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

    #[test]
    fn small_batch_is_exact_and_order_preserving() {
        // Four independent windows from their own restart states.
        let first = lcg(0xB47C_0001, 7);
        let second = lcg(0xB47C_0002, 13);
        let third = lcg(0xB47C_0003, 1);
        let fourth = lcg(0xB47C_0004, 32);
        let jobs = [
            BatchJob {
                start: Snapshot::new(0, 0),
                encoded: &first,
                out_len: first.len() as u64,
            },
            BatchJob {
                start: Snapshot::new(0x1234, 0),
                encoded: &second,
                out_len: second.len() as u64,
            },
            BatchJob {
                start: Snapshot::new(0xFFFF, 0),
                encoded: &third,
                out_len: third.len() as u64,
            },
            BatchJob {
                start: Snapshot::new(0x00AB, 0),
                encoded: &fourth,
                out_len: fourth.len() as u64,
            },
        ];
        let mut out = vec![Vec::new(); jobs.len()];
        BatchReconstructor::new(4).run(&jobs, &mut out).unwrap();
        for (job, got) in jobs.iter().zip(out.iter()) {
            let mut omega = job.start;
            let mut want = Vec::with_capacity(job.out_len as usize);
            for _ in 0..job.out_len {
                want.push(NL2.reveal(job.encoded, &mut omega).unwrap());
            }
            assert_eq!(got, &want);
        }
    }

    #[test]
    fn empty_batch_is_ok() {
        BatchReconstructor::new(4).run(&[], &mut []).unwrap();
    }

    #[test]
    fn mismatched_output_length_is_rejected() {
        let src = lcg(1, 4);
        let jobs = [BatchJob {
            start: Snapshot::new(0, 0),
            encoded: &src,
            out_len: 4,
        }];
        let mut out = vec![Vec::new(); 2];
        assert!(BatchReconstructor::new(4).run(&jobs, &mut out).is_err());
    }

    #[test]
    fn gpu_backend_is_unavailable_and_not_selected() {
        assert!(!GPU_BACKEND.available());
        assert!(!accelerator_available());
        for threads in [1usize, 2, 8] {
            let rec = BatchReconstructor::new(threads);
            assert_eq!(rec.backend(), Backend::CpuThreads);
            assert!(rec.backend().available());
        }
    }
}
