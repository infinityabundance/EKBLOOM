# Adversarial review — EKBLOOM 0.3

Independent "try to break correctness" pass over the frozen contract
(`docs/DESIGN.md`, `docs/PROOFS.md`) and the machine, reader, fast paths,
blooms, checkpoints, history, writer, recovery and verify modules.

All attacks are executable in `tests/adversarial.rs` (15 tests). Run with:

```sh
sh scripts/container-test.sh --release --test adversarial
```

## Findings

### F1 (defect, fixed) — `fastpath::read_ranges` abort on a huge range length

`read_ranges` reserved capacity from the *sum of all requested lengths* **before**
bounds-checking each range. With a certified path bloom present, a single
adversarial range such as `(0, u64::MAX)` drove `Vec::reserve(usize::MAX)` and
aborted with `capacity overflow` — a public-API panic/abort reachable from an
otherwise harmless request. `reader::read` returns a clean `OutOfBounds` for the
same request, so the fast path violated its documented bounds/overflow parity.

Minimal safe fix applied in `src/fastpath.rs::read_ranges`: validate every range
(overflow + `end <= logical_len`) in a first pass, then reserve using only the
true output size of the validated ranges. Error semantics are unchanged
(`Overflow` / `OutOfBounds` on the first invalid range, in order).

Failing-then-passing test: `huge_lengths_are_clean_errors_not_aborts`
(confirmed to panic with `capacity overflow` before the fix and pass after).

### F2 (documented trust boundary) — `read` trusts in-file `Certified` labels

`read_via_path_bloom` gates on `shell.status == Certified`, but a `Certified`
label is just an attacker-writable field: `open_bytes` cross-checks the core hash
and structure but never re-derives the verdict. A self-consistent forgery (one
hot symbol flipped, `status` left `Certified`, section CRC recomputed by the
writer) is accepted by `open` and returned by `read`. `verify` **does** catch it.
This matches the existing "checksums protect integrity, not authenticity" model
(`tests/security.rs`, Finding C); `read` is not authoritative over hostile input,
`verify` is the integrity engine. No library change made (a fix would require
content re-verification at `open`).

Reproducer: `forged_certified_path_bloom_is_used_by_read_but_caught_by_verify`.
Gating itself is sound: `read_never_uses_uncertified_path_shell` corrupts a path
shell's symbols *and* demotes it to `Unverified`/`Rejected`; the incorrect bytes
are never returned, proving `read` skips any shell whose status is not
`Certified` and falls back to the machine.

### F3 (defect, fixed) — global bloom certification status was never checked

`check_global` reproduced `Phi_D` per key but never re-derived the certification
verdict, so a universal bloom whose honest status is `Rejected` could be
relabelled `Certified` (both the shell field and the payload copy) and `verify`
still reported `ok == true`. `check_global` now recomputes conflicts over the
object's positions (like `check_regional`) and **fails** when the re-derived
verdict is `Rejected` while the shell claims `Certified`; a re-derived
`Certified` passes, and an `Unverified` shell remains acceptable (it is never
used by `read`).

Regression: `verify_rederives_global_bloom_certification_status` (the old
documented-gap test, flipped to assert detection) and
`verify_accepts_genuinely_certified_global_bloom` (positive case).

### F4 (minor) — contextual bloom entries are not tied to the body

`check_contextual` replays each stored `(state, ctx)` internally but never
compares against the body's authoritative decode, so an internally-consistent
but body-unrelated contextual table passes. Contextual shells are not produced by
any `ManifestationProfile`, so exposure is currently nil.

## Attacks that held (code held up)

* **Fast-path integrity** — for all byte values, long runs, alternating,
  pseudorandom, and period-1/2/3/4 corpora; lengths `0..=4097` and every residue
  `0..D-1 mod D` for `D ∈ {1,2,4,8,16,32}`; lengths around every checkpoint
  interval (`4,16,64,256,1024,65536`) and large boundaries (`65535/65536/65537`):
  `read`, `read_sequential`, `read_random_1b`, `read_ranges` all equal the
  authoritative `NL2` decode, and path-bloom continuation states equal
  `state_log[i + D]`.
* **Certificate soundness** — verified per structure type (history, checkpoint,
  path/global/regional/contextual bloom, certificate, core): each planted
  corruption is detected by `verify` with the right failing check; a certificate
  field changed without recomputing the receipt is caught.
* **Boundary/overflow** — `offset+length` overflow, zero-length at and past the
  end, length beyond end, and `u64::MAX` ranges all yield clean errors, no panic
  and no attacker-sized allocation.
* **Non-withdrawal** — after `grow`, the on-disk prefix is byte-identical and a
  torn newest commit falls back to the older generation, which still reads the
  exact original bytes.
* **MANIFEST mismatch** — a header whose declared logical length disagrees with
  the `MANIFEST` is rejected at `open` (`CorruptGeneration`); an oversized
  `core_len` is a clean error.

## Residual attack surface not covered

* No fuzzing of arbitrary section *kinds* reordered within a commit, nor
  multi-generation files where an intermediate commit is mutated.
* BLAKE3/CRC collision resistance assumed; forging a structure that keeps both
  the core digest and its own CRC valid is out of scope (F2 assumes a
  self-consistent file, which the writer makes easy).
* `io-uring` backend, `filesystem`/`OrFs`, async publication, and the CLI were
  not adversarially exercised here.
