# Adversarial review — EKBLOOM 0.6

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
  not adversarially exercised by *this* pass. The async `io_uring` backend and its
  `unsafe` were subsequently reviewed SAFE and exercised in
  `docs/reviews/security.md` ("Unsafe review (`src/async_io.rs`)") and
  `sh scripts/container-async-io.sh`.
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

**Update (deflaked at the test level).** Reproduced on this host at 11/30
`--release` runs (`sweep_curves` file, parallel test threads), with the flake now
observed between sparse intervals (`sparse(K=64)` vs `sparse(K=1024)` vs
`sparse(K=256)`), not only path depths. The production selection was *not*
changed: making it fully timing-independent would replace the documented
"fastest measured profile" rule with a static ranking, i.e. change production
behaviour, which is out of scope. Instead the unit tests were rewritten to assert
only deterministic, non-timing properties:
`tests/sweep_curves.rs::budget_frontier_scaffolding_is_seed_deterministic` pins
the row count, the echoed ratios and the forced `root` choice at ratio 1.0; the
`"improved"` / ns-per-read / throughput claims were removed from
`tests/adaptive.rs` (`adapt_preserves_earlier_generations`,
`adapt_to_hot_set_is_additive_and_linked`). The measured instability and the
"shell is measurably faster" evidence now live in `examples/sweep.rs` ->
`evidence/bench/budget-frontier-determinism.json` and `adaptive.json`. 15
consecutive full `--release` suite runs (325 tests x 36 binaries each) finished
with 0 failures.

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

---

## Revision 3 — `prefix`, `packed`, `tiles`, `handle`, `planner`, `court`

Independent third pass over the round-3 modules. Executable in
`tests/adversarial_r2.rs` (11 tests) plus new fuzz targets `fuzz_packed` and
`fuzz_tiles` (registered in `fuzz/Cargo.toml`). Hostile `.ekb` images are produced
by mutating a real body and re-serialising with the writer (so the section CRC-32
and footer checksum are valid), or by patching the writer's output and
recomputing both CRCs with an inline IEEE CRC-32.

### Attack 7 result — fuzz campaign

`sh scripts/container-fuzz.sh` runs **all six** targets clean
(`fuzz_machine`, `fuzz_open`, `fuzz_bloom`, `fuzz_history_store`, `fuzz_packed`,
`fuzz_tiles`); the script's target list is kept in sync with `fuzz/Cargo.toml`.
The two new targets `fuzz_packed` (packed restart/history parsers +
`state_from_hot`) and `fuzz_tiles` (`parse_tile_set_prefix` / `TileSet::from_bytes`
/ tile reads) were verified here: 40,000 runs each, no crash artifacts, and clean
via the script.

### F6 (defect, fixed) — `TileSet::from_bytes` panic on an out-of-range `tile_shift`

Summary of Finding D in `security.md`. `reader::open_bytes` -> (`src/reader.rs:219`)
`TileSet::from_bytes` trusted the wire `tile_shift`; `1u64 << tile_shift` panicked
for `>= 64` and shifts `0` / `31..=63` were silently accepted. Fixed with the
`1..=MAX_TILE_SHIFT` guard (`src/tiles.rs`).

Repro (failing before, passing after):

```sh
sh scripts/container-test.sh --test adversarial_r2
```

`tile_set_from_bytes_rejects_out_of_range_shift` and
`persisted_out_of_range_tile_shift_is_rejected_not_panicked`; both aborted at
`src/tiles.rs:635` with the guard removed.

### Attacks that held (Revision 3)

* **`prefix`** — affine summaries/prefix scan/parallel reconstruction/positional
  state are byte-exact to the machine on empty, all-zero, all-`0xFF`, 1-byte and
  non-dividing-block corpora; `LinMap` compose/power match the bit-matrix action
  and are associative on random maps.
* **`packed`** — snapshot/history equal `state_log` for every position and every
  stride `0..=63`; crafted/truncated bytes give clean `Err`s; `u16` is a type
  invariant, not a runtime hope.
* **`tiles` persistence** — tampered blob with both CRCs recomputed is rejected at
  `open`/`FileBody`/`OpenObject`; boundary/empty/degenerate reads are exact; budget
  cap never exceeded; no-fit is inert; radix empty groups correct.
* **`handle`** — append invalidates the cached handle (new generation, fresh
  `Arc`); held snapshots never tear under concurrent appends and readers.
* **`planner`** — byte-exact to `reader::read` for adversarial requests; selects
  only certified paths; `path_for` is a pure shift/index with no search.
* **`court`** — 0 reps / empty lists / `ws = 0` / `ws < request` never panic; empty
  reports serialise; `n = 1` CI is the sample.

### Residual risk (Revision 3)

