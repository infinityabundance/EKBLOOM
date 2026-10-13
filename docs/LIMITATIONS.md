# EKBLOOM — honest limitations (one place)

This is the consolidated list of things EKBLOOM **does not** claim. Each item is a
non-claim with a one-line pointer to the committed evidence or the doc that measures it.
It gathers the "remaining (honest)" items from [`PROOFS.md`](PROOFS.md) (§Limitations),
the round notes in [`ACCEPTANCE.md`](ACCEPTANCE.md), and the per-topic docs, so a reader
never has to sweep the whole repository to find the boundaries.

The governing objective is **spend manifested space to maximise speed**, but the space
spent is only worth it where the evidence shows a real win — those boundaries are below.

## Compression / footprint

1. **Does not beat zstd on `V/N` for many corpora.** Order-0 modelling only compresses
   *skewed* data; on real text/binary data the pinned-hot zstd codec (0.36–0.74×) and LZ4
   (0.60–0.92×) are smaller than EKBLOOM (0.74–0.98×) at an equal budget. Pointer:
   `evidence/bench/e2e.json` (`holds_vs_pinned = false`), [`E2E.md`](E2E.md),
   [`CORPORA.md`](CORPORA.md), limitation 17.
2. **Uniform data gets no structural benefit.** On incompressible data the `auto` base is
   the verbatim machine 3 (`footprint == raw`, `V/N = 1.0000`), and any manifested
   structure is pure overhead — a whole-object body can reach **1.07× raw**. Pointer:
   `evidence/bench/optimizer.json` `uniform_evidence` (committed `== raw == 4,194,304`),
   `frontier.json` `uniform`, [`OPTIMIZER.md`](OPTIMIZER.md), limitation 12, P43.
3. **Machines 5 and 6 are explicit, non-default options** — and each loses where the
   other wins. Packed trades size for `O(1)` access (larger than order-0 rANS on
   low-entropy data; does not beat raw `memcpy`); Segmented wins **only** on heterogeneous
   objects, paying a per-segment header and a fixed directory on homogeneous ones.
   Pointer: `frontier.json` `packed`/`segmented`, [`PACKED_MACHINE.md`](PACKED_MACHINE.md),
   [`SEGMENTS.md`](SEGMENTS.md), limitation 18.
4. **The shared dictionary has a size boundary.** Machine 9 loses to per-record machine 2
   from ≈1 KiB/record onward on *heterogeneous* records (crossing between 256 B and
   1 KiB), and zstd-with-dictionary is still smaller/faster. Pointer:
   `evidence/bench/dictionary.json`, [`DICTIONARY.md`](DICTIONARY.md).

## Latency / throughput

5. **Does not win cold 4 KiB random access vs LZ4 (≈4.3×).** The best EKBLOOM decoder
   (machine 4, `K = 8`) cold-decodes a 4 KiB block in ≈**9.0 µs** vs LZ4's ≈**2.0 µs**,
   while storing ≈**3.2×** less. The ordered renormalising cursor and per-symbol gather
   are inherent to rANS; closing it needs a non-rANS symbol engine, not another variant.
   Pointer: `evidence/bench/decoders.json`, [`DECODERS.md`](DECODERS.md), P25, limitation 14.
6. **Sequential throughput is decode-bound when compressing.** On uniform data the chosen
   machine-3 core is a slice copy (raw-like, ≈69.6 GB/s), but the compressing machines are
   decode-bound (machine 2 ≈0.32 GB/s, machine 4 ≈0.48 GB/s), so the sequential axis is
   **not** a win where compression is actually used. Pointer: `optimizer.json`,
   `frontier.json` §5, [`FRONTIER.md`](FRONTIER.md), limitation 21, P25/P43.
7. **The decoder court closed the track with every variant rejected.** Word16 (machine 7,
   ≈6 % over machine 2 but ≈37 % under `K = 8`), independent lanes (machine 8, ≈+30 %) and
   the AVX2 rANS gather kernel (≈+25–27 %) are **all slower** than machine 4 `K = 8`.
   Pointer: `evidence/bench/decoders.json`, [`DECODERS.md`](DECODERS.md), P41/P42.