* `Certificate.status` vs bloom `status` agreement is unchecked (subsumed by F3).
* Contextual/regional certification status semantics for multi-region objects
  were only spot-checked for one region size.

---

## Revision 2 — 0.3 additions (segmented history, budgets, adaptive shells, O8)

Independent second pass over the code added since the first review:
`history_store.rs`, the `HISTORY_SEGMENTED` reader path, `bloom.rs`
`Bloom4Resume`, `manifest.rs` budget functions, `adapt.rs` (O7) and the O8
shell readers, plus `simd.rs::gather_bytes_avx512`. Executable in
`tests/segmented_hardening.rs` (12 tests) and the new fuzz target
`fuzz/fuzz_targets/fuzz_history_store.rs`. Hostile `.ekb` images are produced by
patching the real writer's output and recomputing the section CRC-32 in test
code, so the streaming parser is actually reached.

### Verdicts

* **Segmented index/segment parsing (held).** Corrupt header fields (zero /
  non-multiple-of-4 `segment_bytes`, bad interval, `MAX` counts/offsets,
  flipped `core_hash`) are clean `Err`s at `open`. Hostile index entries below
  `states_offset`, straddling `checkpoints_offset`, far past the file, or at
  `u64::MAX` are clean errors from the range guard or the checked arithmetic in
  `FileSegmentedHistory::state_at`; no panic, no OOB, no attacker-sized read.
  Truncated/missing/zero-length segment files error only for the mapped
  positions; a flipped on-disk state is returned by `state_at` (no panic) and
  caught by `verify_at`.
* **`Bloom4Resume::unpack` (held).** Every length `< 16` is `Truncated`; the
  16-byte form round-trips exactly and preserves the `u64` cursor; trailing
  bytes are ignored as documented.
* **Budget transactional integrity (held).** A rejected/`Err`
  `manifest_within_budget` leaves a canonical serialization of every
  byte-carrying field bit-for-bit unchanged, with `physical_bytes()` and
  `generation` unchanged; `create_within_budget` one byte short yields no body.
* **`estimate_physical_bytes` (held, no counterexample).** Exact identities for
  `Root`/`Sparse`/`Dense`/`Full`/`Bloom`/`Path`/`MaxSpeed` and a deliberate
  over-count for `Regional`; 32 lengths x 37 profile parameterizations (1,419
  builds) all asserted `estimate >= actual`.
* **O7 additive claim (held).** Adapting a body that already carries a
  checkpoint + global bloom appends a path shell as a new generation; every
  earlier structure is byte-identical and `append_generation` leaves the earlier
  on-disk bytes as an exact prefix (verified by `starts_with`). The reopened file
  still verifies, the pre-existing bloom survives, and reads are unchanged. A
  body that already carries a certified path bloom is a true no-op (bit-for-bit
  unchanged, no generation bump).
* **O8 byte-exactness (held).** For `D in {1,2,4,8,16,32}` and lengths around
  block boundaries (`D-1, D, D+1, 2D-1, 2D, 2D+1`) including the final partial
  block, the `Path`/`Checkpoint`/`History`/`Machine` shells all return the same
  bytes as `read` and the source, and empty reads agree. File-backed reads equal
  the in-memory reads.
* **AVX-512 gather (held).** See the addendum in `security.md`: identical to the
  scalar reference including boundary/overflow cases; not wired into any default
  path.

### F5 (flaky determinism, reported) — `sweep::budget_frontier` is not
seed-deterministic

`budget_frontier` claims determinism for a fixed seed and
`tests/sweep_curves.rs::budget_frontier_is_deterministic_for_a_fixed_seed`
asserts it, but selection is decided by *measured* timings. Speed scores within
`SPEED_TOLERANCE_OCTAVES = 1.0` are treated as a tie and broken by
`physical_bytes`; timing noise can move two path depths across that 1-octave
boundary between calls, flipping the winner. Repro:

```sh
sh scripts/container-test.sh --release --test sweep_curves \
    budget_frontier_is_deterministic_for_a_fixed_seed
```

observed `path(D=16)` vs `path(D=32)`, then `path(D=8)` vs `path(D=32)`, ~1 in
5 release runs (passes in debug and in isolation). Root cause is measurement
noise, not the machine or the reader. Reported rather than fixed: `src/sweep.rs`
and this test are outside the modules under review, and the honest fix (a
coarser/derived tolerance or removing timing from the tie-break) is a design
choice. No core correctness impact.

### Fuzz target added

`fuzz_history_store` embeds a fuzz-derived store, corrupts the section
(header/index/states) with the CRC fixed, and queries in-range and out-of-range
positions; 357,482 runs clean (cov 1119), no crash. Note the shipped
`scripts/container-fuzz.sh` lists targets explicitly and must be extended (or
run directly) to include it.

### Still not attacked

* `HISTORY_SEGMENTED` **states** are unauthenticated (unkeyed CRC + recomputable
  `core_hash`); `open`/`verify` do not represent the segment in `EkbloomBody`, so
  `segmented_state_at` returns forger-chosen, bounds-checked states. Same
  integrity-not-authenticity model as F2/Finding C; only opt-in `AUTH` covers it.
* The AVX-512 path was exercised by equality tests and the scalar fallback, not
  by a dedicated fuzzer; it is not on the default path.
