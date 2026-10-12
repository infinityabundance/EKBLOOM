# EKBLOOM — batched independent reconstruction and the accelerator boundary

**Module:** [`src/batch.rs`](../src/batch.rs) · **Example:** [`examples/batch.rs`](../examples/batch.rs)
· **Evidence:** [`evidence/bench/batch.json`](../evidence/bench/batch.json)
· **Tests:** [`tests/batch.rs`](../tests/batch.rs) · **Regenerate:** `sh scripts/run-batch.sh`

This document closes the last reconstruction limitation with an honest keep/reject
verdict, and states the GPU position precisely rather than aspirationally.

---

## 1. The shape of the parallelism

Every EKBLOOM machine step is an **irreducible serial dependent chain within one
window**: symbol `i` cannot be revealed before `i - 1` (and for `OrANS-NL2`, before
`i - 2`). `docs/KERNELS.md` §0/§3 and `docs/PREFIX.md` §1 established that no
intra-window ILP or auto-vectorisation is available, and that the only real axis is
**across independent windows/blocks** — one 16-bit lane per window.

`src/batch.rs` makes that axis a first-class, machine-general API:

```rust
pub struct BatchReconstructor { backend: Backend, threads: usize }
pub enum Backend { CpuThreads }
pub struct BatchJob<'a> { pub start: Snapshot, pub encoded: &'a [u8], pub out_len: u64 }
impl BatchReconstructor {
    pub fn new(threads: usize) -> Self;                 // authoritative OrANS-NL2
    pub fn with_machine(threads, machine_id) -> Result<Self>;
    pub fn run(&self, jobs: &[BatchJob], out: &mut [Vec<u8>]) -> Result<()>;
}
```

A job is **self-contained**: `start` is the exact resumable state, `start.cursor`
indexes `encoded`, and `out_len` symbols are revealed. For header-less machines
(`OrANS-NL2`, `OrANS-Store`) a job is literally its own window bytes with cursor
`0`; for the model-carrying machines (`OrANS-RANS0`, `OrANS-RANS0I`) `encoded`
must begin at the model header (pass the whole core with an absolute cursor). No job
observes any other, so a batch is embarrassingly parallel and the result does not
depend on the worker count. `run` decodes every job byte-exact to the authoritative
machine: machine 1 (the default) uses the sanctioned scalar step
[`crate::simd::nl2_step`] (identical to [`crate::machine::Nl2::reveal`], statically
dispatched), and the other machines use their own prepared
[`crate::machine::Prepared`] step.

The machine-1 bridge [`crate::kernels::independent_window_jobs`] turns a core into
exactly this job slice (prefix scan → one `(state, block)` job per window).

---

## 2. Measured (CPU threads)

`examples/batch.rs`, run inside the pinned container by `sh scripts/run-batch.sh`,
reconstructs a **large batch of independent machine-1 windows** and checks every
output byte-for-byte against `NL2::decode` before timing. `scaling_vs_t1` is the
ratio of throughput to the single-thread row; throughput is
`bytes / ns` (numerically GB/s, as in `docs/PREFIX.md`). Real numbers, from
`evidence/bench/batch.json`:

| threads | windows/s | GB/s | cyc/B | scaling vs 1t | exact |
|---:|---:|---:|---:|---:|---|
| 1 | 2 832 299 | 0.725 | 7.22 | 1.00× | true |
| 2 | 5 682 795 | 1.455 | 3.60 | 2.01× | true |
| 4 | 11 356 679 | 2.907 | 1.80 | 4.01× | true |
| 8 | 19 601 970 | 5.018 | 1.04 | 6.92× | true |
| 16 | 24 272 976 | 6.214 | 0.84 | 8.57× | true |

These are the committed values from `evidence/bench/batch.json` (corpus blake3
`acc6d383…873901`, clock estimate 5.23 GHz, median of 5 reps). Scaling is
**near-linear to 4 threads (4.01×)**, **6.92× at 8** and **8.57× at 16**, i.e. it
tracks the core count — the property a GPU would extend to thousands of lane
users. The container exposes 16 logical CPUs (`pinning 0-15`) under an 8-CPU quota,
so the 16-thread row's extra gain is SMT, not 16 physical cores.

**Verdict.** The batch scales with cores — the property a GPU would extend to
thousands of SM lanes. Because jobs are independent, equal-sized and already
resident, this workload is the *right* shape for device offload; nothing about the
CPU implementation is a ceiling on the concept.

---

## 3. The accelerator boundary (honest)

```rust
pub trait AcceleratorBackend { fn name(&self) -> &'static str; fn available(&self) -> bool; }
pub struct GpuBackend; // implements the trait
```

- **A GPU backend is NOT implemented.** The trait exists only to document the
  contract; `GpuBackend` is never wired into `run`.
- **No accelerator device is available here.** `GpuBackend::available()` returns
  `false` unconditionally — no accelerator backend exists, no device/runtime is
  present in the pinned, capability-dropped container, and the crate links no
  accelerator library. `accelerator_device_present()` is a real device-node probe
  and also reports `false`. Nothing is faked and no GPU path is silently taken:
  `BatchReconstructor::run` always uses the [`Backend::CpuThreads`] backend, and
  `tests/batch.rs` pins both facts.

### Why a GPU is **not** the default for single-byte host latency

For EKBLOOM's primary access pattern — one symbol or a small range, served from
host memory — a GPU is the wrong tool. A single read is dominated by host↔device
transfer setup and kernel-launch/scheduling overhead (microseconds), while the
in-memory machine step is nanoseconds; the `docs/FRONTIER.md` and `docs/PREFIX.md`
results (positional state in ~100 ns, decode at ~0.5–4.4 GB/s) are already at the
latency scale where any dispatch cost would swamp the work. The synchronization
required to return one byte to the caller would cost more than computing it on the
CPU.

### Why it **is** a plausible extension for very large independent batches

The batch API is exactly the boundary where a GPU becomes plausible. When the batch
is large, the jobs are independent and self-contained, and the encoded cores plus
restart states are **already resident in device memory**, the per-job dispatch cost
amortises across the whole batch and the serial dependent chain is irrelevant (each
lane runs its own window). Concretely, the workload is: `N` independent
`(u32 state, ≤ block bytes)` inputs producing `N` fixed-size outputs — a
memory-bound, regular, branch-light kernel that maps directly onto GPU lanes. That
is the extension this API anticipates; implementing it is explicitly out of scope
here, and no performance claim is made for it.

**Bottom line.** CPU-thread batched reconstruction is implemented, byte-exact and
measured to scale with cores. A GPU backend is *not* implemented, is reported as
unavailable truthfully, and would plug in at this boundary — worthwhile for large
resident-independent batches, not for single-byte host latency.

---

## 4. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-batch.sh          # writes evidence/bench/batch.json
```
