# EKBLOOM `io_uring` (`src/async_io.rs`)

This document covers the two uses of `io_uring` in EKBLOOM:

1. the **publication backend** (`Backend::IoUring`, covered in `docs/ASYNC.md`); and
2. the **hybrid batched read path** (`HybridReader`), which the reviewer's warning
   — *"io_uring is not automatically faster; it wins for concurrent/batched cold
   I/O, loses for single hot reads"* — is the design premise for.

It is measured by `examples/io_batch.rs`, run inside the pinned container by
`sh scripts/run-io.sh` (also invoked from `sh scripts/container-async-io.sh`), and
written to `evidence/bench/io-batch.json`. Every number below is that committed
evidence; none is estimated.

## 1. Design

```rust
pub struct BatchRead { pub offset: u64, pub length: u64 }
pub struct AlignedBuffer { /* aligned heap buffer */ }

pub struct HybridReader { /* one File + an optional ring */ }
impl HybridReader {
    pub fn open(path) -> io::Result<HybridReader>;            // ring if available
    pub fn open_pread_only(path) -> io::Result<HybridReader>; // never a ring
    pub fn open_direct(path) -> io::Result<HybridReader>;     // O_DIRECT, uncached
    pub fn read_direct(&self, offset, length) -> io::Result<Vec<u8>>; // pread only
    pub fn read_batch(&mut self, reqs) -> io::Result<Vec<Vec<u8>>>;   // owned buffers
    pub fn read_batch_into(&mut self, reqs, bufs) -> io::Result<()>;  // aligned buffers
    pub fn evict_page_cache(&mut self, offset, length) -> io::Result<()>;
    pub fn ring_available(&self) -> bool;
    pub fn ring_submissions(&self) -> u64;
    pub fn fallback_batches(&self) -> u64;
}
```

The split is deliberate:

- **Single / hot reads go direct.** `read_direct` is a plain `pread(2)`; it never
  submits anything to the ring. The test
  `tests/io_batch.rs::single_hot_reads_never_touch_the_ring` pins this
  structurally (the ring-submission counter is unchanged after 64 single reads).
- **Batched cold reads go through the ring.** `read_batch` allocates an owned
  buffer per request, submits one `IORING_OP_READ` SQE per request (pointing at
  that buffer), waits for the completions, matches each completion back to its
  request by `user_data`, and returns the buffers **indexed like the request
  slice** — completion order never affects the result. A batch larger than the
  ring's submission capacity is chunked and still ordered.
- **The fallback is real and identical.** When no ring can be created (no
  `io_uring`, or a seccomp policy denying `io_uring_setup(2)`), the batch path is a
  serial `pread` loop returning exactly the same bytes; a batch and its fallback
  are asserted byte-for-byte equal.
- **`O_DIRECT` for genuine coldness.** `open_direct` opens with `O_DIRECT` and
  `read_batch_into` writes into caller-owned [`AlignedBuffer`]s (allocated with
  only safe Rust), so a cold read truly bypasses the page cache.

`unsafe` remains confined to this module and is only the raw-buffer pointers handed
to `SubmissionQueue::push`, each with a `// SAFETY:` justification.

### Why `O_DIRECT`, not `FADV_DONTNEED`

The first version evicted the page cache with `FADV_DONTNEED` through the ring
before each batch. The `cache_probe` in the evidence shows this is **ineffective
in this container** — the page-cached and "cold" `pread` latencies were the same.
The benchmark therefore uses `O_DIRECT`, where the separation is unambiguous
(4 KiB: 131 ns cached vs 23,444 ns uncached; 1 MiB: 43,181 ns vs 448,560 ns).
`HybridReader::evict_page_cache` is retained as a best-effort API but is *not* what
the cold rows rely on.

## 2. Measurement

**One corpus** — deterministic 64 MiB LCG source (seed `0xEB10_0000_C0DE_2026`),
written both as a raw file (`corpus.bin`, read with `O_DIRECT`) and as an `.ekb`
fully materialised with 1 MiB tiles (for the warm cached-handle row). Container
capped at 8 CPUs / 8 GiB.

Four paths per `(size, queue_depth)`: `io_uring` batch (uncached, `O_DIRECT`),
serial `pread` loop (uncached, `O_DIRECT`), the warm `OpenObject` cached handle
(one bounded `pread` per request from the page cache), and the in-memory `TileSet`
copy (no syscall). Work is warmed and verified byte-exact before timing; iterations
are chosen per configuration to read ~96 MiB.

**Robustness note.** The host is a shared NVMe that occasionally stalls for
milliseconds. IOPS below is therefore derived from the **median per-op latency**
(stall-robust); the JSON also records `wall_iops` (aggregate over the whole region)
so the difference is visible. On a clean run the two agree closely.

### Results (`evidence/bench/io-batch.json`)

`×` is `io_uring / pread`. p50 in µs. CPU is ns per completed op.