Same integrity-not-authenticity boundary as Findings C/F2: a forged `TileSet` or a
forged `Certified` path-bloom label is trusted by `read`/`planner` (only
`verify`/opt-in `AUTH` disagree). The planner is a certified-*dispatch* layer, not
an authenticity boundary. No memory-safety or panic defect remains after F6.

---

## Revision 4 — `blocks`, `engine`, region-scoped cursor, `checkpoint`/`packed` (verdict **TWO DEFECTS, BOTH FIXED**)

Independent fourth pass over the newest round. Executable in
`tests/adversarial_r4.rs` (12 tests), plus the new fuzz target `fuzz_blocks`
(registered in `fuzz/Cargo.toml` **and** added to the `scripts/container-fuzz.sh`
target list — without that one-line addition the campaign would not exercise it).
Every oracle is the authoritative machine or `reader::read`; hostile bodies are
mutated in memory and their linked certificate re-sealed where needed.

### Finding E (**Medium**, FIXED) — `verify` ignored the machine-2 successor cursor

Originally `src/verify.rs::check_regional` re-derived only `(symbols, next_state)`
per region window, and `src/bloom_region.rs::RegionTable::from_shell` parsed the
trailing `next_cursor u64` and **discarded** it (`let _next_cursor = ...`). For a
cursor-divergent machine (`OrANS-RANS0`) that cursor is exactly the field that
makes a table resumable, and `region_adapt::RegionReader::read` consumes it
verbatim when it leaps. A self-consistent forged body therefore passed
`verify_deep` (`ok == true`) while `read_region_scoped` returned bytes disagreeing
with `reader::read` — a `verify`-completeness gap of the same class as F3.

**Resolution.** `check_regional` now takes the authoritative cursor log
(`region_adapt::cursor_log_with`, materialised in `run` only for a
divergent machine) and both re-derives the per-key successor cursor and compares
it against the stored records (`region_adapt::region_stored_cursors`); a mismatch
— or a cursor that diverges across windows sharing a key — fails the check and
forces the recomputed status to `Rejected`. A forged cursor now **fails**
`verify`, so the region-scoped read can no longer be steered by an
integrity-consistent forgery.

Regression tests: `verify::forged_regional_cursor_is_detected` (a flipped stored
cursor fails `verify`, with `(symbols, next_state)` untouched) and
`verify::machine1_regional_identity_cursor_verifies` (machine-1 identity cursor
unchanged). My end-to-end
`tests/adversarial_r4.rs::verify_rejects_a_wrong_machine2_region_cursor_used_by_read`
was flipped from the old gap reproducer to a rejection assertion. Honest output was
already exact (`build_region_shell` treats a cursor divergence as a key conflict;
`machine2_region_successor_cursor_is_stored_and_authoritative`,
`machine2_region_reads_are_exact_across_boundaries_and_after_reopen`).

### Attacks that held (Revision 4)

* **Region cursor storage (machine 2).** Every stored successor cursor equals the
  authoritative `cursors[i+D]` for its window key on the honest corpus; reads are
  byte-exact across **every** regional boundary, the final partial region, single
  bytes at region ends, and after serialize+reopen. Machine 1 stores **no** cursor
  bytes (`table_len == entries*(D+6)`), and its regional reads are byte-identical
  before and after reopen (the cursor column does not perturb machine 1).
* **`blocks`.** `block_bytes` extremes `0`, `1`, `3`, `1<<30`, `(1<<30)+1`
  (`build`/`from_bytes` reject `0` and `>1<<30`, accept the rest; `1<<30` yields a
  single restart); every one of the `to_bytes` truncation points, bad magic /
  version / size / trailing bytes → clean `Err`; a wrong state **or** cursor is
  rejected by both `certify` and `verify_against`; `read_range` is byte-exact for
  both machines on boundary/partial/zero-length probes (B ∈ {1,2,3,7,64,256,1024,
  4096}) and after reopen; the exact step count is `offset % B + len ≤ B-1+len`.
  A body carrying a lying restart is rejected by `verify`; the engine admits it on
  in-file geometry alone and would serve wrong bytes — the documented F2 boundary,
  with `verify` the authority.
* **`checkpoint`/`packed` wire.** Legacy (12-byte, magic-less) and compact
  (`EKBCKPT`) both round-trip; bad version / format / interval / zero count /
  truncation / trailing bytes are clean `Err`s; no ambiguous parse: a payload
  beginning with the magic but a wrong version is rejected, and the legacy
  interval that spells `EKBC` is simply an invalid interval. `PackedHistory` /
  `PackedRestarts` reject `stride_shift > 63`, non-zero reserved bytes, truncation
  and trailing bytes; `state_from_hot` is exact exactly when covered.
* **`engine`.** `read` is byte-for-byte `reader::read` on both machines for
  aligned, unaligned, whole-object, zero-length, past-end and `u64::MAX` requests;
  `all_certified()` holds; a body with no bounded structure is rejected (never a
  genesis replay); the transactional budget tests remain green.
