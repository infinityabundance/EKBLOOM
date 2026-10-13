# EKBLOOM — Acceptance Gates

This document maps each of the 10 hard acceptance gates from the implementation
directive (`research/EKBLOOM-CODEC.md` §26) to the module that implements it, the
tests/commands that prove it, and the evidence artifact it produces.

All commands run **inside the pinned container only** (never on the host). Each
`sh scripts/container-test.sh ...` invocation forwards its arguments to
`cargo test`; `--release` matches the reproducible path used by
`scripts/container-reproduce.sh`. The one-command reproduction of all gates is:

```sh
sh scripts/container-reproduce.sh     # clean build + full test suite
sh scripts/run-sweep.sh               # regenerate the benchmark evidence
```

Environment for every run: [`../evidence/environment.txt`](../evidence/environment.txt).

---

## Gate 1 — Executable state machine

Complete machine, correct for arbitrary byte input.

- **Implements:** `src/machine.rs` (`Nl2`, `OrMachine`, `MACHINE_NL2`),
  `src/machine_rans.rs` (`Rans0`, `MACHINE_RANS0`, machine 2).
- **Artifact:** machine unit tests in `src/machine.rs`; `tests/properties.rs`
  (`machine_roundtrip`); `tests/correctness.rs` (`category_*`);
  `tests/machine_rans.rs` (machine 2 roundtrip, compression, header validation).
- **Proves:**
  - unit tests in `src/machine.rs`: `sbox_is_bijective`, `roundtrip_various`,
    `genesis_matches_log0_zero`, `state_is_last_two_symbols`, `peek_matches_reveal`;
  - `tests/properties.rs::machine_roundtrip` (arbitrary `Vec<u8>`);
  - `tests/correctness.rs` categories (`category_every_byte_value`,
    `category_empty_input`, `category_single_symbol`, `category_lcg_pseudorandom`);
  - `tests/machine_rans.rs::roundtrip_exact_for_every_shape`,
    `skewed_source_compresses`, `random_source_is_not_compressed`,
    `invalid_model_headers_error_cleanly`, `machine_one_is_unaffected`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release machine::
  sh scripts/container-test.sh --release --test properties machine_roundtrip
  sh scripts/container-test.sh --release --test correctness
  sh scripts/container-test.sh --release --test machine_rans
  ```

---

## Gate 2 — Non-withdrawal history

Committed history retained without overwriting earlier states.

- **Implements:** `src/history.rs` (append-only `states`), `src/history_store.rs`
  (segmented streaming projection), `src/checkpoint.rs`, additive `manifest`/`grow`
  in `src/manifest.rs`.
- **Artifact:** `tests/history_checkpoint.rs`, `tests/history_store.rs`,
  `tests/manifest_read_verify.rs`, `tests/format_roundtrip.rs`; streaming-store
  evidence [`../evidence/bench/history-2p30.json`](../evidence/bench/history-2p30.json).
- **Proves:**
  - `tests/history_checkpoint.rs::history_positions_and_decode_agree`,
    `history_roundtrip_and_truncation`, `history_verify_against_detects_tampering`;
  - `tests/manifest_read_verify.rs::additive_growth_remains_verifiable`
    (generation increases, `verify_deep` still passes);
  - `tests/format_roundtrip.rs::append_generation_uses_newest_and_preserves_prefix`
    (the file prefix is byte-identical after appending).
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test history_checkpoint
  sh scripts/container-test.sh --release --test manifest_read_verify additive_growth
  sh scripts/container-test.sh --release --test format_roundtrip append_generation
  sh scripts/run-large-history.sh
  ```

---

## Gate 3 — Positional access

Checkpoint and full-state access return correct symbols and ranges.

- **Implements:** `src/checkpoint.rs` (`restore`, bounded `max_replay_steps`),
  `src/reader.rs` (path selection), `src/fastpath.rs`.
- **Artifact:** `tests/history_checkpoint.rs`, `tests/properties.rs`,
  `tests/correctness.rs`, `tests/manifest_read_verify.rs`.
- **Proves:**
  - `tests/history_checkpoint.rs::checkpoint_restore_agrees_with_state_log_all_intervals`
    (every supported interval, every position, replay bound `K-1` enforced);
  - `tests/properties.rs::checkpoint_restore_matches_state_log`;
  - `tests/correctness.rs::category_checkpoint_restart_all_intervals_after_reopen`,
    `category_full_state_random_lookup`;
  - `tests/manifest_read_verify.rs::random_ranges_match_the_source_slice`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test history_checkpoint checkpoint_restore
  sh scripts/container-test.sh --release --test correctness
  sh scripts/container-test.sh --release --test manifest_read_verify random_ranges
  ```

---

## Gate 4 — Real four-step bloom

A working `D = 4` structure returns four exact symbols and a correct successor.

- **Implements:** `src/bloom_path.rs` (`PathBloom`, exact `P_D[j]`),
  `src/bloom_context.rs` (exact `T_D[x,c]`), `src/bloom.rs` (packed entries).
- **Artifact:** `tests/bloom_path.rs`, `tests/properties.rs`,
  `tests/blooms_research.rs`.
- **Proves:**
  - `tests/bloom_path.rs::symbols_and_blocks_match_authoritative_every_depth`
    (confirms four symbols + successor state against the machine for `D=4`);
  - `tests/properties.rs::path_symbol_at_matches_decode`;
  - `tests/blooms_research.rs::contextual_exact_all_depths` (exact, zero conflicts, `D=4`);
  - `tests/bloom_path.rs::shell_and_bloom_byte_roundtrips`,
    `from_bytes_rejects_truncation_every_cut`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test bloom_path
  sh scripts/container-test.sh --release --test blooms_research contextual
  sh scripts/container-test.sh --release --test properties path_symbol_at
  ```

---

## Gate 5 — Original 65,536-entry challenge

The global `T_4` target is actually investigated, with a certified construction
or a precise counterexample and a justified alternative.

