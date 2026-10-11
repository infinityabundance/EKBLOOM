# EKBLOOM — Documentation & Evidence Index

One-page index of every document and evidence artifact in the repository, with a
one-line description. Start with [`DESIGN.md`](DESIGN.md) for the frozen contract,
[`../README.md`](../README.md) for orientation, and
[`ACCEPTANCE.md`](ACCEPTANCE.md) for the reproduction gates.

## Docs (`docs/`)

| Doc | One line |
|---|---|
| [`DESIGN.md`](DESIGN.md) | **Frozen engineering contract** — machine semantics, state log, blooms, data model, `.ekb` wire layout, module interfaces. |
| [`ACCEPTANCE.md`](ACCEPTANCE.md) | The **10 hard acceptance gates**, each mapped to its implementing module, artifact and exact reproduction command, plus additional evidence. |
| [`PROOFS.md`](PROOFS.md) | **Proved properties P1–P23** (machine 1 + machine 2 + the round-3/round-4 modules, tagged PROVED / EMPIRICALLY OBSERVED) and the honest unresolved limitations. |
| [`RESULTS.md`](RESULTS.md) | **Measured results** — the speed-vs-space frontier, budget frontier, curves, hardware counters, machine 2, baselines, the cache competition, and the 65,536-entry `T_4[x]` research outcome. |
| [`CODEC.md`](CODEC.md) | **Machine 2** (`OrANS-RANS0`): the static order-0 range-ANS compressing machine (`V < N` on skewed data). |
| [`BASELINES.md`](BASELINES.md) | **Conventional baseline comparison** (`RawVec`/`RawFile`/`Predecoded`/`IndexedArray`) plus machine 2 vs `raw`. |
| [`COMPETITIVE.md`](COMPETITIVE.md) | **Region-scoped manifestation** vs an ordinary byte-budgeted LRU cache and raw/no-cache. |
| [`ASYNC.md`](ASYNC.md) | The optional **`io_uring` asynchronous publication backend** — contract, durability ordering, and its honest runtime status/speed. |
| [`SECURITY.md`](SECURITY.md) | The **opt-in keyed-MAC `AUTH` mode**: threat model, exact coverage, and limitations. |
| [`FUSE.md`](FUSE.md) | The **read-only FUSE projection** of the in-process `OrFs` namespace (opt-in `fuse` feature). |
| [`LAYOUT.md`](LAYOUT.md) | **Layout & SIMD experiments** (§9/§23): five implemented, measured keep/reject decisions. |
| [`ADAPTIVE.md`](ADAPTIVE.md) | **Adaptive shells (O7), parallel different-shell readers (O8), memory tiers (O9)**. |
| [`PREFIX.md`](PREFIX.md) | **Parallel-prefix reconstruction** of the `OrANS-NL2` core (affine block composition + associative prefix scan). |
| [`PACKED.md`](PACKED.md) | **Packed 16-bit restart/history storage** (machine 1): 1/6 and 1/2 the bytes. |
| [`TILES.md`](TILES.md) | **Selective certified hot-region byte tiles** vs whole-object `P_32`. |
| [`HANDLES.md`](HANDLES.md) | **Persistent verified handles + RCU publication**, persisted (`TILES`) sections. |
| [`IOURING.md`](IOURING.md) | The **`io_uring` hybrid batched read path** (`HybridReader`) plus the publication backend. |
| [`COURT.md`](COURT.md) | The **rigorous performance court**: interleaved reps, bootstrap 95% CIs, 3-D Pareto frontier. |
| [`PLANNER.md`](PLANNER.md) | The **certified, search-free access planner** (`§14` three-role separation) plus the **workload-directed background planner** (`§4`). |
| [`KERNELS.md`](KERNELS.md) | **SIMD reconstruction kernels** (directive §6): Kernel A/B, each kept or rejected with measured ratios. |
| [`MMAP.md`](MMAP.md) | **Read-only mapped surface + huge pages** (`src/mapped.rs`): mmap KEEP, huge pages rejected. |
| [`BLOCKS.md`](BLOCKS.md) | **Machine-2 independently restartable blocks** (+ compact packed checkpoints): bounded positional reconstruction, 38–330× vs genesis. |
| [`ENGINE.md`](ENGINE.md) | The **unified bounded-access engine** (`src/engine.rs`): `worst_case_steps`, five certified bounded paths, rejects unbounded bodies. |
| [`FRONTIER.md`](FRONTIER.md) | **The engine vs indexed codecs** (LZ4/Zstd ± hot cache) and raw under one equal budget, with the honest milestone verdict. |
| [`INDEX.md`](INDEX.md) | This file. |