| size | depth | io_uring IOPS | pread IOPS | × vs pread | io_uring p50 | pread p50 | io_uring GB/s | io_uring CPU | pread CPU |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 4 KiB | 1 | 49,855 | 51,557 | **0.97×** | 20.1 | 19.4 | 0.20 | 5,804 | 5,130 |
| 4 KiB | 4 | 169,837 | 67,159 | 2.53× | 5.9 | 14.9 | 0.70 | 1,691 | 2,491 |
| 4 KiB | 16 | 329,707 | 71,424 | 4.62× | 3.0 | 14.0 | 1.35 | 802 | 1,713 |
| 4 KiB | 32 | 371,609 | 71,159 | 5.22× | 2.7 | 14.1 | 1.52 | 672 | 1,638 |
| 4 KiB | 64 | 496,278 | 70,706 | **7.02×** | 2.0 | 14.1 | 2.03 | 570 | 1,537 |
| 64 KiB | 1 | 23,217 | 23,121 | **1.00×** | 43.1 | 43.3 | 1.52 | 6,840 | 6,616 |
| 64 KiB | 4 | 34,000 | 23,192 | 1.47× | 29.4 | 43.1 | 2.23 | 3,167 | 3,640 |
| 64 KiB | 16 | 36,681 | 12,536 | 2.93× | 27.3 | 79.8 | 2.40 | 2,784 | 3,314 |
| 64 KiB | 32 | 35,020 | 14,748 | 2.37× | 28.6 | 67.8 | 2.30 | 2,599 | 2,830 |
| 64 KiB | 64 | 46,876 | 16,278 | **2.88×** | 21.3 | 61.4 | 3.07 | 2,628 | 2,835 |
| 1 MiB | 1 | 2,258 | 2,246 | **1.01×** | 442.9 | 445.3 | 2.37 | 24,686 | 29,489 |
| 1 MiB | 4 | 2,903 | 2,269 | 1.28× | 344.4 | 440.7 | 3.04 | 30,200 | 24,622 |
| 1 MiB | 16 | 3,434 | 2,250 | 1.53× | 291.2 | 444.4 | 3.60 | 29,920 | 26,149 |
| 1 MiB | 32 | 3,532 | 2,252 | 1.57× | 283.1 | 444.0 | 3.70 | 34,477 | 23,250 |
| 1 MiB | 64 | 3,549 | 2,246 | **1.58×** | 281.8 | 445.1 | 3.72 | 40,119 | 23,206 |

Warm references (no I/O), for scale:

| size | cached handle p50 | cached IOPS | in-memory tile p50 | in-memory IOPS |
|---:|---:|---:|---:|---:|
| 4 KiB | 0.27–0.36 µs | 2.8–3.8 M | 0.07–0.10 µs | 9.9–14.1 M |
| 64 KiB | 1.2–1.8 µs | 0.55–0.83 M | 0.63–0.92 µs | 1.09–1.58 M |
| 1 MiB | 26–53 µs | 18.9–38.7 k | 14–18 µs | 24.3–69.7 k |

## 3. Verdict (honest, per regime)

- **Single read (depth 1): `io_uring` does not win — it ties and can lose.** At
  depth 1 it is **0.97× / 1.00× / 1.01×** the serial `pread` loop for 4 KiB /
  64 KiB / 1 MiB. One submit-and-wait round trip buys no concurrency, so the ring
  is pure overhead over one `pread`. This is exactly the reviewer's warning,
  measured.
- **Deep queues: `io_uring` wins, and wins most for small random reads.** At depth
  64 it is **7.02×** the `pread` loop at 4 KiB (496,278 vs 70,706 IOPS; p50 2.0 µs
  vs 14.1 µs), **2.88×** at 64 KiB, and **1.58×** at 1 MiB. The advantage appears
  by depth 4–16 and saturates near depth 32–64 as the device queue fills.
- **CPU per completed op follows the same shape.** At 4 KiB depth 64 the ring uses
  **570 ns/op** vs `pread`'s **1,537 ns/op** (~2.7× less) because one wait serves
  64 operations; the gap narrows with size and reverses at 1 MiB, where the larger
  buffer traffic dominates the ring's per-op CPU.
- **A warm read should never go through the ring.** The warm cached handle serves a
  4 KiB read in **0.27 µs** and the in-memory tile in **0.07 µs**, versus the
  *cold* ring's **2.0 µs** at its best (depth 64) — the ring is **~7.4×** slower
  than the cached handle and **~25×** slower than the in-memory copy. `read_direct`
  keeps single/hot reads off the ring for exactly this reason.
- **Scale caveat.** These are storage-bound cold numbers; absolute IOPS depend on
  the host NVMe. The *direction* (parity at depth 1, large gains at depth ≥ 16,
  warm handles far ahead) is stable across runs; exact ratios vary by run/device.

## 4. Mixed reads and concurrent publication

`tests/io_batch.rs::mixed_reads_and_concurrent_publication_through_the_ring` runs a
reader submitting batched reads through one ring while the same process appends
generations through the publication ring. It asserts that every committed
generation verifies (`data → fsync → footer` ordering is preserved), that the
intermediate states are complete — no reader ever observes a partial shell — and
that the logical content is byte-stable throughout, with the final state
verifiable and byte-exact.

## 5. Tests

`tests/io_batch.rs` (4 tests, `io-uring` feature): a shuffled mixed-size batch
returns byte-exact data indexed by request and a batch larger than the ring's
capacity is chunked while staying ordered; the `pread` fallback returns exactly the
bytes the ring path returns; single/hot reads never submit to the ring; and batched
reads interleaved with concurrent generation publication never see a partial shell.
Under the default container (seccomp denies `io_uring_setup`) these run the
transparent fallback; under `scripts/container-async-io.sh` they run the real ring.

## 6. Reproduce

```sh
sh scripts/container-verify.sh     # fmt + clippy(-D warnings) + full test suite
sh scripts/container-async-io.sh   # probe stages + tests/async_io + tests/io_batch + run-io
sh scripts/run-io.sh               # writes evidence/bench/io-batch.json
```

`run-io.sh` relaxes exactly one policy — `--security-opt seccomp=unconfined` — so
`io_uring_setup(2)` is permitted; no capability and no `--privileged` are needed.