- **Implements:** `src/bloom_global.rs` (`T_D[x]`, 65,536 slots, certify/emit
  counterexample), `src/model_search.rs` (finite-model sufficiency sweep),
  `src/bloom_key.rs` (sufficiency primitive).
- **Outcome:** `CERTIFIED` on constant/structured cores; `REJECTED` on
  pseudorandom cores with a concrete, reproducible counterexample. The justified
  alternative is the **contextual** `T_D[x, c]` with exactly `D` context bytes
  (exact by construction) and the **path** `P_D[j]` (exact by construction).
- **Proves:**
  - `tests/blooms_research.rs::global_constant_certified_and_matches_machine`;
  - `tests/blooms_research.rs::global_random_rejected_with_valid_counterexample`;
  - `tests/blooms_research.rs::writes_deterministic_global_counterexample`;
  - `tests/model_search.rs::sweep_shows_minimal_sufficient_context_is_depth_bytes`,
    `canonical_state_only_counterexample_is_witness`;
  - `tests/blooms_research.rs::global_physical_bytes_follow_depth`
    (`65536 * (D + 4)` bytes).
- **Evidence:**
  - [`../evidence/counterexamples/bloom-key-0001.json`](../evidence/counterexamples/bloom-key-0001.json)
    — `D=4`, key `175`, two positions with equal state and different outcomes;
  - [`../evidence/model-search/model-search-sweep.json`](../evidence/model-search/model-search-sweep.json)
    — `D=4` conflicts `64 / 256 / 1024 / 4096 / 0` for `k = 0..4`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test blooms_research
  sh scripts/container-test.sh --release --test model_search
  ```

---

## Gate 6 — Expanded manifestations

Regional, contextual and path blooms implemented and evaluated; region-scoped
adaptation added.

- **Implements:** `src/bloom_region.rs` (`T_D^(reg)[x]`, machine-general), `src/bloom_context.rs`,
  `src/bloom_path.rs`; profiles `Regional`, `Path`, `Bloom` in `src/manifest.rs`;
  region-scoped adaptation in `src/region_adapt.rs`.
- **Proves:**
  - `tests/blooms_research.rs::contextual_exact_all_depths`,
    `regional_constant_all_certified_with_exact_lookups`,
    `regional_random_reports_counts_consistently`;
  - `tests/bloom_path.rs`;
  - `tests/async_build.rs::regional_parallel_equals_sequential_byte_for_byte`;
  - `tests/region_adapt.rs` (7 tests: additivity, budget enforcement,
    traced/cold-region byte-exactness, LRU byte-exactness/eviction, determinism);
  - the regional table is built **machine-generally** and stores the **complete
    successor cursor** for a cursor-divergent machine, and `verify`
    (`src/verify.rs::check_regional`) **re-derives that cursor** from the
    authoritative machine pass so a self-consistent forged cursor is rejected
    (`src/verify.rs::forged_regional_cursor_is_detected`;
    `tests/adversarial_r4.rs`);
  - the frontier measures `bloom(D=4)`, `regional(D=4,R=4096)`, `path(D=4)`,
    `path(D=8)` (see Gate 9).
- **Evidence:** [`../evidence/bench/sweep.json`](../evidence/bench/sweep.json),
  [`../evidence/bench/competitive.json`](../evidence/bench/competitive.json).
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test blooms_research
  sh scripts/container-test.sh --release --test bloom_path
  sh scripts/container-test.sh --release --test region_adapt
  sh scripts/container-test.sh --release --test async_build
  sh scripts/run-sweep.sh
  sh scripts/run-competitive.sh
  ```

---

## Gate 7 — Portable format

`.ekb` files can be written, closed, reopened, verified and read correctly.

- **Implements:** `src/format.rs` (fixed records, codecs, checksums),
  `src/writer.rs` (publication), `src/reader.rs` (validated open, generation
  selection, `read`), `src/writer.rs`/`src/reader.rs` authenticated variants.
- **Artifact:** `tests/format_roundtrip.rs`, `tests/cli.rs`, `tests/security.rs`,
  `tests/authenticated.rs`, `tests/file_body.rs`, `tests/correctness.rs`.
- **Proves:**
  - `tests/format_roundtrip.rs::write_open_read_roundtrip_for_every_profile`
    (10 inputs × 8 profiles, whole-object and ranged reads);
  - `tests/format_roundtrip.rs::exact_layout_is_fixed_size_header_and_footer`;
  - `tests/format_roundtrip.rs::malformed_files_are_rejected_specifically_and_never_panic`;
  - `tests/cli.rs::end_to_end_command_surface`;
  - `tests/correctness.rs::category_history_persistence_reopen`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test format_roundtrip
  sh scripts/container-test.sh --release --test cli
  ```

---

## Gate 8 — Crash integrity

Interrupted publication cannot invalidate earlier committed generations under
the documented durability model.

- **Implements:** `src/recovery.rs` (`recover`, `scan_generations`,
  `truncate_to_valid`), `src/writer.rs` (footer-after-payload order),
  `src/async_io.rs` (`io_uring` ordering, when enabled).
- **Artifact:** `tests/crash_recovery.rs`, `tests/correctness.rs`,
  `tests/segmented_hardening.rs`, `tests/adversarial.rs`, `tests/async_io.rs`.
- **Proves:**
  - `tests/crash_recovery.rs::truncate_mid_footer_keeps_previous_generation`,
    `corrupt_newest_section_payload_keeps_previous_generation`,
    `trailing_garbage_keeps_newest_generation_and_truncates`,
    `fully_corrupt_commit_is_rejected_not_ignored`;
  - `tests/correctness.rs::category_interrupted_publication_torn_file`,
    `category_corrupted_file_rejected`;
  - `tests/async_io.rs::failed_append_never_corrupts_the_previous_generation`,
    `failed_publish_leaves_the_existing_committed_file_intact`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test crash_recovery
  sh scripts/container-test.sh --release --test correctness
  sh scripts/container-async-io.sh
  ```

---

## Gate 9 — Speed measurement

A real speed-versus-manifested-space frontier exists.