## Independent reviews (`docs/reviews/`)

| Review | One line |
|---|---|
| [`reviews/adversarial.md`](reviews/adversarial.md) | Try-to-break-correctness pass: 15 tests, a rev-2 segmented/budget/adaptive pass, a rev-3 `prefix`/`packed`/`tiles`/`handle`/`planner`/`court` pass (11 tests + `fuzz_packed`/`fuzz_tiles`), and a rev-4 `blocks`/`engine`/region-cursor/`checkpoint` pass. |
| [`reviews/concurrency.md`](reviews/concurrency.md) | Concurrency audit + **Loom** model checking of the `OrFs` index critical section. |
| [`reviews/mathematics.md`](reviews/mathematics.md) | Independent re-derivation of P1–P6; only defect is a wording clause (benign). |
| [`reviews/performance.md`](reviews/performance.md) | Locality & SIMD investigation of the certified path-bloom hot path. |
| [`reviews/reproducibility.md`](reviews/reproducibility.md) | Independent reproducibility review: pinning, lockfile, clean-checkout, benchmark drift. |
| [`reviews/security.md`](reviews/security.md) | Wire-form security review through rev 4 (`blocks`/`engine`/`region_adapt`/`machine_rans`/`checkpoint`) and the **three** sanctioned `unsafe` modules (`src/simd.rs`, `src/async_io.rs`, `src/mapped.rs`), all SAFE. |

## Evidence (`evidence/`)

| Artifact | One line |
|---|---|
| [`environment.txt`](../evidence/environment.txt) | Pinned image id/digest, Dockerfile sha256, toolchain, kernel, CPU, `Cargo.lock` sha256, git HEAD. |
| [`bench/sweep.json`](../evidence/bench/sweep.json) | The speed-vs-manifested-space frontier (2 MiB corpus, 256 samples/point). |
| [`bench/cpu.json`](../evidence/bench/cpu.json) | CPU features + source provenance for the sweep. |
| [`bench/sweep-large.json`](../evidence/bench/sweep-large.json) | 4 MiB budget frontier, region/depth/checkpoint curves, cold starts, cache-resident throughput. |
| [`bench/depth-metrics.json`](../evidence/bench/depth-metrics.json) | Per-depth path-bloom metrics `D ∈ {1,2,4,8,16,32}` (§10). |
| [`bench/memory-tiers.json`](../evidence/bench/memory-tiers.json) | The O9 L1/L2/LLC/DRAM tier curve at constant `E_V = 2.13×`. |
| [`bench/adaptive.json`](../evidence/bench/adaptive.json) | The O7 adaptive-shell before/after demonstration. |
| [`bench/perf.json`](../evidence/bench/perf.json) | Hardware counters (cycles, IPC, cache/branch misses, bandwidth) under `CAP_PERFMON`. |
| [`bench/history-2p30.json`](../evidence/bench/history-2p30.json) | The 2^30 `HistoryStore` bounded-memory streaming build (4,294,967,300 bytes, peak RSS 2.9 MiB). |
| [`bench/baselines.json`](../evidence/bench/baselines.json) | Conventional direct-access baselines vs the `Root`/`Path{D=4}`/`Full` profiles. |
| [`bench/competitive.json`](../evidence/bench/competitive.json) | Region-scoped manifestation vs an LRU cache vs raw, under one corpus/trace/budget. |
| [`bench/io-uring.json`](../evidence/bench/io-uring.json) | The `io_uring` publication measurement (usable, byte-identical, and its wall-clock). |
| [`bench/prefix.json`](../evidence/bench/prefix.json) | Parallel-prefix reconstruction: block/thread strategies, construction cost, short-input warning, `state_at_via_prefix` vs replay. |
| [`bench/packed.json`](../evidence/bench/packed.json) | Packed 16-bit restarts/history vs existing forms: bytes, positional latency, locality, `state_from_hot`. |
| [`bench/tiles.json`](../evidence/bench/tiles.json) | Selective hot tiles (1/4/16/64 KiB, flat/radix) vs whole-object `P_32`, regional, and raw baselines. |
| [`bench/handles.json`](../evidence/bench/handles.json) | Cached handle vs reopen/reparse, concurrent readers under publication, persisted-tile `pread` cost. |
| [`bench/io-batch.json`](../evidence/bench/io-batch.json) | The `io_uring` batch path vs a serial `pread` loop and the cached handle, by size and depth. |
| [`bench/court.json`](../evidence/bench/court.json) | The performance court: 152 rows of distributions/95% CIs and the per-working-set 3-D Pareto frontier. |
| [`bench/planner.json`](../evidence/bench/planner.json) | The planner's chosen-path distribution and latency vs each fixed path on a mixed hot/cold body. |
| [`bench/kernels.json`](../evidence/bench/kernels.json) | SIMD reconstruction kernels (68 rows): Kernel A/B throughput/latency and keep/reject verdicts. |
| [`bench/planner-bg.json`](../evidence/bench/planner-bg.json) | The workload-directed background planner: moving hot window, adaptation recovery, additive publication. |
| [`bench/mmap.json`](../evidence/bench/mmap.json) | `pread`/resident/`mmap`/`mmap_huge`: latency, faults, and the huge-page non-result. |
| [`bench/rans.json`](../evidence/bench/rans.json) | Machine-2 ready decode context: prepared vs per-symbol rebuild (**165×** on skewed), byte-exact. |
| [`bench/blocks.json`](../evidence/bench/blocks.json) | Machine-2 restartable blocks (256 B–64 KiB) vs genesis and the packed checkpoint container (**1/6**). |
| [`bench/frontier.json`](../evidence/bench/frontier.json) | The engine vs indexed LZ4/Zstd (± cache) and raw: hot index ratio, footprint, non-eviction advantage. |
| [`bench/fuse-perf.json`](../evidence/bench/fuse-perf.json) | Measured FUSE read-path cost vs a direct read and the `FUSE` passthrough / `FUSE`-over-`io_uring` non-result. |
| [`counterexamples/bloom-key-0001.json`](../evidence/counterexamples/bloom-key-0001.json) | The rejected universal `T_4[x]` witness (key `175`, two equal-state/different-outcome positions). |
| [`model-search/model-search-sweep.json`](../evidence/model-search/model-search-sweep.json) | The `D = 4` finite-model sufficiency sweep (`k = 0..4` conflicts `64/256/1024/4096/0`). |