* **`machine_rans` prepared context.** `RansContext` reproduces the per-symbol
  authoritative symbol **and** state sequence for empty, all-`0xFF`, constant,
  skewed and uniform corpora; truncated headers, bad frequency sums and zero
  frequencies are clean `Err`s; two contexts for different cores do not interfere.

### Fuzz campaign (Revision 4)

`sh scripts/container-fuzz.sh` now runs **all seven** targets clean. The new
`fuzz_blocks` (prefix/full parser + restart read path, both machines) ran 20,000
runs with no crash; the other six targets pass on the same invocation.

### Evidence (Revision 4)

`sh scripts/container-reproduce.sh` completed (clean `--locked` build, full
`--release` suite **445 passed / 0 failed / 0 ignored**, then evidence
validation). `sh scripts/validate-evidence.sh` reports **27** evidence JSON files
`OK`.

### Residual risk (Revision 4)

* Findings E and F are **RESOLVED** (see above); no open defect from this pass.
* As everywhere: checksums/digests are integrity, not authenticity; only opt-in
  `AUTH` is an authenticity boundary.

### Finding F (**Low**, FIXED) — `RegionView::covers` overflowed on a forged region

`src/region_adapt.rs::RegionView::covers` computed `self.start + self.len` with an
unchecked add. A forged regional shell whose `len == shell.logical_len` is near
`u64::MAX` (which `open` and `RegionView::parse` accept structurally, since only
the length *equality* is checked) could overflow that add — a debug-build panic
reachable from `open_bytes` + `read_region_scoped`. Not memory-unsafe and not
reachable through the honest writer.

**Resolution.** `covers` now uses `self.start.checked_add(self.len)`; on overflow
it returns `false` (the position is never covered), so the read path cleanly falls
back to bounded restart. Regression test:
`region_adapt::tests::covers_rejects_overflowing_forged_region`.

## Revision 5 — machines 3/4, `batch`, `ResidentView`, AUTH byte-coverage, FUSE (verdict **NO DEFECT FOUND**)

Round 5 attacks the newest L1–L4 surface. All attacks are executable and live in
`tests/adversarial_r5.rs` (13 tests, all green) plus a non-mount fallback case in
`tests/fuse.rs`. No library defect was found, so there is no new finding to FIX or
REPORT; the one non-test change was a chore (below).

### Attack 8 — machine 3 (identity) and machine 4 (interleaved rANS)

* **Roundtrip.** Machine 3: `encode == source` for every category, `decode` exact, and
  a mid-stream `(0, p)` restart reveals `src[p..]` for `p ∈ [0,8]`. Machine 4: for
  every category and `K ∈ {1,2,4,8}`, `skip(p)` + bulk decode equals `src[p..]`,
  stepwise `reveal` equals the bulk decode, and the symbols equal machine 2's oracle
  for **all** `K ∈ [1,8]` — lanes never poison each other.
* **Cross-oracle.** `reader::read` equals the source for machines 3 and 4 over
  `Root`/`Sparse`/`Path`/`MaxSpeed` at every boundary range (`(0,1)`, `(1,4095)`,
  `(4095,2)`, `(2048,300)`, `(n-5,5)`, `(n,0)`).
* **Hostile headers.** `[]` / 100 / 515 zero bytes; `K ∈ {0,9,u32::MAX}`; a lane table
  that does not fit; zero frequency; frequency sum `≠ M`; a lane state below
  `RANS_L`; a short renormalisation stream; 64 random 600-byte images — every entry
  point (`from_core`, `decode`, `check_core`, `state_log`, `reveal`, `peek`, `skip`)
  is a clean `Err`, never a panic and never an attacker-sized allocation.
* **Arithmetic safety (why no finding).** The fused-table invariant `slot >= cum`
  makes `(x>>12)*f + slot - c` non-narrowing for *any* lane state (even one the
  header did not range-check), and `f ≤ 3841`, `x>>12 ≤ 2^20−1`, so the product stays
  `< 2^32`: no debug-build overflow even on a forged state — the same reasoning that
  clears machine 2 in Revision 4.

### Attack 9 — `ResidentView`

Byte-exact against `reader::read` for **machines 1–4** over both a whole-object
`HotTile` plan and a `Path`/`DirectStorage` plan, at start/end/boundary offsets
(`{0,1,4095,4096,4097,8191,n/2,n-2,n-1}`) and a 300-byte cross-tile window.
`get_checked(n)`, `get_checked(u64::MAX)`, `copy_range(n,..)`, `copy_range(u64::MAX,..)`
are `None`/`false`; `get` past the end panics exactly as slice indexing does. The
engine refuses a mixed-path plan (no single resident slice).

### Attack 10 — AUTH byte-coverage (hunting for an uncovered section)