- **Implements:** `src/sweep.rs` (`manifestation_sweep`, `write_evidence_json`),
  `src/fastpath.rs` (optimised paths), `src/metrics.rs` (timing/percentiles);
  drivers `examples/sweep.rs`, `examples/baselines.rs`, `examples/competitive.rs`,
  `examples/perf_target.rs`; harness `benches/ekbloom.rs`.
- **Proves:** `tests/fastpath.rs::fast_paths_are_byte_exact_for_every_profile`
  (optimised paths are byte-identical to `read`), and the generated frontier.
- **Artifact:** `tests/fastpath.rs`, `tests/sweep_curves.rs`, `tests/layout.rs`,
  `tests/simd.rs`, `tests/baselines.rs`.
- **Evidence:** [`../evidence/bench/sweep.json`](../evidence/bench/sweep.json),
  [`../evidence/bench/cpu.json`](../evidence/bench/cpu.json),
  [`../evidence/bench/sweep-large.json`](../evidence/bench/sweep-large.json),
  [`../evidence/bench/depth-metrics.json`](../evidence/bench/depth-metrics.json),
  [`../evidence/bench/memory-tiers.json`](../evidence/bench/memory-tiers.json),
  [`../evidence/bench/adaptive.json`](../evidence/bench/adaptive.json),
  [`../evidence/bench/perf.json`](../evidence/bench/perf.json),
  [`../evidence/bench/baselines.json`](../evidence/bench/baselines.json),
  [`../evidence/bench/competitive.json`](../evidence/bench/competitive.json).
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test fastpath
  sh scripts/container-test.sh --release --test baselines
  sh scripts/run-sweep.sh
  sh scripts/run-baselines.sh
  sh scripts/run-competitive.sh
  sh scripts/container-bench.sh
  sh scripts/container-perf.sh
  ```

---

## Gate 10 — Independent reproduction

A clean pinned container reproduces the correctness tests and representative
benchmarks.

- **Implements:** `scripts/lib.sh` (pinned base digest), `Dockerfile`,
  `scripts/container-reproduce.sh`, `scripts/run-sweep.sh`,
  `scripts/capture-environment.sh`.
- **Proves:** `scripts/container-reproduce.sh` wipes build artifacts, rebuilds
  `--locked` from `Cargo.lock`, and runs the full release test suite from a
  pristine tree. `scripts/run-sweep.sh` regenerates the benchmark evidence.
- **Evidence:** [`../evidence/environment.txt`](../evidence/environment.txt)
  (project image id, base digest, Dockerfile sha256, toolchain, kernel, CPU,
  `Cargo.lock` sha256, git HEAD, nightly digest).
- **Reproduce:**

  ```sh
  sh scripts/container-image.sh        # build the pinned image (if absent)
  sh scripts/container-reproduce.sh    # clean build + tests from scratch
  sh scripts/run-sweep.sh              # representative benchmarks
  sh scripts/capture-environment.sh    # refresh evidence/environment.txt
  sh scripts/container-verify.sh       # fmt + clippy -D warnings + tests
  sh scripts/container-loom.sh         # Loom model checking
  ```

---

## Additional evidence (post-0.3 additions)

These artifacts back the properties beyond the original 10 gates. None is
fabricated; each is regenerated by the command shown and by nothing else.

| Artifact | What it shows | Regenerate with |
|---|---|---|
| [`../evidence/bench/history-2p30.json`](../evidence/bench/history-2p30.json) | Segmented `HistoryStore` streams the full `4*(N+1)` projection at `N = 2^30` (4,294,967,300 bytes in 3.666 s, peak RSS 2.9 MiB, sampled checks `PASS`) | `sh scripts/run-large-history.sh` |
| [`../evidence/bench/perf.json`](../evidence/bench/perf.json) | Real hardware counters (cycles, IPC, cache/branch misses, bandwidth) for the `path(D=4)` workloads under `CAP_PERFMON` | `sh scripts/container-perf.sh` |
| [`../evidence/bench/depth-metrics.json`](../evidence/bench/depth-metrics.json) | Per-depth path-bloom metrics `D ∈ {1,2,4,8,16,32}` (§10): identical semantics, falling physical bytes, flat 30 ns p50 | `sh scripts/run-sweep.sh` |
| [`../evidence/bench/memory-tiers.json`](../evidence/bench/memory-tiers.json) | O9 L1/L2/LLC/DRAM tier curve: throughput 139 → 71 → 57 → 18 GB/s at constant `E_V = 2.13×` | `sh scripts/run-sweep.sh` |
| [`../evidence/bench/adaptive.json`](../evidence/bench/adaptive.json) | O7 adaptive shell: appending an exact `P_32[j]` as generation 2 takes 52,727 → 6.96 ns/read (≈7,570×) for +1,179,883 bytes | `sh scripts/run-sweep.sh` |
| [`../evidence/bench/sweep-large.json`](../evidence/bench/sweep-large.json) | 4 MiB budget frontier + region/depth/checkpoint curves, cold starts, cache-resident throughput | `sh scripts/run-sweep.sh` |
| [`../evidence/bench/baselines.json`](../evidence/bench/baselines.json) | Conventional baselines vs `Root`/`Path{D=4}`/`Full`: no read-speed win over `raw-vec`; machine 2 `V/N = 0.511×` on skewed data | `sh scripts/run-baselines.sh` |
| [`../evidence/bench/competitive.json`](../evidence/bench/competitive.json) | Region-scoped manifestation vs an LRU cache vs raw under one corpus/trace/budget: LRU wins hot (60 vs 871 ns), EKBLOOM wins cold (240 vs 281 ns) | `sh scripts/run-competitive.sh` |
| [`../evidence/bench/io-uring.json`](../evidence/bench/io-uring.json) | Genuine completion-driven `io_uring` publication, byte-identical and durability-ordered, but ≈2.2–2.3× slower than sync for ~1 MiB | `sh scripts/container-async-io.sh` |
| [`../evidence/bench/prefix.json`](../evidence/bench/prefix.json) | Parallel-prefix reconstruction: 4.36 vs 0.50 GB/s decode (8.7×, 8 threads), 6,958× positional state vs replay, and the short-input two-pass loss | `sh scripts/run-prefix.sh` |
| [`../evidence/bench/packed.json`](../evidence/bench/packed.json) | Packed 16-bit restarts/history: exactly 1/6 and 1/2 the bytes, equal-or-better latency, 6× denser restarts, `state_from_hot` 0 mismatches | `sh scripts/run-packed.sh` |
| [`../evidence/bench/tiles.json`](../evidence/bench/tiles.json) | Selective hot tiles match whole-object `P_32` hot latency (80–110 ns) for 65,792–69,632 B vs 4,718,827 B (≈68–72× fewer added bytes) | `sh scripts/run-tiles.sh` |
| [`../evidence/bench/handles.json`](../evidence/bench/handles.json) | Cached verified handle ~3,400×/~2,100× over reopen+reparse (120/201 ns vs ~413/424 µs), lock-free under publication, persisted tiles one `pread` | `sh scripts/run-handles.sh` |
| [`../evidence/bench/io-batch.json`](../evidence/bench/io-batch.json) | `io_uring` batched cold reads: 7.02× `pread` at 4 KiB / depth 64, parity (0.97–1.01×) at depth 1; single/hot reads never submit to the ring | `sh scripts/run-io.sh` |
| [`../evidence/bench/court.json`](../evidence/bench/court.json) | The rigorous court (152 rows): interleaved reps, pooled distributions with bootstrap 95% CIs, per-working-set 3-D Pareto frontier | `sh scripts/run-court.sh` |
| [`../evidence/bench/planner.json`](../evidence/bench/planner.json) | Certified planner: 16 hot regions → `HotBytes`, 48 cold → `PackedRestart`, byte-exact to `reader::read`; hot 19 ns / cold 129 ns | `sh scripts/run-court.sh` |
| [`../evidence/bench/kernels.json`](../evidence/bench/kernels.json) | SIMD reconstruction kernels (§6): Kernel A vector batch 5.9–8.0× the equal-layout scalar control (unrolled KEEP, 2-symbol table REJECT); Kernel B vector KEEP ≤ 1024 B, REJECT ≥ 4096 B; all byte-exact vs `NL2::decode` | `sh scripts/run-kernels.sh` |
| [`../evidence/bench/planner-bg.json`](../evidence/bench/planner-bg.json) | Workload-directed background planner: moving hot window adapted from sharded counters (cold p50 301 → warm 130 ns, hot 0% → 100%), additive, 44 shared locks / 12,288 reads | `sh scripts/run-planner-bg.sh` |
| [`../evidence/bench/mmap.json`](../evidence/bench/mmap.json) | Read-only mapped surface (`src/mapped.rs`): mmap KEEP (rand-1b 130 → 30 ns, 4.3×), resident/`pread` comparison, huge pages REJECTED (already file-THP backed) | `sh scripts/run-mmap.sh` |
| [`../evidence/bench/rans.json`](../evidence/bench/rans.json) | Machine-2 prepared decode context: one validated `RansContext` reused for a bulk decode is **165×** the per-symbol rebuild on skewed data (byte-exact) | `sh scripts/run-rans.sh` |
| [`../evidence/bench/blocks.json`](../evidence/bench/blocks.json) | Machine-2 restartable blocks (256 B–64 KiB): **38–330×** vs replay-from-genesis (6,095,739 ns) at a **141×** footprint span; packed checkpoint format **1/6** with backward-compatible legacy reads | `sh scripts/run-blocks.sh` |
| [`../evidence/bench/frontier.json`](../evidence/bench/frontier.json) | Unified engine vs indexed LZ4/Zstd (**no cache / LRU / pinned-hot**) and raw: hot 4 KiB **1.00–1.14×** raw, footprint **0.27×/0.79×** raw on skewed/mixed (uniform 1.07×); **wins footprint at non-evicting hot access (skewed 1.14 MB < pinned Zstd 1.38 MB / LZ4 2.94 MB) — not latency**: raw/pinned-hot Zstd are faster hot and cold p99 loses to bare LZ4. Also carries the **`packed`** (machine 5) and **`segmented`** (machine 6) frontier sections. | `sh scripts/run-frontier.sh` |
| [`../evidence/bench/fuse-perf.json`](../evidence/bench/fuse-perf.json) | Measured FUSE read-path cost vs a direct read, and the FUSE passthrough / FUSE-over-io_uring **non-result** (incl. the genuine `ReadMode::Machine` decode baseline, **382×** the dispatch). | `sh scripts/run-fuse-perf.sh` |
| [`../evidence/bench/batch.json`](../evidence/bench/batch.json) | Batched independent reconstruction (L4): CPU-thread scaling **8.57× at 16 threads** (every row byte-exact), plus the honest GPU-unavailable verdict | `sh scripts/run-batch.sh` |
| [`../evidence/bench/window.json`](../evidence/bench/window.json) | Bounded encoded-window reads (N2): a 4 KiB read of a 64 MiB machine-2 object fetches **10,270,243 → 16,440 B** (**625×**) at the same 2 `pread` syscalls, p99 **972 → 26.4 µs**; a 1-byte read fetches 8,216 B (1,250×) | `sh scripts/run-window.sh` |
| [`../evidence/bench/orfs.json`](../evidence/bench/orfs.json) | OrFS single authoritative store (N5): real `du`-style on-disk bytes vs one `.ekb`/raw/pinned-hot LZ4/Zstd; exceeds the in-memory engine by **+1.54 % / +0.58 %** (O-round compaction, from +1.89 %/+0.70 %; bounded framing, no duplicated payload) | `sh scripts/run-orfs.sh` |
| [`../evidence/bench/optimizer.json`](../evidence/bench/optimizer.json) | Physical-budget-aware joint planner (N6): held-out p99 **10.07 µs** vs greedy **13.58 µs** (budget `N`) and **9.84** vs **13.19 µs** (`1.5·N`); `joint_improves_frontier = true`. `uniform_evidence` (O2): chosen machine 3, `committed_bytes == raw_bytes == 4,194,304`, p99 not worse than no-structure, sequential **69.558 GB/s** | `sh scripts/run-optimizer.sh` |
| [`../evidence/bench/e2e.json`](../evidence/bench/e2e.json) | End-to-end real-data proof (N7): `beats_raw_store = true` and `holds_vs_pinned = false` on `text`/`elf`/`mixed`; ORFS 0.74×/0.98×/0.78× raw and hot 4 KiB p50 120–130 ns vs raw 160–170 ns | `sh scripts/run-e2e.sh` |
| [`../evidence/bench/decoders.json`](../evidence/bench/decoders.json) | The **decoder court** (O1): machines 7 (word16, ≈6 % over m2 but ≈37 % under `K = 8`), 8 (independent lanes, +30 %) and the AVX2 rANS kernel (+25–27 %) all **rejected**; best stays machine 4 `K = 8` (cold 4 KiB p50 **8,997 ns**, **3.2×** smaller than LZ4, ≈4.3× slower cold); every variant byte-exact | `sh scripts/run-frontier.sh` |
| [`../evidence/bench/stream.json`](../evidence/bench/stream.json) | Streaming container (P2): a **2 GiB** stream builds at **8.6 MiB** peak RSS (build 4.5 MiB, 428 MiB/s), byte-exact `read_at` p50 **11,011** / p99 **20,818 ns**, sequential **1,076 MiB/s** | `sh scripts/run-stream.sh` |
| [`../evidence/bench/corruption.json`](../evidence/bench/corruption.json) | Corruption court (P3): **1024** single-bit flips per structure (584,152 B body, 18 sections); fast path **1024/1024** in every covered structure (inert bytes 0/1024), deep verifier 1024/1024 with located offsets, **≤4096 B** window, `Corrupt { section, offset }` | `sh scripts/run-corruption.sh` |
| [`../evidence/bench/dictionary.json`](../evidence/bench/dictionary.json) | Machine 9 `OrANS-DICT` (P5): **83.5 B/rec** vs per-record machine 2 **590.0** (raw 100.0), decode 721 vs 1,403 ns, break-even **2 records**; unregistered/tampered/relabelled id a typed `Err` | `sh scripts/run-dictionary.sh` |
| [`../evidence/bench/corpora.json`](../evidence/bench/corpora.json) | Standard-corpora court (P6): Silesia/enwik8/Calgary/text/ELF at an equal budget — **`E_V` 0.988–0.998** (≤ raw), `byte_at` ~1.1–3.3 µs vs zstd 8.6–16.1 µs, bounded encoded windows vs LZ4/zstd | `sh scripts/run-corpora.sh` |
| [`../evidence/bench/counters.json`](../evidence/bench/counters.json) | Hardware counters (P6): **39.14 instructions/byte** / 8.1 cycles for a 1-byte read; **0.03** instr/byte, 122 instr/read for 4 KiB | `sh scripts/run-counters.sh` |
| [`../evidence/bench/concurrency.json`](../evidence/bench/concurrency.json) | Concurrency p99 (P6): 64 readers vs a publishing writer — **1.4e-3 locks/read**; the busy arm is **not worse** than quiet at p50/p99/p99.9 | `sh scripts/run-corpora.sh` |
| [`../evidence/bench/bound.json`](../evidence/bench/bound.json) | Worst-case step bound (P6): **1,200,000** samples (1,000,000 on a 1 MiB block restart), measured ≤ claimed, **0** violations, every range byte-exact | `sh scripts/run-bound.sh` |
| [`../evidence/bench/aarch64-determinism.json`](../evidence/bench/aarch64-determinism.json) | Cross-architecture determinism (P8): x86_64 vs arm64 (QEMU) produce **identical** `physical_bytes()` and content hashes (`identical: true`, equal sha256) | `sh scripts/container-aarch64.sh` |
| [`../evidence/gates/G1.json`](../evidence/gates/G1.json) … [`G5.json`](../evidence/gates/G5.json) | The machine-checked acceptance gates **G1–G5** (P8), all PASS: invertibility (machines 1–9), prefix composition, worst-case bound (1,000,000 positions, 0 violations), corruption detection (≤4096 B), physical accounting | `sh scripts/run-gates.sh` |
| [`../evidence/environment.txt`](../evidence/environment.txt) | Pinned image/digest, toolchain, kernel, CPU, `Cargo.lock` sha256, git HEAD | `sh scripts/capture-environment.sh` |

Loom model checking is reproducible with `sh scripts/container-loom.sh`
([`reviews/concurrency.md`](reviews/concurrency.md)); the FUSE integration test
with `sh scripts/container-fuse.sh` ([`FUSE.md`](FUSE.md)).

Every committed evidence JSON is validated by `sh scripts/validate-evidence.sh`
(`examples/validate_evidence.rs`, dependency-free, wired into
`scripts/container-reproduce.sh`); it currently reports **46** files `OK`, and a
malformed file fails the reproduction non-zero rather than publishing silently.
The R4 verifier fix (`src/verify.rs::check_regional` re-derives the regional
successor cursor and uses a checked `start + len`) is exercised by
`tests/adversarial_r4.rs` and `src/verify.rs::forged_regional_cursor_is_detected`.

### Limitations closure — rounds L1–L4 and M1–M5

The final rounds fold the long-standing limitations into the certified body; each
is measured by a committed artifact and independently reviewed (Revision 5 for
L1–L4, **Revision 6** for M1–M5, in
[`reviews/adversarial.md`](reviews/adversarial.md),
[`reviews/security.md`](reviews/security.md), verdict **NO DEFECT FOUND**).

| Round | Limitation folded in | Measured result | Artifact / doc |
|---|---|---|---|
| **L1** | allocation-free per-call reads; a base representation that never expands | `byte_at`/`read_into` allocation-free (1-byte allocation-parity **1.00–1.05×** raw); machine 3 `OrANS-Store` + `auto` policy ⇒ uniform base **1.0000×** raw | `frontier.json`, [`CODEC.md`](CODEC.md), [`FRONTIER.md`](FRONTIER.md) |
| **L1b** | 1-byte fully-resident read within 15% of raw `Vec` | borrowed `ResidentView` **0.95–1.03×** raw (inside 15%); per-call `byte_at` dispatch residual (4.2–5.7×) documented | `frontier.json`, [`FRONTIER.md`](FRONTIER.md) §2 |
| **L2/L2b** | cold decode gap to block-indexed LZ4 | machine 4 interleaved rANS **1.49–1.60×** faster than machine 2; best cold **10.0 µs** vs LZ4 **2.0 µs** at **3.1× less storage** — narrowed, **not closed** (≈4.7–4.9×) | `frontier.json`, [`CODEC.md`](CODEC.md) |
| **L3** | FUSE read cost; authenticity when required | `ReadMode::Auto` direct backing surface (marginal ~1.2× on uncached 4 KiB); passthrough still unsupported by `fuser`; keyed MAC covers **every** section + strict `--require-auth` fail-closed | `fuse-perf.json`, [`FUSE.md`](FUSE.md), [`SECURITY.md`](SECURITY.md) |
| **L4** | batched independent reconstruction; accelerator boundary | **8.57× at 16 threads**, every row byte-exact; GPU **not implemented** (`available() == false`) | `batch.json`, [`GPU.md`](GPU.md) |
| **M1** | complete successor state for lane-bearing machines | machine-4 restarts store the **wide** snapshot (state + cursor + per-lane states + active-lane index); `verify` re-checks the lanes; `(state,cursor)` projection is lossy; machines 1/2/3 unchanged | `tests/machine4_restarts.rs`, [`PROOFS.md`](PROOFS.md) P29 |
| **M2** | exact accounting + committed-vs-active budget | every structure's `physical_bytes() == to_bytes().len()` (shared model header counted once), audited machines 1–4; `Engine::plan` admits on `body.physical_bytes()` (committed), not the active plan; `n == 0` checked | `tests/accounting.rs`, [`ENGINE.md`](ENGINE.md) §3, P30 |
| **M3** | request-level bounded work | `worst_case_steps_for(req)` sums the dispatched segments; multi-region actual **114,687** ≤ claimed **131,070** (per-region max **65,535**), checked mechanically | [`ENGINE.md`](ENGINE.md) §4, P32 |
| **M4** | pinned-hot competitor + corrected headline | indexed LZ4/Zstd **pinned-hot** rows (decompressed blocks retained permanently, counted in footprint); **footprint at non-eviction — not latency** | `frontier.json`, [`FRONTIER.md`](FRONTIER.md), `tests/pinned_hot.rs` |
| **M5** | machine-4 cold engine + `create_auto` reuse | machine-4 engine cuts skewed cold p99 **17.3 → 12.0 µs** (**1.44×**) at **+12%** bytes; `create_auto` **18.1 vs 36.1 ms** (**1.99×**, byte-identical); machine-4 best **K = 8** (**1.60×**) | `frontier.json`, [`CODEC.md`](CODEC.md), [`ENGINE.md`](ENGINE.md), P33 |

These closure rounds are covered by the R5 adversarial/security passes
(`tests/adversarial_r5.rs`, 13 tests; `tests/auth_coverage.rs`), the **M-round**
Revision 6 pass (`tests/adversarial_m.rs`, 13 tests + `fuzz_region`), and the
**N-round** Revision 7 pass (`tests/adversarial_r7.rs`, 11 tests +
`fuzz_pack`/`fuzz_seg`/`fuzz_resource`); the new properties are P24–P33 and
P34–P40 in [`PROOFS.md`](PROOFS.md).

### Limitations closure — rounds N1–N8

The N-round closes the concealed-access surface; each item is measured by a
committed artifact and independently reviewed in **Revision 7**
([`reviews/adversarial.md`](reviews/adversarial.md),
[`reviews/security.md`](reviews/security.md), verdict **TWO DEFECTS, BOTH FIXED**:
`BlockIndex` output-doubling for machines 5/6 and an `index + 1` overflow panic,
both fixed).

| Round | What it folds in | Measured result | Artifact / doc |
|---|---|---|---|
| **N1** | read plans bound to core identity | `Engine::plan` records the planned body's `CoreIdentity`; a different body or a newer generation of the same body is rejected with `EngineBodyMismatch` **before any decode**; a reopened `ReadyObject` is object-bound | `tests/adversarial_r7.rs`, [`ENGINE.md`](ENGINE.md), P34 |
| **N2** | bounded file-backed encoded windows | a 4 KiB read of a 64 MiB machine-2 object fetches **10,270,243 → 16,440 B** (**625×**) at the **same 2 `pread` syscalls**, p99 **972 → 26.4 µs**; a 1-byte read fetches 8,216 B (1,250×) | `window.json`, [`WINDOW.md`](WINDOW.md), P35 |
| **N3** | machine 5 `OrANS-PACK` (fixed-width direct addressing) | 1-byte p50 **30 ns** at raw-index parity; 4 KiB **3.5 µs vs 13.5 µs**; core **`N/4`** on the 4-symbol source; constant/identity renderings exact; never larger than raw + 10 B | `frontier.json` `packed`, [`PACKED_MACHINE.md`](PACKED_MACHINE.md), P36 |
| **N4** | machine 6 `OrANS-SEG` (segmented container) | `mixed` **2,686,048 B = −10.1 %** vs machine 2 (2,986,989 B), within **2.5 %** of the 2,621,440 B oracle incl. metadata; BLAKE3 directory hash + content ids | `frontier.json` `segmented`, [`SEGMENTS.md`](SEGMENTS.md), P37 |
| **N5** | OrFS one authoritative store + pointer views | authoritative payload stored **exactly once**; real on-disk excess **+1.89 % / +0.70 %** over the in-memory engine; genuine `ReadMode::Machine` decode **382×** the dispatch | `orfs.json`, [`ORFS.md`](ORFS.md), P38 |
| **N6** | physical-budget-aware joint planner | held-out p99 **10.07 µs** vs greedy **13.58 µs** (budget `N`); **9.84** vs **13.19 µs** (`1.5·N`); `all_certified`, cap-rechecked, additive `grow_into` | `optimizer.json`, [`OPTIMIZER.md`](OPTIMIZER.md), P39 |
| **N7** | end-to-end real-data verdict | `beats_raw_store = true` on all three corpora (0.74×/0.98×/0.78× raw; hot 4 KiB 120–130 ns vs raw 160–170 ns; non-evicting); `holds_vs_pinned = false` (behind pinned LZ4/Zstd, and behind raw/LZ4 cold + sequential) | `e2e.json`, [`E2E.md`](E2E.md) |
| **N8** | resource policy + fuzzing | every whole-object allocation driven by a declared/core-derived length bounded at **1 GiB** (`AllocationTooLarge`); **17** libFuzzer targets (three added in N8, two in O3, two in the P-round) | [`SECURITY.md`](SECURITY.md), `src/resource.rs`, P40 |

**Honest reading.** The N-round adds real capabilities and bounded resource use —
identity-bound plans, bounded windows, two directly-addressable machines, a
single-store OrFS, a certified joint planner and a hard allocation ceiling — but it
**does not** change the end-to-end verdict: on real data EKBLOOM/OrFS wins against
raw (space + hot latency, non-evicting) and **loses** to the pinned-hot LZ4/Zstd
codecs and on the cold/sequential axes ([`E2E.md`](E2E.md)).

Supporting documents added alongside these artifacts:
[`CODEC.md`](CODEC.md) (machines 2/3/4 + the `auto` policy),
[`BASELINES.md`](BASELINES.md) (conventional baselines),
[`COMPETITIVE.md`](COMPETITIVE.md) (vs an LRU cache),
[`ASYNC.md`](ASYNC.md) (io_uring),
[`LAYOUT.md`](LAYOUT.md) (layout/SIMD keep-or-reject decisions),
[`ADAPTIVE.md`](ADAPTIVE.md) (O7/O8/O9),
[`PREFIX.md`](PREFIX.md) (parallel-prefix reconstruction),
[`PACKED.md`](PACKED.md) (packed restarts/history),
[`TILES.md`](TILES.md) (selective hot tiles),
[`HANDLES.md`](HANDLES.md) (persistent handles + RCU),
[`IOURING.md`](IOURING.md) (io_uring hybrid read path),
[`COURT.md`](COURT.md) (the performance court),
[`PLANNER.md`](PLANNER.md) (certified planner + background planner),
[`KERNELS.md`](KERNELS.md) (SIMD reconstruction kernels),
[`MMAP.md`](MMAP.md) (mapped surface + huge pages),
[`BLOCKS.md`](BLOCKS.md) (machine-2 blocks + compact checkpoints),
[`ENGINE.md`](ENGINE.md) (unified bounded-access engine),
[`FRONTIER.md`](FRONTIER.md) (engine vs indexed codecs),
[`GPU.md`](GPU.md) (batched reconstruction + GPU boundary),
[`SECURITY.md`](SECURITY.md) (opt-in `AUTH`),
[`INDEX.md`](INDEX.md) (one-page doc/evidence index), and the independent
reviews in [`reviews/`](reviews/) (adversarial, concurrency, mathematics,
performance, reproducibility, security), plus the N-round docs
[`WINDOW.md`](WINDOW.md) (bounded encoded windows),
[`PACKED_MACHINE.md`](PACKED_MACHINE.md) (machine 5),
[`SEGMENTS.md`](SEGMENTS.md) (machine 6),
[`ORFS.md`](ORFS.md) (the OrFS single store),
[`OPTIMIZER.md`](OPTIMIZER.md) (the joint planner) and
[`E2E.md`](E2E.md) (the end-to-end verdict), the O-round doc
[`DECODERS.md`](DECODERS.md) (the decoder court), and the P-round docs
[`MIGRATION.md`](MIGRATION.md) (versioning and compatibility),
[`CORE-LAYOUT.md`](CORE-LAYOUT.md) (the implementable byte layout),
[`STREAMING.md`](STREAMING.md) (streaming containers),
[`CORRUPTION.md`](CORRUPTION.md) (the corruption model),
[`SAFETY.md`](SAFETY.md) (the `unsafe` budget),
[`ALLOC.md`](ALLOC.md) (the allocation/panic audit),
[`DICTIONARY.md`](DICTIONARY.md) (machine 9),
[`CORPORA.md`](CORPORA.md) (the standard-corpora court),
[`MACHINES.md`](MACHINES.md) (the machine catalogue),
[`BUDGET-GUIDE.md`](BUDGET-GUIDE.md) (the budget chooser),
[`LIMITATIONS.md`](LIMITATIONS.md) (the consolidated non-claims),
[`GATES.md`](GATES.md) (the machine-checked gates) and
[`TESTING.md`](TESTING.md) (the trust infrastructure).

### Limitations closure — rounds O1–O3

The O-round completes the decoder **programme track** and folds in two economy
fixes; the surface is reviewed in **Revision 8**
([`reviews/adversarial.md`](reviews/adversarial.md),
[`reviews/security.md`](reviews/security.md), verdict **TWO DEFECTS, BOTH FIXED**:
the optimiser tight-budget base defect (**J**) and the `cursor + N` overflow
(**I**), both fixed), and its properties are **P41–P44** in [`PROOFS.md`](PROOFS.md).

| Round | What it folds in | Measured result | Artifact / doc |
|---|---|---|---|
| **O1** | the **decoder court** — word16 (machine 7), independent lanes (machine 8), an AVX2 rANS kernel | all **REJECTED**: machine 7 ≈**6 %** over machine 2 but ≈**37 %** under `K = 8`; machine 8 **+30 %**; kernel **+25–27 %**. Best stays **machine 4 `K = 8`** (cold 4 KiB p50 **8,997 ns**, 45-byte restart), **3.2× smaller** than LZ4 but ≈4.3× slower cold; every variant byte-exact, machines 1–6 wire-hash pinned | `decoders.json`, [`DECODERS.md`](DECODERS.md), P41, P42 |
| **O2** | uniform-footprint gate + OrFS compaction | uniform plan `committed_bytes == raw_bytes == 4,194,304`, no p99 regression (130.9 == 130.9 ns), sequential raw-like (**69.6 GB/s**); OrFS on-disk excess **+1.89 %/+0.70 % → +1.54 %/+0.58 %** (payload stored once) | `optimizer.json`, `orfs.json`, [`OPTIMIZER.md`](OPTIMIZER.md), [`ORFS.md`](ORFS.md), P43, P44 |
| **O3** | review fixes + cursor hardening | Finding **I** (cursor overflow, machines 7/8 + SIMD kernel) and Finding **J** (gate aborted when only the base fit) fixed; **14** unchecked `cursor + N` sites hardened to `saturating_add` across machines 1–4; **17** fuzz targets, **766/0** release, **46** JSON, verify green at **780** | `tests/adversarial_o.rs`, [`reviews/security.md`](reviews/security.md) Revision 8 |

**Honest reading.** The decoder court is **complete with all variants rejected**
(a word16 renorm, independent per-lane streams and a SIMD gather kernel all lose to
the incumbent interleaved machine 4 `K = 8`), so the O-round adds **no** new speed
ceiling: it closes the decoder track, keeps uniform data at `≤ raw`, and removes a
redundant manifested structure ([`DECODERS.md`](DECODERS.md)).

### Limitations closure — rounds P1–P8

The P-round makes the stored format safe to evolve, adds a streaming container, audits
corruption/`unsafe`/allocation, adds a shared-dictionary machine, and closes the loop
with a standard-corpora court, a machine catalogue + budget chooser, and fuzz +
cross-architecture + gate trust infrastructure. It is reviewed in **Revision 9**
([`reviews/adversarial.md`](reviews/adversarial.md),
[`reviews/security.md`](reviews/security.md); verdict **NO LIBRARY DEFECT FOUND — one
fuzz-coverage gap fixed, two doc defects reported**; 7 tests in `tests/adversarial_p.rs`),
and its properties are **P45–P50** in [`PROOFS.md`](PROOFS.md).

| Round | What it folds in | Measured result | Artifact / doc |
|---|---|---|---|
| **P1** | explicit, versioned wire format | `core_version` (`u16` at header offset 68; `0` legacy / `1` current) + `format_major.minor` + `machine_id`; unknown versions are typed refusals, never guessed; legacy reads exactly, machine cores byte-identical | [`MIGRATION.md`](MIGRATION.md), [`CORE-LAYOUT.md`](CORE-LAYOUT.md), P45 |
| **P2** | streaming, seekable container with bounded builder memory | **2 GiB** stream builds at **8.6 MiB** peak RSS (one block + `O(blocks)` directory); `read_at` p50 **11,011** / p99 **20,818 ns**; sequential **1,076 MiB/s**; every read byte-exact | `stream.json`, [`STREAMING.md`](STREAMING.md), P46 |
| **P3** | corruption detection and granularity | **1024** single-bit flips per structure, **1024/1024** detected (fast path) and **1024/1024** (deep verifier); **≤4096 B** window; typed `Corrupt { section, offset }`; inert bytes 0/1024 | `corruption.json`, [`CORRUPTION.md`](CORRUPTION.md), P47 |
| **P4** | the `unsafe` + allocation audit, machine-enforced | exactly **three** sanctioned modules; `check-safety.sh` fails closed; **0** production `.unwrap()`/`panic!` (4 audited `.expect(`); Miri covers **61** unit tests | [`SAFETY.md`](SAFETY.md), [`ALLOC.md`](ALLOC.md) |
| **P5** | machine 9 (`OrANS-DICT`) shared trained dictionary | **83.5 B/rec** vs machine 2 **590.0** (raw 100.0); decode 721 vs 1,403 ns; break-even **2 records**; unregistered/tampered id typed `Err` | `dictionary.json`, [`DICTIONARY.md`](DICTIONARY.md), P49 |
| **P6** | standard corpora + counters + concurrency + worst-case bound | **`E_V` 0.988–0.998** (≤ raw) on Silesia/enwik8/Calgary/text/ELF; **39.14 instr/byte** (8.1 cycles) for 1-byte reads; **1.4e-3 locks/read** for 64 readers; **1,200,000** bound samples, **0** violations | `corpora.json`/`counters.json`/`concurrency.json`/`bound.json`, [`CORPORA.md`](CORPORA.md), P50 |
| **P7** | machine catalogue + `choose_plan` budget chooser + one limitations list | machines 1–9 specified exactly; `choose_plan` selects machine × segment × restart × path × tiles under a byte budget; every honest non-claim in one place | [`MACHINES.md`](MACHINES.md), [`BUDGET-GUIDE.md`](BUDGET-GUIDE.md), [`LIMITATIONS.md`](LIMITATIONS.md) |
| **P8** | fuzz + cross-arch determinism + machine-checked gates | **17** libFuzzer targets PASS; x86_64 == arm64 (`identical: true`); gates **G1–G5 all PASS**; machine-6 trait `decode` fixed from `O(n²)` to **linear** (**415 s → 0.5 ms** at `n = 2²⁰`) | [`TESTING.md`](TESTING.md), [`GATES.md`](GATES.md), `aarch64-determinism.json`, P48 |

**Honest reading.** The P-round adds **no new speed ceiling**. It makes the format
safe to evolve (typed version refusal, byte-identical cores), bounds streaming memory,
proves 100 % + localized corruption detection, confines and audits `unsafe` and
allocation, factors a shared model out of small records (machine 9, against per-record
machine 2 — not against zstd), and adds the standard-corpora/counters/concurrency/bound
evidence plus fuzz, cross-architecture determinism and the G1–G5 gates. The end-to-end
verdict of the N/O rounds is unchanged: on real data EKBLOOM wins against raw (space +
hot latency, non-evicting) and **loses** to the pinned-hot LZ4/Zstd codecs and on the
cold/sequential axes ([`CORPORA.md`](CORPORA.md), [`E2E.md`](E2E.md)).