## Reproduce (inside the pinned container only)

```sh
sh scripts/container-reproduce.sh   # clean build + full release test suite
sh scripts/container-verify.sh      # fmt + clippy -D warnings + tests
sh scripts/run-sweep.sh             # sweep/sweep-large/depth-metrics/memory-tiers/adaptive/cpu JSON
sh scripts/run-large-history.sh     # history-2p30.json
sh scripts/run-baselines.sh         # baselines.json
sh scripts/run-competitive.sh       # competitive.json
sh scripts/container-perf.sh        # perf.json
sh scripts/container-async-io.sh    # io-uring.json (seccomp=unconfined); also probes + tests/io_batch
sh scripts/run-io.sh                # io-batch.json (seccomp=unconfined)
sh scripts/run-prefix.sh            # prefix.json
sh scripts/run-packed.sh            # packed.json
sh scripts/run-tiles.sh             # tiles.json
sh scripts/run-handles.sh           # handles.json
sh scripts/run-court.sh             # court.json + planner.json
sh scripts/run-kernels.sh           # kernels.json
sh scripts/run-planner-bg.sh        # planner-bg.json
sh scripts/run-mmap.sh              # mmap.json
sh scripts/run-rans.sh              # rans.json (machine-2 prepared decode context)
sh scripts/run-blocks.sh            # blocks.json (machine-2 blocks + packed checkpoints)
sh scripts/run-frontier.sh          # frontier.json (engine vs indexed codecs)
sh scripts/run-fuse-perf.sh         # fuse-perf.json (requires /dev/fuse)
sh scripts/validate-evidence.sh     # validate every evidence/**/*.json (27 files)
sh scripts/container-fuzz.sh        # all libFuzzer targets (fuzz_machine/open/bloom/history_store/packed/tiles/blocks)
sh scripts/container-loom.sh        # Loom model checking
sh scripts/capture-environment.sh   # environment.txt
```