8. **SIMD rANS kernels are unwired and slow.** `vpgatherdd` is microcoded on this host;
   the AVX2 gather kernel stays in `src/simd.rs`, byte-exact to its scalar reference, on
   no default path. Pointer: `decoders.json`, [`KERNELS.md`](KERNELS.md), P42.
9. **Selective hot tiles require a skewed / predictable trace.** A `TileSet` has no
   checkpoint set, so a non-materialised read replays from `Ω₀` (cold 1-byte p50
   **10.9–12.4 ms** vs **40 ns** for the whole-object `P_32` surface), and on uniformly
   random access the trace names every tile so nothing is saved. Pointer:
   `evidence/bench/tiles.json`, [`TILES.md`](TILES.md), P13, limitation 9.
10. **`io_uring` is a batched-cold win, not a general or hot-read win.** The hybrid reader
    reaches **7.02×** a `pread` loop at 4 KiB / depth 64 but only parity (0.97–1.01×) at
    **low queue depth 1**, and the publication backend is ≈**2.2–2.3× slower** than the
    synchronous writer for a ~1 MiB body. Pointer: `evidence/bench/io-batch.json`,
    `io-uring.json`, [`IOURING.md`](IOURING.md), [`ASYNC.md`](ASYNC.md), limitation 7.
11. **No hot-read win vs raw/LRU or pinned-hot.** The path bloom ties `raw-vec` because its
    hot array *is* the raw output; the region-scoped competition loses hot to an ordinary
    LRU cache (871 vs 60 ns); pinned-hot codecs are ≈2× faster hot on real data. The
    engine's real win is **footprint at non-evicting hot access, not latency**. Pointer:
    `evidence/bench/competitive.json`, `frontier.json`, [`FRONTIER.md`](FRONTIER.md),
    [`COMPETITIVE.md`](COMPETITIVE.md), limitation 12.
12. **A 1-byte read pays a fixed dispatch residual.** The 4 KiB hot-read target (within 15 %
    of a raw `Vec`) is met only for reads at least a region long; a 1-byte read is ≈**1.5×**
    for the engine's `byte_at` dispatch (the borrowed `ResidentView` reaches 0.95–1.03×).
    Pointer: `frontier.json`, [`FRONTIER.md`](FRONTIER.md) §2, P26, limitation 12.

## Integrity, security, backends

13. **Integrity ≠ authenticity unless `AUTH` is used.** CRC-32/BLAKE3 detect accidental
    corruption, not forgery: a writer with file access can recompute them consistently.
    Segmented history states, `TileSet`s and `Certified` labels are trusted by
    `read`/`planner`; only the opt-in keyed-MAC `AUTH` mode (fail-closed
    `--require-auth`) is an authenticity boundary, and it has **no anti-rollback** and
    caller-managed keys. The trailing footer and the `AUTH` section's own bytes lie
    **outside** that literal MAC range and are cross-checked. Pointer:
    `evidence/bench/` + [`SECURITY.md`](SECURITY.md), [`reviews/security.md`](reviews/security.md),
    P27, limitations 8/13/16/19.
14. **FUSE passthrough is unsupported by the pinned `fuser`.** Letting the kernel serve a
    fully materialised output file via FUSE passthrough is a recorded **non-result** in
    this environment, as is FUSE-over-`io_uring`; the measured FUSE read-path cost is a
    marginal ≈1.2× on uncached 4 KiB and the genuine `ReadMode::Machine` decode baseline is
    **382×** the direct read. Pointer: `evidence/bench/fuse-perf.json`, [`FUSE.md`](FUSE.md),
    limitation 10.
15. **The GPU / accelerator boundary is not implemented.** `GpuBackend::available()` is
    `false` unconditionally; only the CPU-thread batch API exists (8.57× at 16 threads),
    and it is a plug-in boundary, not a performance claim. Pointer:
    `evidence/bench/batch.json`, [`GPU.md`](GPU.md), P28, limitation 15.
