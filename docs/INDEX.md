# EKBLOOM — Documentation & Evidence Index

One-page index of every document and evidence artifact in the repository, with a
one-line description. Start with [`DESIGN.md`](DESIGN.md) for the frozen contract
and [`../README.md`](../README.md) for orientation.

## Docs (`docs/`)

| Doc | One line |
|---|---|
| [`DESIGN.md`](DESIGN.md) | **Frozen engineering contract** — machine semantics, state log, blooms, data model, `.ekb` wire layout, module interfaces. |
| [`ACCEPTANCE.md`](ACCEPTANCE.md) | The **10 hard acceptance gates**, each mapped to its implementing module, artifact and exact reproduction command, plus additional evidence. |
| [`PROOFS.md`](PROOFS.md) | **Proved properties P1–P9** (tagged PROVED / EMPIRICALLY OBSERVED) and the honest unresolved limitations. |
| [`RESULTS.md`](RESULTS.md) | **Measured results** — the speed-vs-space frontier, budget frontier, curves, hardware counters, and the 65,536-entry `T_4[x]` research outcome. |
| [`SECURITY.md`](SECURITY.md) | The **opt-in keyed-MAC `AUTH` mode**: threat model, exact coverage, and limitations. |
| [`FUSE.md`](FUSE.md) | The **read-only FUSE projection** of the in-process `OrFs` namespace (opt-in `fuse` feature). |
| [`LAYOUT.md`](LAYOUT.md) | **Layout & SIMD experiments** (§9/§23): five implemented, measured keep/reject decisions. |
| [`ADAPTIVE.md`](ADAPTIVE.md) | **Adaptive shells (O7), parallel different-shell readers (O8), memory tiers (O9)**. |
| [`INDEX.md`](INDEX.md) | This file. |

## Independent reviews (`docs/reviews/`)

| Review | One line |
|---|---|
| [`reviews/adversarial.md`](reviews/adversarial.md) | Try-to-break-correctness pass: 15 tests + a rev-2 segmented/budget/adaptive pass; fixes and residuals. |
| [`reviews/concurrency.md`](reviews/concurrency.md) | Concurrency audit + **Loom** model checking of the `OrFs` index critical section. |
| [`reviews/mathematics.md`](reviews/mathematics.md) | Independent re-derivation of P1–P6; only defect is a wording clause (benign). |
| [`reviews/performance.md`](reviews/performance.md) | Locality & SIMD investigation of the certified path-bloom hot path. |
| [`reviews/reproducibility.md`](reviews/reproducibility.md) | Independent reproducibility review: pinning, lockfile, clean-checkout, benchmark drift. |
| [`reviews/security.md`](reviews/security.md) | Wire-form / `unsafe` security review, plus the rev-2 segmented/budget/AVX-512 findings. |

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
| [`counterexamples/bloom-key-0001.json`](../evidence/counterexamples/bloom-key-0001.json) | The rejected universal `T_4[x]` witness (key `175`, two equal-state/different-outcome positions). |
| [`model-search/model-search-sweep.json`](../evidence/model-search/model-search-sweep.json) | The `D = 4` finite-model sufficiency sweep (`k = 0..4` conflicts `64/256/1024/4096/0`). |

## Reproduce (inside the pinned container only)

```sh
sh scripts/container-reproduce.sh   # clean build + full release test suite
sh scripts/container-verify.sh      # fmt + clippy -D warnings + tests
sh scripts/run-sweep.sh             # sweep/sweep-large/depth-metrics/memory-tiers/adaptive/cpu JSON
sh scripts/run-large-history.sh     # history-2p30.json
sh scripts/container-perf.sh        # perf.json
sh scripts/container-loom.sh        # Loom model checking
sh scripts/capture-environment.sh   # environment.txt
```