The committed `tests/auth_coverage.rs` already tampers every non-AUTH section of a
rich body — history/**segmented**, packed checkpoints, **all** bloom kinds incl.
regional, tiles, blocks, and a self-consistent core+manifest forge — with the
section CRC recomputed, and every case is rejected by `open_bytes_require_auth` /
`verify_file_require_auth`; unkeyed and wrong-key files are rejected in strict mode;
honest files pass. R5 pushed on the region **outside** the literal MAC range
`[0, auth.header_offset)` and found only fail-closed behaviour:

1. **Footer** (not MAC-covered): flipping its generation and re-sealing its CRC is
   rejected, because the footer is cross-checked against the covered manifest.
2. **Trailing bytes** after the last footer: dropped as a torn tail; the reopened
   object and every read are unchanged.
3. **The AUTH tag itself** (also outside its own range): flipping a payload byte and
   re-sealing the section CRC is rejected by the keyed MAC.
4. **Appended generation** (the sharpest probe): appending a structurally valid but
   **unkeyed** generation (`writer::append_generation`), or one authenticated under
   the **wrong key** (`append_generation_authenticated` with a different key), does
   not inject content — `open_bytes_require_auth` skips it and returns the honest
   authenticated generation, byte-exact. The unkeyed reader does see the append,
   which is precisely the trust boundary opt-in AUTH draws.

**Verdict:** no uncovered section can change a read; the only uncovered bytes (the
footer, the AUTH tag, and post-footer tail) fail closed.

### Attack 11 — `select_machine`/`auto` and `batch::BatchReconstructor`

`auto` selects machine 3 for incompressible data (`V == N <= raw`) and machine 2 for
compressible data (`V < N`); explicit machine ids are honoured exactly. The batched
reconstructor is byte-exact vs the authoritative machine for machines 1/3/4 at
thread counts `{0,1,2,3,5,7,8,16,64}`, for empty/single/large and boundary-sized
batches, preserves output order, and never silently takes an accelerator path
(`accelerator_available() == false`).

### Attack 12 — FUSE `ReadMode::Auto`

A surface is attached only when it is a fully materialised, logically-aligned object
(`OrFs::aligned_surface` is `None` for a `Root` object), so reads fall back to
`OrFs::read`; both the surface and decode paths are byte-exact across
region-crossing windows. Exercised by the surface-vs-decode unit test and by the new
non-mount `tests/fuse.rs::auto_uses_a_surface_only_when_present_and_the_decoder_is_byte_exact`,
which runs in the ordinary gate; the real-mount tests also pass in this environment.

### Fuzz campaign (Revision 5)

`sh scripts/container-fuzz.sh` runs **all nine** targets clean: `fuzz_batch` (34,466
runs, 61 s) and `fuzz_rans_i` (100,000 runs, 48 s) are joined by the seven existing
targets; no crash, no leak, and the build is warning-free.

### Evidence (Revision 5)

`sh scripts/container-reproduce.sh` completed: clean `--locked --release` build, full
`--release` suite **523 passed / 0 failed / 0 ignored**, then
`sh scripts/validate-evidence.sh` → **28** evidence JSON files `OK`. The verify gate
(`cargo fmt --check`, `clippy -D warnings`, `cargo test --all-features`) is green at
**535 passed** (13 R5 + 3 FUSE).

### Chore (non-library)

`fuzz/fuzz_targets/fuzz_batch.rs` imported `OrMachine` unused (trait methods resolve
through the `dyn OrMachine` receiver); removed so the fuzz build is warning-free. No
library behaviour changed.

### Residual risk (Revision 5)

* **No defect found**; no open finding from this pass.
* The commit `footer` and the `AUTH` tag lie outside the MAC's literal range; their
  rejection rests on the footer↔manifest cross-checks (and, for the tag, the keyed
  hash). A future footer field that is neither cross-checked nor MAC-covered would
  reopen a window — the parser currently rejects any disagreement.
* Strict authenticated open prefers the newest authenticating generation: an appended
  unauthenticated generation is a rollback/DoS, not an integrity break.
* Integrity is not authenticity; only opt-in `AUTH` is the authenticity boundary.

## Revision 6 — M1–M5 (verdict **NO DEFECT FOUND**)

The M-round surface: machine-4 complete successor state (M1), exact physical
accounting (M2), committed-vs-active budget (M2), request-level bounded work (M3),
the machine-4 engine path and `create_auto` core reuse (M5), and the pinned-hot
baseline (M4). All attacks are executable and live in `tests/adversarial_m.rs`
(13 tests, all green). No library defect was found, so there is no new finding to
FIX or REPORT; the only code changes are the new tests and one **new fuzz target**.

### Attack 1 — machine-4 complete state

Every accelerated path (DirectStorage / HotTile / PackedRestart / BlockRestart) is
forced on a machine-4 body and read **beyond** the lookup, byte-exact vs
`reader::read` and the source, before and after serialize + reopen. A
`(state,cursor)`-only reconstruction is proven lossy (`Snapshot::new` drops the
lanes) and the stored wide snapshot (cursor/lane/lane-index) is what resumes. A
forged-but-self-consistent wide record is rejected by `verify`; a cross-machine
regional read (base/cursor/wide forms for machines 1/2/3/4) is byte-exact across
and beyond the region; machines 1/2/3 record forms are unchanged.

### Attack 2 — accounting

`physical_bytes()` equals `to_bytes().len()` for a `BlockIndex` (no shared model
header re-count) and obeys the documented fixed-header relation for
`History`/`CheckpointSet`/`BloomShell`, on machines 1–4; the machine-4 wide
regional shell is counted once and `inspect`'s sum equals `physical_bytes()`.

### Attack 3 — budget

A body whose committed bytes exceed the budget is rejected even when the active
plan fits; an over-budget `n == 0` core is rejected; and the hard-cap build entry
points reject an over-budget machine-4 wide body even though the static estimate
under-counts it (the post-build re-check catches it).

### Attack 4 — request-level bound

A machine-4 body whose plan alternates HotTile/PackedRestart over many regions is
read whole-object: actual positioning ≤ `worst_case_steps_for`, byte-exact.

### Attack 5/6 — machine-4 engine + `create_auto` reuse

The prepared interleaved context and the unprepared per-read path are
byte-identical; `create_auto`/`create_auto_within_budget` are byte-identical to the
explicit path across machines/profiles and the body core is the source encode.

### Attack 7 — pinned-hot

The pinned footprint is the actual pinned bytes; the set is permanent, bounded, and
never displaced by cold lookups.

### Fuzz campaign (Revision 6)

`sh scripts/container-fuzz.sh` runs **all ten** targets clean: the nine existing
plus the new `fuzz_region` (44 097 runs / 61 s), with `fuzz_blocks` extended to
machines 3 and 4 (45 930 runs / 61 s); no crash, warning-free build.

### Evidence (Revision 6)

`sh scripts/container-reproduce.sh`: clean `--locked --release` build, **566 passed
/ 0 failed**, then `sh scripts/validate-evidence.sh` → **28** evidence JSON files
`OK`. The verify gate is green at **578 passed** (13 R6 tests).

### Residual risk (Revision 6)

* **No defect found.** The static byte estimate is the machine-1 view (under-counts
  a machine-2/4 wide checkpoint, over-counts some profiles); the hard cap survives
  via the post-build re-check, but `select_profile_within_budget` is estimate-only.
* `pinned_block_count` returns `1` for a zero hot budget (documented "at least one").
* Integrity is still not authenticity; the engine/reader trust only as far as
  `verify` does.

## Revision 7 — N1–N7 (verdict **TWO DEFECTS, BOTH FIXED**)

The N-round surface: core-identity binding (N1), the bounded encoded window (N2),
machines 5/6 (N3/N4), OrFS pointer views (N5), the budget-aware optimiser (N6),
and the end-to-end OrFS proof (N7). All attacks are executable and live in
`tests/adversarial_r7.rs` (11 tests, green); the campaign also adds the fuzz
targets `fuzz_pack`, `fuzz_seg`, and the N8 `fuzz_resource`.

### Finding G (**High**, FIXED) — block-index reads doubled the output for machines 5/6

`BlockIndex::read_range_counted` allocated `out = vec![0u8; len]`, then the `_`
arm (machines 5/6, which have no tight fill arm) called `Prepared::decode_into`,
which **appends**. A machine-5/6 block index therefore produced `2 * len` bytes
with `len` leading zeros, and `Engine::read` returned that buffer verbatim — a
wrong-length, zero-prefixed read. Repro: `block_index_reads_are_length_and_byte_exact_for_machines_five_and_six`
and `engine_identity_binding_holds_for_machines_five_and_six` (both failed before
the fix). **Fix:** `out.clear();` before the `_`-arm decode (`src/blocks.rs`).

### Finding H (**Medium**, FIXED) — `index + 1` overflow panic in machines 5/6

`PackContext::byte_at` and `SegContext::locate` built `CoreTooShort { needed: index + 1 }`
from an attacker-controlled `u64`; `byte_at(core, u64::MAX)` panicked with
`attempt to add with overflow` under overflow checks. Found by `fuzz_pack`.
**Fix:** `saturating_add(1)` at both sites; pinned by
`positional_reads_past_the_end_are_typed_errors_not_overflow_panics`.

### Attack 1 — plan identity (N1): held

Planning on body `A`, reading a different body `B` (different object, different
core hash) and a newer generation of `A` are rejected with `EngineBodyMismatch`
before any decode, for machines 5 and 6; the identity also checks machine, logical
length and object id. Same body+generation is byte-exact and equals `reader::read`.

### Attack 2 — bounded window (N2): held

With a frequency-1 model that drives many minimal-frequency boundary states, the
per-read encoded bytes stay `≤ 2·(steps+len)+64` across block boundaries (counted
through `FileBody::fetched_encoded_bytes`), byte-exact vs `reader::read`; truncation and a
cursor at the core end are typed `Err`; machines 1/3 are unaffected (rebased /
whole-core paths, `tests/window.rs`). A forged state outside `[RANS_L, RANS_L<<8)`
and a machine-4 state/lane disagreement are rejected by `validate_restart`.

### Attack 3 — packed machine 5 (N3): held (besides H)

Every position is exact for every width 1–8, including non-power-of-two widths and
the partial final word; constant (k=1) and raw/identity fallbacks are exact; a
forged header (`bits∉[1,8]`, `k>2^bits`, identity with `bits≠8`, duplicate
alphabet, truncation, trailing bytes) and an out-of-alphabet *payload* index are
clean typed errors.

### Attack 4 — segmented container 6 (N4): held

A forged directory (aliased offset, aliased offset+content-id, an over-spanning
logical length, wrong version/magic, truncation, directory-hash mismatch, a
zeroed content id) is a clean `Err`; reads are exact across every segment/window
boundary; an unknown segment machine id is rejected, so no machine 2/4 core is
silently reinterpreted.

### Attack 5 — OrFS pointer views (N5): held

`inspect_physical` equals an independent `du`-style walk; the authoritative core
is stored in exactly one file (sidecars hold no payload); `read_machine` bypasses
the hot/bloom/tile dispatch and is byte-exact for a `Path` object; FUSE
`ReadMode::Machine` attaches no surface and agrees with the decode dispatch.

### Attack 6 — optimiser (N6): held

The chosen plan is built and its `physical_bytes()` re-checked against the cap
before selection; only `all_certified` plans are evaluated; `expected_p99` equals
`evaluate(engine, held_out)` and the recorded fingerprint is that same trace (the
evaluation trace is never passed); the result is deterministic for a fixed seed.

### Attack 7 — fuzz + evidence

`sh scripts/container-fuzz.sh`: **thirteen targets PASS** (added `fuzz_pack`,
`fuzz_seg`, `fuzz_resource`). `sh scripts/container-reproduce.sh`: clean build,
**649 passed / 0 failed**, `validate-evidence.sh` → **32 OK**. Verify gate green at
**663 passed**.

### Residual risk (Revision 7)

* Two defects fixed (G, H). No unfixed read-path defect found.
* Whole-object APIs (`Pack::bulk_unpack`, `machine.decode`, whole `SegContext`
decode) allocating a core-declared logical length are now **ADDRESSED (N8)** by the
`ResourcePolicy` (1 GiB default): each site guards with `checked_default` and
returns `AllocationTooLarge` before reserving — `OrMachine::decode`/`state_log`,
`Prepared::decode_into`, `block_restarts_with` (`machine.rs`);
`RansContext`/`RansIContext::decode_into`, `Rans0`/`Rans0I::decode`/`state_log`
(`machine_rans*.rs`); `PackContext::unpack_into`; `SegContext::from_core`/
`decode_into`; `History::from_bytes`/`extend_with`; `CheckpointSet::from_bytes_with`;
the `PathBloom`/region build vectors. The bounded read paths never allocate a
declared length and are unaffected. Pinned by `tests/resource.rs` + `fuzz_resource`;
a caller may raise/lower the cap via `set_max_single_allocation`.
* A restart cursor that is in range but state-inconsistent is not re-derived at
read time; the file path relies on section CRC + `core_hash` linkage (integrity,
not authenticity). Unchanged.
* Pre-existing: a crafted `Snapshot.cursor == u64::MAX` still overflows `p + 1` in
`NL2`/`STORE` reveal; not reachable from a read path.

## Revision 8 — O1–O2 (verdict **TWO DEFECTS, BOTH FIXED**)

The O-round surface: machines 7 (`OrANS-RANS0W`) and 8 (`OrANS-RANS0IL`), the
unwired SIMD rANS kernel, the budget-aware optimiser footprint gate, and the OrFS
compacted views/framing. Executable attacks live in `tests/adversarial_o.rs`
(9 tests, green); the campaign adds `fuzz_rans_w`/`fuzz_rans_il`.

### Finding I (**Medium**, FIXED) — `cursor + 1` overflow (machines 7/8, SIMD kernel)

`CoreTooShort.needed` was built with a non-saturating `cursor + 1` (`+ 2` in machine
7's `read_word`) on an attacker-controlled cursor: `machine_rans_w.rs`,
`machine_rans_il.rs` (three sites), `simd.rs` (five sites). `cursor = u64::MAX`
panicked in any overflow-checked build. For **machine 7 this is a read path**: it is
cursor-divergent but not `is_windowed()`, so `read_via_checkpoint` loads the entry
cursor verbatim and never bounds it (`validate_restart` is skipped; only machine 1
enforces its implicit cursor) — a crafted machine-7 `.ekb` with checkpoint
`cursor = u64::MAX` reaches `read_word`. Machine 8/kernel are reachable via the
public `reveal`/`peek`/kernel API with a crafted state. **Fix:** `saturating_add` at
each site; pinned by the four `*_is_a_typed_error` tests above (all panicked before).

### Finding J (**Medium**, FIXED) — footprint gate aborted when only the base fit

`optimize` enumerated `Checkpoint`/`Block` cold geometries (never `None`) and
returned `AllocationTooLarge` when none fit the budget, *before* computing the
structureless base representation (Phase 1b). On uniform data at `budget == raw`
(the case the gate is for) it **errored instead of returning the ≤-raw base**.
Repro: uniform 64 KiB, `budget = raw` → `AllocationTooLarge { 65536, 65536 }`.
**Fix (`src/optimize.rs`):** build Phase 1b before the guard; gate both emptiness
checks on `base_reference.is_none()`. Pinned by
`optimizer_uniform_tight_budget_stays_leq_raw` (failed before).

### Attacks that held (Revision 8)

* **Machines 7/8** — exact round-trip all categories × lanes 1–8, byte-exact vs
  `reader::read` before/after reopen, complete-state checkpoint/block/`il_…` resume
  at every boundary, crafted/truncated headers clean `Err`, machines 1–6 wire-hash
  pinned.
* **SIMD kernel** — equals the scalar reference for adversarial cursors/boundaries
  (and is now total); unwired from every default path; `unsafe` confined to
  `simd`/`async_io`/`mapped`.
* **Footprint gate** — committed ≤ budget; uniform ≤ raw; constant corpus does not
  expand; only `all_certified` plans evaluated.
* **OrFS** — `inspect_physical` equals an independent `du` walk, payload stored once
  in the compacted container, corruption rejected; views are write-only (no reader),
  so no forged-view acceptance surface.

### Fuzz + evidence (Revision 8)

`container-fuzz.sh`: **fifteen** targets PASS. `container-reproduce.sh`: clean build,
**687 passed / 0 failed**, **33** evidence JSON OK. Verify gate green at **701
passed** (all-features).

### Residual risk (Revision 8)

* Findings I and J fixed; no unfixed read-path defect found.
* The pre-existing `NL2`/`STORE` `reveal`/`peek` `cursor + 1` overflow (R7) is
  unchanged (public-API-only); the same `saturating_add` would close it.
* In-range-but-inconsistent restart cursors are still not re-derived at read time
  (integrity, not authenticity) — unchanged.

## Revision 9 — P1–P8 (verdict **NO LIBRARY DEFECT FOUND; one fuzz-coverage gap fixed, two doc defects reported**)

Attacks are executable in `tests/adversarial_p.rs` (7 tests, green), written against
the public API as an independent oracle. Results: verify gate green (all-features
**779 passed / 0 failed**), gates **G1–G5 PASS**, fuzz **17/17 targets PASS**,
reproduce clean + **46** evidence JSON OK (release **765 passed / 0 failed**).

### Finding K (**Low**, fuzz coverage, FIXED) — the machine-6 workaround hid the new decode

`fuzz_roundtrip` skipped machine 6 and called `SegContext::decode_into` directly
(a leftover from when the trait `decode` was `O(N^2)`), so the SEG-linear commit's
`Seg::decode`/`state_log` overrides were **not** fuzzed. Removed: `machine.decode` is
now called for machines 1–9. (fuzz-only edit.)

### Attacks tried

* **P1 versioning** — machines **1–9** write→read→verify byte-exact; future
  `format_major`/`minor` and future `core_version` typed-rejected for every machine
  (incl. 9); legacy version-0 reads. **(held)**
* **P2 streaming** — variable (checkpointed) layout: `read_at` at every block edge,
  the final partial block, arbitrary spans, `read_sequential` == source; tampered
  payload/directory and truncation typed `Err`; builder memory one block +
  documented O(blocks) directory. **(held)**
* **P5 dictionary** — wrong/absent/tampered/relabelled id, bad state, length
  mismatch all typed `Err`; embedded vs referenced exact; relabelled referenced core
  rejected by core linkage in a body. **(held)**
* **P3 corruption** — independent >2000 single-bit flips over core + manifest payload
  + full footer, **all detected**; padding/inert bytes never interpreted. **(held)**
* **P4 unsafe/alloc gate** — `unsafe` confined to the three sanctioned modules and
  the script fails closed; production `.unwrap()/.expect(/panic` allowlist minimal
  (0/4/0). **(held by inspection)**
* **SEG linearity** — `decode`/`state_log` byte-equal to the unmodified per-symbol
  `reveal` reference (incl. non-power-of-two n), mismatch rejected; reader/verify/
  stream all route through it. **(held)**

### Residual risk (Revision 9)

* `read_sequential` reserves `Vec::with_capacity(block logical_len)` (attacker-
  controlled to `block_bytes`, `u32`) *before* the per-machine `checked_default`
  guard; the decode still returns `AllocationTooLarge`, but a crafted container can
  transiently request up to ≈4 GiB of address space. Add a `checked_default` before
  the reservation. **(Addressed in the follow-up commit:** `read_sequential` now
  calls `resource::checked_default(e.logical_len)?` immediately before the
  `Vec::with_capacity`.)**
* A referenced dictionary core relabelled to a *different registered* id has no
  in-core binding (caught by the body's `core_hash` linkage, not by the bare machine)
  — integrity ≠ authenticity; only `AUTH` covers it.
* Doc drift reported (not fixed): `MIGRATION.md` / `CORE-LAYOUT.md` still say
  "machines 1–8", and `MIGRATION.md`'s worked example reuses `id = 9`, now
  `OrANS-DICT`. **(Addressed in the follow-up commit:** both docs now say machines
  1–9 and the worked example allocates `MACHINE_NL2P = 10`.)**

  ## Revision 10 — Q/R rounds (verdict **NO LIBRARY DEFECT FOUND; one test-hardening fix, one honesty observation**)

  Independent attacks over the Q/R surface, executable in the committed suites
  (`tests/compat_fixtures.rs`, `tests/simd.rs`, `tests/loom_rcu.rs`,
  `tests/dictionary.rs`, `tests/faults.rs`, `tests/crossfs.rs`) and re-run in the
  pinned 4 GiB container: verify **806/0**, reproduce clean `--locked` + 53
  evidence JSON, loom 6 models, faults **5,933/5,933**, matrix `{simd,scalar,arm64}`
  PASS (footprint 15,478 B identical), fuzz **19/19**, cross-FS 3 filesystems.
  *(Verify was 805 at review time; the follow-up `cpu_features` fix adds one test.)*

  ### Finding L (**Low**, test coverage, FIXED) — fixtures not pinned by a whole-file digest

  The frozen-fixture manifest asserted header fields, length, `physical_bytes`,
  `logical_len`/`machine_id`, 12 sampled `byte_at`s + their hash, and
  `verify`/`verify_deep` — **not** the whole file. A regenerated fixture preserving
  those fields would have passed. FIXED: `tests/fixtures/expected.json` gained
  `file_blake3` per record and `tests/compat_fixtures.rs` asserts
  `blake3(file) == file_blake3` **before** `open`, so any changed byte fails. The
  frozen `.ekb` bytes were not touched; 3 compat tests green; fmt clean.

  ### Attacks tried

  * **Q1 fixture tamper** — flip a core byte and a footer byte → `open` refuses both; a byte-identical read-back proves the artifact is never rewritten; `every_fixture_file_is_recorded_in_the_manifest` forbids add/drop. Regeneration now also fails the new whole-file digest. **(held)**
  * **Q2 forced-scalar hunt** — grepped every `is_x86_feature_detected!`/`std::arch`/`_mm*` site: all SIMD is probe-gated in `simd.rs`; `cpu_features()` only reports. `EKB_NO_SIMD=1` byte-identical everywhere. **(held)**
  * **Q4 torn-read schedule** — RCU swap of an immutable `Arc<OpenObject>` under the map mutex; the handle holds an fd, so neither append nor rename retargets it. Loom explores the real types; no torn read. **(held)**
  * **R1 dictionary** — unregistered/wrong/tampered/relabelled id, bad table, bad state, length mismatch → typed `Err`; shapes + embedded/referenced exact; declared-length guard present. **(held)**
  * **R2 fault offsets** — every stream byte (incl. `finish`), every append truncation, publish fsync/rename boundaries; previous generation always intact, recovery clean, 0 torn. **(held)**
  * **R2 cross-FS** — overlayfs/tmpfs/ext2-ext3 discovered honestly; **xfs/btrfs blocked** (no `mkfs`); all identical. **(held)**

  ### Residual risk (Revision 10)

  * Fault images are truncation-shaped; a full-length-but-partially-durable `fsync`
  failure is unmodellable (covered by the corruption court / `crash_recovery.rs`).
  * `file_blake3` lives in the manifest, not inside the `.ekb`; regenerating and
  editing the manifest together still passes (a deliberate, trusted edit).
  * `fastpath::cpu_features()` originally ignored `force_scalar()`, so a `cpu`-recording
  artifact produced under `EKB_NO_SIMD=1` would misreport AVX2 (no committed scalar
  artifact does; it never changes a byte). **(Addressed in the follow-up commit:**
  `cpu_features()` now reports the **effective** (scalar-aware) capability — clearing
  the SIMD fields and setting `forced_scalar` under forced scalar — with
  `hardware_features()` kept for the raw facts (`cpu_features_reports_no_simd_under_forced_scalar`).)**
