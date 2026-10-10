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

- **Implements:** `src/machine.rs` (`Nl2`, `OrMachine`, `MACHINE_NL2`).
- **Proves:**
  - unit tests in `src/machine.rs`: `sbox_is_bijective`, `roundtrip_various`,
    `genesis_matches_log0_zero`, `state_is_last_two_symbols`, `peek_matches_reveal`;
  - `tests/properties.rs::machine_roundtrip` (arbitrary `Vec<u8>`);
  - `tests/correctness.rs` categories (`category_every_byte_value`,
    `category_empty_input`, `category_single_symbol`, `category_lcg_pseudorandom`).
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release machine::
  sh scripts/container-test.sh --release --test properties machine_roundtrip
  sh scripts/container-test.sh --release --test correctness
  ```

---

## Gate 2 — Non-withdrawal history

Committed history retained without overwriting earlier states.

- **Implements:** `src/history.rs` (append-only `states`), `src/checkpoint.rs`,
  additive `manifest`/`grow` in `src/manifest.rs`.
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
  ```

---

## Gate 3 — Positional access

Checkpoint and full-state access return correct symbols and ranges.

- **Implements:** `src/checkpoint.rs` (`restore`, bounded `max_replay_steps`),
  `src/reader.rs` (path selection), `src/fastpath.rs`.
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

Regional, contextual and path blooms implemented and evaluated.

- **Implements:** `src/bloom_region.rs` (`T_D^(reg)[x]`), `src/bloom_context.rs`,
  `src/bloom_path.rs`; profiles `Regional`, `Path`, `Bloom` in `src/manifest.rs`.
- **Proves:**
  - `tests/blooms_research.rs::contextual_exact_all_depths`,
    `regional_constant_all_certified_with_exact_lookups`,
    `regional_random_reports_counts_consistently`;
  - `tests/bloom_path.rs`;
  - `tests/async_build.rs::regional_parallel_equals_sequential_byte_for_byte`;
  - the frontier measures `bloom(D=4)`, `regional(D=4,R=4096)`, `path(D=4)`,
    `path(D=8)` (see Gate 9).
- **Evidence:** [`../evidence/bench/sweep.json`](../evidence/bench/sweep.json).
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test blooms_research
  sh scripts/container-test.sh --release --test bloom_path
  sh scripts/container-test.sh --release --test async_build
  sh scripts/run-sweep.sh
  ```

---

## Gate 7 — Portable format

`.ekb` files can be written, closed, reopened, verified and read correctly.

- **Implements:** `src/format.rs` (fixed records, codecs, checksums),
  `src/writer.rs` (publication), `src/reader.rs` (validated open, generation
  selection, `read`).
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
  `truncate_to_valid`), `src/writer.rs` (footer-after-payload order).
- **Proves:**
  - `tests/crash_recovery.rs::truncate_mid_footer_keeps_previous_generation`,
    `corrupt_newest_section_payload_keeps_previous_generation`,
    `trailing_garbage_keeps_newest_generation_and_truncates`,
    `fully_corrupt_commit_is_rejected_not_ignored`;
  - `tests/correctness.rs::category_interrupted_publication_torn_file`,
    `category_corrupted_file_rejected`.
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test crash_recovery
  sh scripts/container-test.sh --release --test correctness
  ```

---

## Gate 9 — Speed measurement

A real speed-versus-manifested-space frontier exists.

- **Implements:** `src/sweep.rs` (`manifestation_sweep`, `write_evidence_json`),
  `src/fastpath.rs` (optimised paths), `src/metrics.rs` (timing/percentiles);
  driver `examples/sweep.rs`; harness `benches/ekbloom.rs`.
- **Proves:** `tests/fastpath.rs::fast_paths_are_byte_exact_for_every_profile`
  (optimised paths are byte-identical to `read`), and the generated frontier.
- **Evidence:** [`../evidence/bench/sweep.json`](../evidence/bench/sweep.json),
  [`../evidence/bench/cpu.json`](../evidence/bench/cpu.json).
- **Reproduce:**

  ```sh
  sh scripts/container-test.sh --release --test fastpath
  sh scripts/run-sweep.sh
  sh scripts/container-bench.sh
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
  ```