16. **The OrFS namespace carries a residual ≈1.5 % container overhead.** After the O-round
    compaction the real on-disk excess over the in-memory engine is **+1.54 % / +0.58 %**
    (from +1.89 %/+0.70 %); the framing itself is only ≈1.1 KB per object, and the excess
    is dominated by four allocated 4 KiB blocks (registry, two concealed directories, one
    pointer artifact) — removable only with a finer allocation unit. Pointer:
    `evidence/bench/orfs.json`, [`ORFS.md`](ORFS.md), P38/P44, limitation 22.

## Scope / research boundaries

17. **No certified universal 65,536-entry `T_4[x]` for the byte machine exists.** By P6 it
    cannot over arbitrary cores; the object-wise verdict is certified-or-counterexample
    and on pseudorandom input it is *rejected*, so no speedup is claimed there. Pointer:
    `evidence/counterexamples/`, `evidence/model-search/`, [`RESULTS.md`](RESULTS.md),
    limitation 1.
18. **Contextual `T_D[x,c]` is exact but enormous**, and **regional blooms are
    conditional** (certified only when a region's state→context map is unambiguous; on
    pseudorandom data they fall back). Pointer: `PROOFS.md` P4/P5, limitations 3/4.
19. **Empirical scope is single-host, single-run, container-capped**, with a fixed sample
    count; it is evidence, not a multi-platform study. CPU features are detected at
    runtime. Pointer: `evidence/environment.txt`, `evidence/bench/cpu.json`, limitation 5.
20. **The three sanctioned `unsafe` modules are real boundaries.** `src/simd.rs`,
    `src/async_io.rs` and `src/mapped.rs` (read-only `mmap`/`madvise`/`munmap`) each need
    care; the mapped surface must not be pointed at an untrusted or concurrently-truncated
    file (a truncation of an unfaulted page is a `SIGBUS`). Pointer: `tests/mmap.rs`,
    [`MMAP.md`](MMAP.md), [`reviews/security.md`](reviews/security.md), P17, limitation 11.
21. **Whole-object APIs are bounded by policy, not by the bounded read path.**
    `ResourcePolicy` (default **1 GiB**) turns a forged declared length into a typed
    `AllocationTooLarge`, but a caller that feeds an untrusted core to a *whole-object* API
    still allocates up to the ceiling; the bounded file-backed paths
    (`Engine`/`reader`/`BlockIndex`/tiles) are the recommended untrusted-input route.
    Pointer: [`SECURITY.md`](SECURITY.md), `src/resource.rs`, P40, limitation 20.

## P-round non-claims (v0.5)

22. **The dictionary id is not cryptographically bound in the bare machine.** Relabelling
    a *referenced* machine-9 core to a different **registered** id is not caught by the
    machine API (the record then decodes against the wrong model); a manifested body still
    catches it through the manifest `core_hash` linkage, and only the opt-in keyed-MAC
    `AUTH` closes it. Pointer: [`DICTIONARY.md`](DICTIONARY.md),
    [`reviews/security.md`](reviews/security.md) Revision 9, P49, limitation 23.
23. **Miri covers the safe subset (machines 1–8), not the three `unsafe` modules or machine
    9.** The SIMD intrinsics, the `io_uring` syscalls and the `mmap`/`madvise` surface are
    outside Miri's model, and the shared-dictionary machine is not in the Miri filter; they
    are pinned by `tests/simd.rs`/`tests/unsafe_review.rs`/`tests/mmap.rs`, the SAFE review
    and `tests/dictionary.rs`. Pointer: [`SAFETY.md`](SAFETY.md), [`ALLOC.md`](ALLOC.md),
    P4, limitation 24.

## See also

[`PROOFS.md`](PROOFS.md) §Limitations (the origin of items 1–23 above),
[`ACCEPTANCE.md`](ACCEPTANCE.md) (rounds L1–L4, M1–M5, N1–N8, O1–O3, P1–P8),
[`MACHINES.md`](MACHINES.md) (per-machine wins/loses) and
[`BUDGET-GUIDE.md`](BUDGET-GUIDE.md) §4 (when **not** to spend bytes).
