# EKBLOOM `.ekb` / wire-form security review

Scope: untrusted-input handling on the `.ekb` read path — `format`, `reader`,
`recovery`, `history`, `checkpoint`, all bloom `from_bytes`/`from_shell`,
`filesystem` index parsing, and publication races. Adversarial method:
read the parsers, then craft concrete byte images and feed them to
`open_bytes`/`from_bytes`/`verify`, asserting a clean `Err` (never panic, never
OOM). The crate's `unsafe` is confined to exactly **three** sanctioned modules —
`src/simd.rs` (the feature-gated AVX2/AVX-512 gathers), `src/async_io.rs`
(the `io-uring` submission-queue pushes), and `src/mapped.rs` (the read-only
`mmap`/`madvise`/`munmap` mapped surface); each is independently reviewed below.
Every `unsafe` block carries a `// SAFETY:` justification; other `unsafe`
mentions in the tree are prose. All tooling ran via
`scripts/container-verify.sh` (8 GiB cap).

## Findings

| # | Severity | Location | Concrete input | Status |
|---|---|---|---|---|
| A | **High** | `src/reader.rs:161` (guard added), sink `src/reader.rs:295` | Header/manifest declare `logical_len = u64::MAX` over a 3-byte core plus a checksum-valid 1-entry checkpoint → `read` did `Vec::with_capacity(length)` from the declaration (OOM/abort vector) | **Fixed** |
| B | Low | `src/format.rs:218` | Header `feature_flags = 1` was parsed and ignored (unknown semantics accepted) | **Fixed** |
| C | Medium | `src/reader.rs` (open/read paths) | Forged-but-checksummed checkpoint/`history`/path-bloom **states** are accepted by `open`; `read` then returns attacker-chosen bytes silently. `verify` detects it. | **Mitigated**: `core_hash` linkage cross-checked at open; state-forgery residual below |

### A — untrusted declaration sized a heap allocation (fixed)
`read` bounds only by `logical_len`, which `open_bytes` never tied to the core
length. A tiny, *self-consistent* file (`write_body` of a body with
`logical_len` inflated and one `CheckpointSet{interval:1,count:1}`) passed
`open`, and `read_via_checkpoint` allocated `Vec::with_capacity(length)` before
any replay could fail with `CoreTooShort`. `u64::MAX` → abort.
Fix (`src/reader.rs`): reject `manifest.logical_len > core.len()` for
`MACHINE_NL2` at open (one core byte per symbol is the frozen contract), so all
read capacities are bounded by input size. Regression:
`tests/security.rs::huge_logical_len_over_small_core_is_rejected`.

### B — nonzero feature flags accepted (fixed)
`GenesisHeader::decode` ignored `feature_flags` (`docs/DESIGN.md` fixes it at 0).
Fix: reject nonzero. Regression:
`tests/security.rs::nonzero_feature_flags_are_rejected`.

### C — manifested structures are integrity-checked in transit, not authenticity (mitigated for linkage)
CRC/BLAKE3 protect against accidental corruption, not forgery: an attacker who
can author a file recomputes them. `open_bytes` now recomputes `blake3(core)`
once and cross-checks it directly against **every** structure carrying a
`core_hash` (manifest, `History`, each `BloomShell`, and certificates),
rejecting with `CorruptGeneration` on any mismatch — so a history or bloom whose
`core_hash` disagrees with the core is refused at open, not just by `verify`.
This does not authenticate **states**: a forger who recomputes the digests
authors a *different valid object* (see residual risk).
Regression: `tests/security.rs::forged_structure_core_hash_is_rejected_at_open`.

## Residual risk: corruption detection is not authenticity — **ADDRESSED (opt-in)**
`.ekb` uses **unkeyed** crc32/BLAKE3 (and CRC-32 for section/footer linkage). These
detect accidental corruption only. A malicious forger with write access can
rewrite `core` **and** every structure consistently; the result is merely a
*different internally valid object*, not a detection. In particular:

- `CheckpointSet` carries **no** `core_hash` field, so a forged checkpoint state
  (cursor left legal) is caught only by replay — `verify` / `verify_deep` — never
  at `open`. The same is true of forged `hot` symbols inside an otherwise
  `Certified` path bloom.
- Therefore plain `open` is a *structural* validator, not an authenticity
  boundary.

**Resolution (this revision).** An **opt-in authenticated mode** adds a keyed-MAC
`AUTH` section. On an authenticated write, a 32-byte tag
`blake3::keyed_hash(key, blake3(file[0 .. auth_header_offset]))` is written after
the manifest and immediately before the footer; it covers the genesis header, the
core, padding, and **every** section including the manifest. New API:
`write_body_authenticated`, `append_generation_authenticated`,
`open_authenticated`, `open_bytes_authenticated`; the CLI accepts
`--key-hex`/`--key-file`. `open_authenticated` recomputes the MAC and rejects a
missing or mismatching `AUTH` (treating it as an invalid generation, never
falling back to an unauthenticated read). A consistently re-forged file — even one
that plain `open` still accepts — fails the MAC. See `docs/SECURITY.md` for the
threat model and exact coverage. Regressions: `tests/authenticated.rs`.

**Caveat — opt-in.** Protection holds only when a key is used on both write and
read. Default (unkeyed) files remain **corruption-detecting only**: without a key
the residual below still applies, and the scheme offers no anti-rollback (an
entire, genuinely authenticated older file can always be replayed).

## Adversarial authenticity (original note, now implemented)
Adversarial authenticity requires a **keyed MAC** over the committed bytes
(e.g. keyed BLAKE3/HMAC) with a key the forger lacks — now shipped as the opt-in
`AUTH` mode above; opt-in scope and anti-rollback limits are documented in
`docs/SECURITY.md`.

## Unsafe review (`src/simd.rs`) — verdict **SAFE**

This module's `unsafe` (one of the crate's **three** sanctioned `unsafe`
modules; the others, `src/async_io.rs` and `src/mapped.rs`, are reviewed later in
this document):
`gather_bytes_avx2` (`unsafe fn`, one block) plus its
one call site in `gather_bytes`. It implements a `_mm256_i32gather_epi32` gather
of many single bytes from a path-bloom `hot` array. Reviewed independently, not
wired into `fastpath` (the scalar loop remains the default; see
`docs/reviews/performance.md` §6).

**SAFETY argument.** The caller (`gather_bytes`) enters the `unsafe` block only
when `avx2_available()` (safe `is_x86_feature_detected!`) **and**
`hot.len() <= i32::MAX as usize`. Inside, `validate_offsets` runs *before any
gather*: every `u64` offset must satisfy `offset + 1 <= hot.len()` (checked add),
else `Overflow`/`OutOfBounds` with no output. The vector path is taken for a
group of eight only when `hot.len() >= 4` **and** every index `o <= len - 4`, so
each 4-byte dword read lies in `[o, o+4) ⊆ [0, len)`: no over-read, and no
page-boundary fault even when `o` is near the end. Indices are `i32` lanes with
scale 1 on a `*const i32` base; loads are `_mm256_loadu_si256` (32 bytes from a
`[i32;8]`) and `_mm_storeu_si128` (16 bytes), both unaligned and exact-sized.
Boundary groups, the tail, and `len < 4` use the validated scalar loads.

**Adversarial attempts (all held).**

- Offsets `len-1` (valid), `len`, `len+1` → agree / clean `OutOfBounds`, empty
  output. `len-4` is safe; `len-3` is forced to the scalar branch — both agree.
- Empty `hot`: `[]` agrees; any offset is `OutOfBounds`; no panic.
- **32-bit truncation probe:** with a `1 MiB` `hot` (AVX2 active), offsets
  `(1<<32)+5`, `(1<<32)+5` repeated ×8, `u32::MAX`, `u32::MAX+1`, `u64::MAX` all
  error (the first would read `hot[5]` under a silent `u32`/`i32` cast). Because
  `hot.len() <= i32::MAX`, no valid offset can exceed `i32::MAX`, so `o as i32`
  cannot truncate — **no truncation hole**.
- Long lists with non-multiple-of-8 tails and boundary indices injected mid-list
  (100,003 offsets) agree byte-for-byte; mixed groups where exactly one of eight
  is a boundary index agree.
- The `packus` compaction (`packus_epi32`→`packus_epi16`→`unpacklo_epi32`) is
  verified to leave the eight low bytes in order via the equality tests above
  (lane-0/1 analysis in the module docs is exact).

**Other-unsafe confirmation:** `grep -E 'unsafe\s*(fn|\{|impl|extern|trait)'`
across `src/` matches exactly three modules — `src/simd.rs` (the AVX2/AVX-512/
prefetch gather blocks and their call sites), `src/async_io.rs` (the two
`SubmissionQueue::push` calls), and `src/mapped.rs` (the read-only
`mmap`/`madvise`/`munmap` and pointer-view blocks), all reviewed below. These are
the crate's **three** sanctioned `unsafe` modules. Zero other `unsafe` is
present.

**Miri:** `scripts/container-miri.sh` runs `cargo miri test --lib -- machine::`,
so `simd::tests::*` is **excluded**; Miri never interprets the AVX2 intrinsics
(which it cannot execute). No Miri gap introduced.

**Residual risk.** The `unsafe` is correctness-reviewed and fuzzed only
indirectly (the `simd` module is not a fuzz target and is not on the read path).
If it were ever wired into a default path, the arithmetic here would need a fresh
review; as shipped it is opt-in and not on the critical path.

## Audit items with no findable defect
- **Checked arithmetic / truncation**: every declared length/offset/count
  (`GenesisHeader`, `SectionHeader`, `CommitFooter`, `ManifestRecord`,
  `History`, `CheckpointSet`, all bloom payloads, filesystem index) uses
  `checked_*`/`saturating_*` + `take`-style bounds checks. `u64::MAX` counts and
  payload lengths return `Err` (`declared_counts_cannot_force_huge_allocations`).
- **Overlapping/out-of-range sections**: `parse_commit` forces contiguous,
  8-aligned sections (`header.offset == header_end`, `cur = align8(payload_end)`)
  and a manifest last; footer pointers must match the section.
- **Nontermination**: both scan loops strictly advance (`cur` grows by ≥40,
  `pos = commit.end > start`), bounded by file length.
- **Unsupported versions / bad magic / header_len / depth / interval / machine**
  are rejected. Padding between structures is skipped, never interpreted.
- **Publication race**: `write_body` uses temp+rename; `append_generation` is
  prefix-preserving; `scan` treats a partial tail as a torn tail and falls back
  to the last valid commit, so a reader never accepts a partial shell.

## What I could not attack / residual risk
- No reachable `panic`/`unwrap` from parsed bytes found by inspection or
  mutation (`mutated_and_truncated_images_never_panic`); the `unsafe` in
  `src/simd.rs` is reviewed above and is not on the read path, and the `unsafe`
  in `src/async_io.rs` is reviewed below and is likewise not on the read path.
- Not exhaustively fuzzed here; a dedicated campaign (`fuzz/fuzz_targets`) plus
  Miri/sanitizers would be stronger evidence. `memmap2` is a declared dependency
  but unused in `src` (no mmap of untrusted offsets). Finding C is now mitigated
  for `core_hash` linkage at open; state-forgery authenticity (checkpoints, path
  `hot`) is not a memory-safety issue and needs a keyed MAC — now provided
  **opt-in** (see the residual risk section above and `docs/SECURITY.md`).

---

## Revision 2 — segmented history, budgets, adaptive shells, AVX-512 (verdict **SAFE**)

Scope added since Revision 1: the persistent segmented store
(`src/history_store.rs`), the `HISTORY_SEGMENTED` `.ekb` section
(`format::HS_FIXED_LEN`, `reader::{decode_segmented_header, parse_segmented,
FileSegmentedHistory::state_at}`, `FileBody::segmented_state_at`), the
`Bloom4Resume` 16-byte packing, the budget pre-filter
(`estimate_physical_bytes`), the O7 adaptive shell (`adapt_to_workload`), and the
AVX-512 gather (`simd::gather_bytes_avx512`). Method: crafted byte images with
the section CRC-32 **recomputed** so the parser is genuinely reached.

### Segmented index/segment parsing — **held**
Adversarial inputs (all with a valid section CRC so `open` reaches the parser):

- **Header fields**: `segment_bytes ∈ {0, 5, 0xFFFFFFFD}`, invalid/zero
  `checkpoint_interval`, `logical_len = u64::MAX`, `state_count ∈ {0, MAX}`,
  `segment_count ∈ {0, MAX}`, `index_offset`/`states_offset`/`checkpoints_offset
  = {0, MAX}`, `checkpoints_len = MAX`, and a flipped `core_hash` — every one is
  a clean `Err` at `open` (`decode_segmented_header` consistency checks +
  overflow-checked layout, and the `core_hash`/`logical_len` linkage check).
- **Index entries** (header untouched, so `open` succeeds and `state_at` is
  reached): `seg_rel ∈ {0, states_offset-1, checkpoints_offset-3,
  checkpoints_offset, 1<<40, u64::MAX, u64::MAX-3}` (for entry 0 and the final
  entry) are all rejected by the `seg_rel < states_offset || end_rel >
  checkpoints_offset` guard, and `u64::MAX`/`u64::MAX-3` hit the
  `checked_mul`/`checked_add` `Overflow` path. No panic, no OOB, no read past
  the declared file length (`read_exact_at_len` bounds every `pread`).
- **`u64` position math**: `within.checked_mul(4)` cannot overflow (`within <
  segment_bytes/4 ≤ (2^32-1)/4`) and every absolute offset is `checked_add`ed
  against the file length.
- **Segment files on disk**: missing, zero-length, and truncated segments all
  yield clean errors only for the positions that map into them; in-range reads
  never panic; a flipped state byte is returned by `state_at` (no panic) and
  caught by `verify_at`.

### `Bloom4Resume::unpack` — **held**
Every length `0..=15` is a clean `Truncated` (via `get(..16)`), the full 16-byte
form round-trips bit-exactly for `cursor ∈ {0, 1, DEADBEEF…, u64::MAX}` with no
narrowing, and trailing bytes are ignored exactly as documented.

### AVX-512 gather (`unsafe`, addendum to the unsafe review) — **SAFE**
`gather_bytes_avx512_impl` reuses the AVX2 page-safety argument: `validate_offsets`
runs first, a 16-lane group is gathered only when `hot.len() >= 4` **and** every
index `≤ len-4`, and boundary groups/`len < 4`/the tail use the validated scalar
loop. `hot.len() <= i32::MAX` prevents `i32` lane truncation. Equality with the
scalar path holds for empty, single, uniform, boundary, near-`u32::MAX`, and
overflowing offset lists; out-of-range offsets error identically with no partial
output. The function is **not** wired into any default path.

### Budget pre-filter (`estimate_physical_bytes`) — **held (no counterexample)**
The estimate is a hard upper bound on the built `physical_bytes()`:
`Root`/`Sparse`/`Dense`/`Full`/`Bloom`/`Path`/`MaxSpeed` are *exact* identities
(the `u32` history, `12·(⌊N/K⌋+1)` checkpoints, `D`-byte-entry universal table,
`N + 4·⌈N/D⌉` path payload, and the four length-prefixed-string certificate all
match the builders), and `Regional` is a deliberate over-count (one shell per
`⌈N/R⌉` region × `min(R, 2^16)` entries × `(D+6)` bytes + certificate).
Exhaustive search over 32 lengths (including `0`, `D±1`, `K±1`, segment
boundaries) × 37 profile parameterizations found **no** under-estimate (1,419
successful builds asserted `estimate ≥ actual`).

### Transactional budget rejection — **held**
`manifest_within_budget` clones first and only commits on success. Across
`Root`/`Sparse`/`Bloom`/`Full` starts, budgets `base ± {0,1,8,64,4096}`, and
profile attempts that all reject or fail to build (including unsupported depth
`3`), a canonical serialization of every byte-carrying field (core, history,
checkpoints, blooms, certificates, profiles, generation) is **bit-for-bit**
unchanged; `physical_bytes()` and `generation` are unchanged.
`create_within_budget` at `actual-1` returns `AllocationTooLarge` and yields no
body.

### New fuzz target — `fuzz_history_store`
Builds a store over a fuzz-derived core, embeds it via `write_history_segmented`,
then corrupts the section payload (header/index/states) with the CRC recomputed
and queries many state positions and out-of-range indices, and truncates an
on-disk segment before querying. 357,482 runs clean (cov 1119), no crash/OOM.
The shipped `scripts/container-fuzz.sh` still lists only the original three
targets, so the new target must be invoked directly (`cargo fuzz run
fuzz_history_store`); see the process note below.

### Findings and residual risk (Revision 2)
- **No memory-safety defect found** in any newly reviewed module.
- **Residual (integrity, not safety):** `EkbloomBody` has no segmented field, so
  `open`/`verify` never validate `HISTORY_SEGMENTED` *states*: `open` checks the
  `core_hash`/`logical_len` linkage, but a forger who authors the file can
  recompute the (unkeyed) section CRC and the BLAKE3 `core_hash`, after which
  `segmented_state_at` returns their chosen states (only bounds-checked, never
  authenticated; aliased/overlapping index entries return plausible-but-wrong
  states). This is the same *integrity-not-authenticity* model as Finding C;
  states are not a memory-safety boundary. `verify` does not cover the segmented
  section either (no body field), so only the opt-in `AUTH` mode authenticates
  the whole file prefix.
- **Process gap (minor):** `scripts/container-fuzz.sh` hard-codes
  `fuzz_machine fuzz_open fuzz_bloom`, so a newly added target is silently
  excluded from the shipped campaign. Consider globbing `fuzz/fuzz_targets/*.rs`.
- **Determinism (reported, not fixed — outside this revision's modules):**
  `sweep::budget_frontier` is not deterministic for a fixed seed despite its doc
  and `tests/sweep_curves.rs::budget_frontier_is_deterministic_for_a_fixed_seed`.
  Speed ties (within `SPEED_TOLERANCE_OCTAVES = 1.0`) break by `physical_bytes`,
  but genuinely measured *timings* decide the "clearly faster" branch, so timing
  noise can flip the winner between path depths across runs. Repro:
  `sh scripts/container-test.sh --release --test sweep_curves
  budget_frontier_is_deterministic_for_a_fixed_seed` failed ~1/5 release runs
  (observed `path(D=16)` vs `path(D=32)`, then `path(D=8)` vs `path(D=32)`); it
  passes in debug and in isolation. Not caused by this revision.
- **Regression suite:** new adversarial tests live in
  `tests/segmented_hardening.rs` (12 tests); the full debug suite is 300 passed / 0
  failed and the release suite is 299 / 0 with the flaky determinism test skipped.

---

## Unsafe review (`src/async_io.rs`) — verdict **SAFE**

First independent review of the crate's *second* `unsafe` module (the first is
`src/simd.rs`). The module is compiled only under `#[cfg(feature = "io-uring")]`
and is the real completion-driven publication backend for `.ekb` generations.

### Exact `unsafe` inventory (grep)

`grep -rn unsafe src/` finds `unsafe` *code* in exactly three modules:
`src/simd.rs` (`gather_bytes_avx2`, `gather_bytes_prefetch_sse`,
`gather_bytes_avx512_impl` + their three gated call sites), `src/async_io.rs`
(two `SubmissionQueue::push` calls: one write SQE, one `Fsync` SQE), and
`src/mapped.rs` (the read-only `mmap`/`madvise`/`munmap` and pointer-view
blocks). Every one carries a `// SAFETY:` comment. The string `unsafe` also
appears in doc comments
in `src/fastpath.rs` / `src/reader.rs` (asserting they contain *no* `unsafe`),
and in `tests/unsafe_review.rs` (the `simd` review), but no other `unsafe` blocks
exist. No `unsafe` in tests, benches, examples, or fuzz targets.

### SAFETY argument

* **Buffer lifetime.** `write_all` takes `data: &[u8]`; each SQE's pointer is
  `data[idx..].as_ptr()`. `write_all` reaps every submitted batch via
  `submit_and_wait(planned.len())` before returning or re-borrowing, and the caller
  (`publish`: owned `image` Vec; `append`: owned `image` Vec; `probe`: `&'static`
  literal) keeps the buffer alive across the call. Declaration order also puts the
  owned buffer *before* the `IoUring`, so on any early return the ring (and its fd)
  is dropped before the buffer. The kernel only reads until the matching CQE.
* **SQE/CQE indexing and `user_data`.** `user_data` is the payload-relative
  start offset, strictly increasing within and across batches; the CQE is matched
  back with that key, and an unmatched `user_data` is a hard error
  (`"completion with unknown user_data"`). No aliasing is possible.
* **Counts.** `planned.len()` is bounded by `sq.capacity()` and only counts
  successful `push`es; `submit_and_wait(planned.len())` then guarantees that many
  completions, and the CQ is fully drained each batch, so `done` advances by exactly
  the bytes acknowledged. `reaped == 0` is an error.
* **Bounds/offsets.** `base_offset + idx` and `len` are derived from an in-memory
  image; `len <= WRITE_CHUNK` fits the `u32` `Write::len`. `append` truncates the
  torn tail to `last.end` *before* writing at that offset, so the new block never
  overlaps a committed one.
* **Error propagation — no silent partial writes.** A negative CQE result becomes
  `io::Error::from_raw_os_error(-res)`; a positive-but-short result is a
  `WriteZero` "short write" error; a submission failure is a clean error. `publish`
  removes its `*.tmp` and returns `Err`, and `asyncpub` then falls back to the
  synchronous writer.
* **Durability ordering.** `publish`: write whole image to `*.tmp` -> fsync ->
  rename -> dir fsync. `append`: (truncate torn tail) -> write complete commit
  block -> fsync. Readers validate the footer, so a torn *tail* is ignored and an
  earlier commit is never invalidated.

### Adversarial attempts (executable)

Added to `tests/async_io.rs` (run under `io-uring`):

* `failed_append_never_corrupts_the_previous_generation` — publish gen 1, grow the
  body, `chmod 0444` the destination, call `append(..., Backend::IoUring)` => the
  async path and the sync fallback both fail; the file is **byte-for-byte the
  previous image**, `recover().newest_valid == Some(gen_before)`,
  `torn_tail_bytes == 0`, no `*.tmp` sibling, and `open`/`verify`/`read` still see
  only the previous committed generation. (On a privileged uid that bypasses the
  mode bit the test instead asserts the non-withdrawal prefix property.)
* `failed_publish_leaves_the_existing_committed_file_intact` — block the `*.tmp`
  path with a *directory* (`File::create` -> `EISDIR` for every uid); `publish`
  fails and the destination is unchanged and still verifies.
* Existing `publishing_never_exposes_a_partial_shell` covers the torn-tail crash:
  a truncated tail recovers to the previous generation and still reads the
  original bytes.

No attempt produced a corrupt or partially-published generation readable by
`open`/`recover`.

### Observations (not defects)

* `fsync_fd` does not assert `cqe.user_data() == FSYNC_TAG`; harmless because
  `write_all` fully drains the CQ first, but a defensive check would be cheaper to
  audit. On an `fsync` *failure* after a fully written commit block, the fallback
  `writer::append_generation` re-scans the file and can reject the (already
  present) generation with `InvalidArgument` — the file stays consistent either
  way, so this is an error-reporting nuance, not a durability gap.
* A `rename`/dir-fsync failure in `publish` can leave the `*.tmp` sibling behind
  (the removal is inside the earlier closure); the synchronous writer shares this
  exact shape, so it is not a regression.
* The module doc-comment in `sweep.rs` (`budget_frontier` is a pure function of
  its inputs) remains false; see F5 in `adversarial.md` (now handled at the test
  level, not in production).

### Result

`sh scripts/container-async-io.sh`: probe works end-to-end under
`--security-opt seccomp=unconfined` (byte-identical, `durability_ordered: true`,
6/6 tests pass in both stage 0 fallback and stage 1). `sh scripts/container-fuzz.sh`:
all four targets then in the campaign PASS (`fuzz_machine`, `fuzz_open`, `fuzz_bloom`,
`fuzz_history_store` @100k runs, cov 1121; the campaign now runs **fifteen**).
**Verdict: SAFE — no memory-safety or
durability defect found.**

---

## Revision 3 — `prefix`, `packed`, `tiles`, `handle`, `planner`, `court` (verdict **ONE DEFECT, FIXED**)

Independent adversarial pass over the round-3 modules. Executable in
`tests/adversarial_r2.rs` (11 tests), plus the new fuzz targets `fuzz_packed` and
`fuzz_tiles`. Method: craft byte images and hostile files from the real writer's
output, recompute section CRC-32 and commit-footer checksums where needed, and
re-derive every claimed invariant against the authoritative machine.

### Finding D (**Medium**, fixed) — `TileSet::from_bytes` trusted an out-of-range `tile_shift`

| # | Severity | Location | Concrete input | Status |
|---|---|---|---|---|
| D | **Medium** | `src/tiles.rs:609` (`from_bytes`) | A `Tiles` section whose prefix byte 9 (`tile_shift`) is `>= 64` (also `0` or `31..=63`) over an otherwise well-formed payload | **Fixed** |

`reader::open_bytes` reaches `TileSet::from_bytes` directly (`src/reader.rs:219`)
for the in-memory path. `from_bytes` read `tile_shift` and immediately computed
`logical_len.div_ceil(1u64 << tile_shift)` **without** the range check that its
sibling `parse_tile_set_prefix` (and `to_bytes`' provenance) enforce. A crafted
section with `tile_shift >= 64` therefore aborted a debug build with `attempt to
shift left with overflow` — a reachable panic from `open_bytes`/`open` on hostile
input (the file is a valid commit; the section CRC is attacker-recomputable).
Shifts `0` and `31..=63` were silently *accepted*, contradicting the module's
`1..=MAX_TILE_SHIFT` (30) contract and the `u32` directory-offset domain.

**Why the file-backed path was safe.** `FileBody::open` parses via
`parse_tiles`, which calls `parse_tile_set_prefix` and rejects the shift — so the
bug was an asymmetry between the two readers, exactly the kind an attacker
reaches through `open_bytes`/`reader::open` (both of which use the in-memory path).

**Fix (`src/tiles.rs`, 3 lines).** After reading `tile_shift`:
`if tile_shift == 0 || tile_shift > MAX_TILE_SHIFT { return Err(bad(...)); }`,
mirroring `parse_tile_set_prefix`.

**Failing-then-passing evidence.** `tile_set_from_bytes_rejects_out_of_range_shift`
(unit) and `persisted_out_of_range_tile_shift_is_rejected_not_panicked`
(end-to-end: mutate `body.tiles[0].tile_shift`, `write_body`, then `open_bytes`).
Confirmed to fail at `src/tiles.rs:635` (`attempt to shift left with overflow`)
with the guard removed, and to pass with it restored.

### Attacks that held

* **`prefix` affine claim.** `block_summary` / `prefix_scan` / `reconstruct_blocks`
  / `state_at_via_prefix` equal the authoritative machine for empty, all-zero,
  all-`0xFF`, 1-byte, and block-size-non-dividing corpora; `LinMap::compose`/
  `power` equal the direct bit-matrix action over the whole 16-bit domain and are
  associative on random triples; `power(a)∘power(b) == power(a+b)`.
* **`packed` exactness.** `PackedRestarts::snapshot_at` and `PackedHistory` equal
  `state_log` exactly for every position and stride `0..=63`; crafted/truncated
  bytes (including `logical_len = u64::MAX`) are clean `Err`s with no
  attacker-sized allocation; `state_from_hot` is exact exactly when covered. The
  `u16` assumption is a **type invariant** and cannot be violated by parsing.
* **`tiles` persistence.** Tampering the tile blob with the section CRC-32 **and**
  footer checksum recomputed is rejected (content-digest mismatch) at `open`/
  `FileBody`/`OpenObject`; `verify` re-derives against the machine. Boundary-crossing
  reads, empty/1-byte sets and shifts `1..=14` are byte-exact. `TwoLevelRadix`
  with empty middle groups is correct (both before and after the fix).
* **Budget cap.** 2 kinds x 6 allowances (incl. 0 and `u64::MAX`):
  `after_physical_bytes <= cap`, `after == before + added`, and a no-fit call is
  byte-for-byte inert with no generation bump.
* **`handle`.** An appended generation is re-validated (fresh `Arc`, newer
  generation); a held snapshot keeps reading its immutable bytes while appends and
  4 reader threads run (no torn read).
* **`planner`.** `read` is byte-exact to `reader::read` across tile/region/
  checkpoint boundaries, 0-length and whole-object requests; `all_certified()`
  holds; `path_for` is the flat `offset >> region_shift` index (no search).
* **`court`.** 0 reps, empty candidate lists, `working_set = 0`, and a working set
  smaller than the request all return without panic; empty reports serialise to
  valid JSON; `n = 1` collapses the bootstrap CI to the sample.

### Residual risk (Revision 3)

* A persisted `TileSet` is integrity-checked, not authenticated: a forger who
  recomputes the content digest and the digests linkage authors a different valid
  object (same non-authenticity model as Findings C/F2; only opt-in `AUTH`
  covers it). This includes the `planner`'s `TileSet` path and `read`'s
  `Certified` path-bloom label: both trust in-file verdicts, so the planner is a
  *performance* dispatch over certified structures, not an authenticity boundary.
* The `planner` does not re-derive a checkpoint's validity before selecting
  `PackedRestart`; on a hostile body a `restore_with` failure surfaces as `Err`
  rather than falling back to replay (reader parity holds for valid bodies, which
  is what the planner contract promises).
* `scripts/container-fuzz.sh` now lists the whole fuzz surface and runs **all
  six** targets, including `fuzz_packed` and `fuzz_tiles` (verified: 40,000 runs
  each, no crash artifacts).

## Unsafe review (`src/mapped.rs`) — verdict **SAFE**

**Boundary verdict:** an immutable-for-lifetime read-only map, published
rename-then-retain, never reachable from the untrusted parser, every view
bounds-checked, a single `munmap` on drop — **SAFE**.

First independent review of the crate's *third* `unsafe` module: a read-only
memory-mapped surface over an immutable output file. The module's **safe** public
API is `MappedSurface { open, open_with, read, read_into, len, is_empty,
physical_bytes, mode, advised, huge_page_report, sample_huge_pages }` and
`thp_anonymous_probe`. `unsafe` is confined to the raw `libc` calls and the two
slice/pointer views they justify.

### Exact `unsafe` inventory (grep)

`grep -rn unsafe src/` finds `unsafe` *code* in exactly three modules:
`src/simd.rs`, `src/async_io.rs`, and `src/mapped.rs`. In `src/mapped.rs` the
blocks are: one `mmap(PROT_READ, MAP_PRIVATE[|MAP_POPULATE])`, one `munmap` in
`Drop`, one `madvise` per applied advisory (`MADV_HUGEPAGE` /
`MADV_POPULATE_READ`), one `slice::from_raw_parts` in `read_into`, one volatile
`read` in the huge-page touch probe, one `mmap`/`madvise`/volatile-`write`/
`munmap` set in `thp_anonymous_probe`, one `sysconf`, and `unsafe impl Send/Sync`
for `MappedSurface`. Every block carries a `// SAFETY:` justification.
`tests/mmap.rs::unsafe_is_confined_to_three_sanctioned_modules` asserts the set
of modules containing `unsafe` code is exactly `{simd.rs, async_io.rs,
mapped.rs}`.

### SAFETY argument

* **`mmap`.** Called with `PROT_READ` and `MAP_PRIVATE`, a valid length `> 0`,
  and an fd open read-only for the call; the result is checked against
  `MAP_FAILED` before the base is stored. The mapping outlives the descriptor
  (the kernel pins the inode), so closing `file` cannot invalidate it.
* **`munmap`.** `base`/`map_len` are exactly the values from a successful `mmap`,
  stored privately, never mutated, and unmapped once in `Drop` (sole owner).
* **`from_raw_parts`.** Guarded by `offset.checked_add(len)? <= file_len <=
  map_len` (checked-add `Overflow`, else `OutOfBounds`) **before** the slice is
  formed; a zero-length read returns early. The mapping is never written through
  this type, so the bytes are readable for its lifetime, and the slice is copied
  into `out` immediately (no reference escapes).
* **`madvise`.** Only changes policy for the live `base`/`map_len` range; it
  never frees or moves the mapping, so a failure is recorded (`advised = false`),
  not fatal.
* **volatile touch.** `off < map_len`; used only to materialise huge pages.
* **`Send`/`Sync`.** Sound because the mapping is read-only and all output is
  copied into caller-owned memory.

### Adversarial attempts (executable)

* **map + truncate.** A file mapped then `set_len(0)` would make an unfaulted
  page `SIGBUS`. Defence: the module never opens the mapped file for writing,
  and the invariant forbids truncating/rewriting a mapped inode; publication is
  rename-then-retain. `missing_and_truncated_files_error_cleanly` pins that an
  empty/`set_len(0)` file is rejected at `open`.
* **map + replace.** Atomically renaming a new generation over the path does not
  affect a live mapping (the inode is retained).
  `old_generation_is_retained_while_a_mapping_is_alive` reads the old bytes after
  a replace **and** after an unlink; `concurrent_readers_are_never_torn…` runs 4
  readers across 64 published generations with no tear.
* **read past end.** `read(n,1)` / `read(n-1,2)` / `read(n+1,0)` return
  `OutOfBounds` with no output; `read(u64::MAX,2)` returns `Overflow`. No
  over-read, no panic.
* **untrusted inputs.** The module is not referenced by `reader`/`verify`/
  `recovery`, so it is never reached from the `.ekb` parser.

### Result

`sh scripts/container-verify.sh` is GREEN (fmt, clippy `-D warnings`, full
suite; `tests/mmap.rs` 7/7). `sh scripts/run-mmap.sh` runs unprivileged under
Docker's **default** seccomp profile (`--cap-drop=ALL
--security-opt=no-new-privileges`; no relaxation, no capability), writes
`evidence/bench/mmap.json`. **Verdict: SAFE — no memory-safety, lifetime, or
use-after-unmap defect found.**

## Revision 4 — `blocks`, `engine`, `region_adapt`, `machine_rans`, `checkpoint`/`packed` (verdict **TWO DEFECTS, BOTH FIXED**)

Independent adversarial pass over the newest round, executable in
`tests/adversarial_r4.rs` (12 tests) with a new fuzz target `fuzz_blocks`
(`fuzz/Cargo.toml` + the `scripts/container-fuzz.sh` target list). Method: mutate
a real body / writer output in memory, re-seal any linked certificate whose
`bloom_hash`/receipt must follow the mutation, and compare every result against
the authoritative machine or `reader::read`.

### Finding E (**Medium**, FIXED) — `verify` did not check the machine-2 successor cursor

| # | Severity | Location | Concrete input | Status |
|---|---|---|---|---|
| E | **Medium** | `src/verify.rs:check_regional` (+ `src/bloom_region.rs:RegionTable::from_shell`, which discarded `next_cursor`) | A `Certified` machine-2 `Regional` shell whose per-record successor cursor is altered, with its linked certificate re-sealed | **Fixed** |

Originally `check_regional` re-derived each region window's `(symbols, next_state)`
and compared it to the stored entries, but never compared the stored successor
`u64` cursor to the authoritative `cursors[i+D]`. For a cursor-divergent machine
(`OrANS-RANS0`) the cursor — not the state — is what makes the table resumable, and
`region_adapt::RegionReader::read` uses it verbatim on every leap. A self-consistent
forged body therefore **passed `verify_deep` (`ok == true`) while
`read_region_scoped` returned bytes that disagreed with `reader::read`** — a
verifier-completeness gap of the same class as Finding F3.

**Resolution.** `check_regional` now consumes the authoritative cursor log
(`region_adapt::cursor_log_with`, built in `run` only for a divergent machine) and
re-derives the per-key successor cursor, comparing it against the stored records
(`region_adapt::region_stored_cursors`); a mismatch — or a key whose windows
diverge on the cursor — fails the check and forces the recomputed status to
`Rejected`. A forged cursor now **fails** `verify`. Regression tests:
`verify::forged_regional_cursor_is_detected` (a flipped stored cursor fails
`verify` though `(symbols, next_state)` is untouched) and
`verify::machine1_regional_identity_cursor_verifies`; my end-to-end
`tests/adversarial_r4.rs::verify_rejects_a_wrong_machine2_region_cursor_used_by_read`
was flipped from the old gap reproducer to a rejection assertion. The builder was
already sound, so honest output was exact
(`machine2_region_successor_cursor_is_stored_and_authoritative`,
`machine2_region_reads_are_exact_across_boundaries_and_after_reopen`).

### Finding F (**Low**, FIXED) — `RegionView::covers` overflowed on a forged region

`src/region_adapt.rs::RegionView::covers` computed `self.start + self.len` with an
unchecked add, so a forged regional shell whose `len == shell.logical_len` is near
`u64::MAX` (accepted structurally by `open` and `RegionView::parse`, which check
only length *equality*) could overflow that add — a debug-build panic reachable
from `open_bytes` + `read_region_scoped`. Not memory-unsafe; not reachable through
the honest writer.

**Resolution.** `covers` now uses `self.start.checked_add(self.len)` and returns
`false` on overflow, so a position is never covered and the read cleanly falls back
to bounded restart. Regression test:
`region_adapt::tests::covers_rejects_overflowing_forged_region`.

### Attacks that held (Revision 4)

* **Region cursor (machine 2).** Stored successor cursors equal the authoritative
  `cursors[i+D]` for every window key; reads are byte-exact across every regional
  boundary, the final partial region, region-edge single bytes, and after
  serialize+reopen.
* **Machine-1 regional byte-identity.** No cursor bytes are stored
  (`table_len == entries*(D+6)`); reads are identical before and after reopen.
* **`blocks`.** `block_bytes` ∈ {0, 1, 3, 1<<30, (1<<30)+1}: `0` and `>1<<30` are
  rejected, the rest exact (a `1<<30` block is one restart); every truncation,
  bad magic/version/size/trailing byte → clean `Err`; a wrong state **or** cursor
  is rejected by `certify` **and** `verify_against`; `read_range` is byte-exact for
  both machines on boundary/partial/zero-length probes and after reopen; the step
  count is exactly `offset % B + len ≤ B-1+len`. A lying restart is caught by
  `verify`; the engine would serve it on in-file geometry alone (F2 boundary).
* **`checkpoint`/`packed`.** Legacy (12-byte) and compact (`EKBCKPT`) both parse;
  bad version/format/interval/zero-count/truncation/trailing → clean `Err`; no
  ambiguous parse (the magic-spelling legacy interval is simply invalid);
  `PackedHistory`/`PackedRestarts` reject `stride_shift > 63`, non-zero reserved
  bytes, truncation and trailing bytes.
* **`engine`.** `read` equals `reader::read` on both machines for aligned,
  unaligned, whole-object, zero-length, past-end and `u64::MAX` requests;
  only certified paths are selected; a body with no bounded structure is rejected
  (never an `O(prefix)` genesis replay); the transactional budget holds.
* **`machine_rans` prepared context.** `RansContext` is byte- and state-exact vs
  the per-symbol path across all categories; invalid headers (truncated, bad sum,
  zero frequency) are clean `Err`s; independent contexts for different cores do
  not interfere.

### Fuzz + evidence (Revision 4)

`sh scripts/container-fuzz.sh` now runs **all seven** targets clean (the existing
six plus `fuzz_blocks`, 20,000 runs, no crash). `sh scripts/container-reproduce.sh`
completed: clean `--locked` `--release` build, full suite **445 passed / 0 failed /
0 ignored**, and `sh scripts/validate-evidence.sh` reports **27** evidence JSON
files `OK`.

### Residual risk (Revision 4)

* Findings E and F are **RESOLVED** (see above); no open defect remains from this
  pass.
* Integrity is not authenticity, as throughout: a forger who recomputes the
  section CRC and the core linkage authors a different valid object; only opt-in
  `AUTH` is an authenticity boundary.

## Revision 5 — `machine_store`, `machine_rans_i`, `batch`, `ResidentView`, AUTH coverage (verdict **NO DEFECT FOUND**)

Independent, executable attacks over the newest L1–L4 surface: machines 3 and 4,
`batch::BatchReconstructor`, the borrowed `ResidentView`, the `auto` policy, and the
byte coverage of the opt-in `AUTH` MAC. Pinned by `tests/adversarial_r5.rs` (13
tests) and an extended non-mount case in `tests/fuse.rs`; every assertion is against
the authoritative machine, `reader::read`, or the keyed MAC — never against another
manifested structure.

### Machines 3 and 4 — **held**

* **Machine 3 (identity).** `encode == source` for every category (empty, one,
  all-256, repeating, skewed, random); `check_core`/`decode` exact; a mid-stream
  restart `(0, p)` reveals `src[p..]` for every `p ∈ [0, 8]`.
* **Machine 4 (interleaved rANS).** For every category and `K ∈ {1,2,4,8}`: a
  prepared `skip(p)` followed by a bulk decode equals `src[p..]` (skipping corrupts
  no lane), stepwise `reveal` equals the bulk decode, `lanes() == K`, and the decoded
  symbols equal machine 2's oracle for **all** `K ∈ [1,8]` (lanes are isolated).
  `reader::read` is byte-exact for machines 3 and 4 on
  `Root`/`Sparse`/`Path`/`MaxSpeed` at every boundary range.
* **Crafted/truncated headers.** `[]`, `[0u8;100]`, `[0u8;515]`; `K ∈ {0,9,u32::MAX}`;
  a lane table that does not fit; a zero frequency; a frequency sum `≠ M`; a lane
  state below `RANS_L`; a short renormalisation stream; 64 random 600-byte images →
  all clean `Err`, never a panic. The fused table's `slot >= cum` invariant keeps
  `(x>>12)*f + slot - c` inside `u32` even for an unvalidated state, so the step
  cannot overflow in a debug build either — the same argument that clears machine 2.
* **Structures.** Both machines drive `CheckpointSet::restore_with` (exact at every
  position), `BlockIndex` (exact/certified for `B ∈ {1,7,256,4096}`) and the
  `n+1`-entry state log.

### `ResidentView` — **held**

Byte-exact vs `reader::read` for **machines 1–4** over two surfaces (a
whole-object `HotTile` plan and a certified `Path`/`DirectStorage` plan) at
`{0, 1, 4095, 4096, 4097, 8191, n/2, n-2, n-1}` plus a 300-byte tile-crossing
window; `get_checked`/`copy_range` past the end are `None`/`false`; `get` panics only
as slice indexing does (documented). A mixed-path plan is rejected.

### AUTH coverage — **held; no uncovered section changes a read**

`AUTH` is the final section and its MAC covers `[0, auth.header_offset)` — verified
by the committed `tests/auth_coverage.rs` to contain the header, core, alignment
padding and every other section of a rich body (history/segmented, packed
checkpoints, every bloom kind incl. regional, tiles, blocks, manifest). Tampering
any (with its CRC recomputed) is rejected; unkeyed and wrong-key files are rejected
in strict mode; honest files pass.

**Outside the literal MAC range**, the R5 attacks found only fail-closed regions:

* A re-sealed **footer** tamper (generation) breaks the manifest cross-check, so the
  commit does not parse and strict open rejects it.
* Arbitrary **trailing** bytes after the last footer are dropped as a torn tail and
  cannot change a read.
* A **tampered AUTH tag** (its own payload), even with the section CRC re-sealed, is
  rejected by the keyed MAC.
* A whole **appended generation** — unkeyed, or `AUTH`-carrying under the wrong key,
  both structurally valid so `scan` accepts them — is *skipped* by
  `open_bytes_authenticated`, which selects the honest authenticated generation; the
  returned bytes are the honest bytes. (The unkeyed view does see the append — the
  exact boundary opt-in AUTH closes.)

### `select_machine`/`auto` and `batch` — **held**

`auto` picks machine 3 for incompressible data (`V == N <= raw`) and machine 2 for
compressible data (`V < N`); explicit machine ids are exact. `batch` is byte-exact
vs the authoritative machine for machines 1/3/4 at thread counts
`{0,1,2,3,5,7,8,16,64}`, for empty/single/large/boundary batches, preserves order,
and `accelerator_available() == false` (no GPU path is selected).

### FUSE `ReadMode::Auto` — **held**

`OrFs::aligned_surface` exposes a surface only for a fully materialised object; a
`Root` object has none, so reads fall back to `OrFs::read`. Both the decode path and
the surface path are byte-exact across region-boundary-crossing windows. The
surface-vs-decode unit test and the new non-mount `tests/fuse.rs` case run in the
ordinary gate; the mount tests also pass here.

### Fuzz + evidence (Revision 5)

`sh scripts/container-fuzz.sh` now runs **all nine** targets clean — the seven
existing plus `fuzz_batch` (34,466 runs / 61 s, no crash) and `fuzz_rans_i`
(100,000 runs / 48 s, no crash); the fuzz build is warning-free.
`sh scripts/container-reproduce.sh` completed: clean `--locked --release` build,
**523 passed / 0 failed / 0 ignored**, then `sh scripts/validate-evidence.sh` →
**28** evidence JSON files `OK`. The formatting gate is green (`cargo fmt --check`,
`clippy -D warnings`, `cargo test --all-features`: **535 passed**, incl. 13 R5 tests
and 3 FUSE tests).

### Residual risk (Revision 5)

* **No defect found this pass**; no regression test was needed beyond the attacks.
* The commit **footer** is bound to the MAC-covered manifest by cross-checks rather
  than by direct MAC coverage; that reasoning depends on `parse_commit`'s checks
  remaining complete (they reject any disagreement).
* Strict authenticated open **prefers the newest authenticating generation**, so an
  appended unauthenticated generation is a rollback/DoS, never an integrity break.
* `ReadMode::Auto` serves the `.ekb` path-bloom region by `pread`; a caller who
  mutates that file after mount steps outside the mount's snapshot assumption.
* Integrity is still not authenticity: only opt-in `AUTH` is the authenticity
  boundary.

## Revision 6 — M1–M5: machine-4 complete state, exact accounting, committed budgets, request-level bound, machine-4 engine + `create_auto` reuse (verdict **NO DEFECT FOUND**)

Independent, executable attacks over the newest surface, pinned by
`tests/adversarial_m.rs` (13 tests, all green). Every byte assertion is against the
**authoritative machine**, `reader::read`, or the exact serialized form — never
against another manifested structure.

### M1 — machine-4 complete successor state — **held**

A machine-4 body with regional/block/checkpoint structures is driven through every
accelerated path and read **well beyond** the single lookup (so the reconstructed
successor is consumed): byte-exact vs `reader::read` and vs the source, before and
after serialize + reopen. The stored wide snapshot is proven **not** a
`(state,cursor)` projection (`Snapshot::new` drops lanes and decodes different
bytes), and the region reader uses the stored lanes. `grep Snapshot::new` finds no
site that reconstructs a machine-4 successor from `(state,cursor)` alone. A
*self-consistent but forged* wide record (Phi_D projection intact) — tampered
cursor, lane state, or active-lane index — is rejected by `verify`'s re-derived
successor comparison. Machines 1/2/3 keep their compact forms (2-byte packed /
12-byte full / no cursor vs `u64` cursor) under the `snapshot_needs_lanes`
predicate.

### M2 — exact physical accounting — **held**

For machines 1–4, `BlockIndex::physical_bytes() == to_bytes().len()` (directory +
restart records, the shared rANS model header live once in `core`, never
re-counted); `History`/`CheckpointSet`/`BloomShell` obey their documented
fixed-header relations; the machine-4 wide regional shell obeys the shell-header
relation and is counted once by `EkbloomBody::physical_bytes()`/`inspect`.

### M2 — committed budget — **held**

`Engine::plan` rejects a body whose **committed** footprint exceeds the budget even
when the active plan fits, and rejects an over-budget `n == 0` core. The hard-cap
build entry points (`create_*_within_budget`) never admit an over-budget machine-4
body: the post-build `physical_bytes()` re-check closes the gap left by the
machine-1 static estimate, which (documented) under-counts a wide checkpoint.

### M3 — request-level bounded work — **held**

On a machine-4 body with many cold→hot→cold segments, a whole-object read's actual
positioning never exceeds `worst_case_steps_for(req)` (checked via
`read_counted_steps`), and is `O(segments)`, never `O(prefix)`.

### M5 — machine-4 engine + `create_auto` reuse — **held**

The engine reads machine-4 bodies byte-exactly for every request shape over all
four accelerated paths; the prepared interleaved context and the unprepared
per-read path are byte-identical. `create_auto`/`create_auto_within_budget` are
byte-identical to the explicit select-then-create path across machines and
profiles, and the body's core is the encode of the source (no stale core).

### M4 — pinned-hot baseline — **held**

The pinned footprint counts the actual pinned decoded bytes, the pin set is
permanent and bounded, and cold lookups never displace a pin.

### Fuzz + evidence (Revision 6)

`sh scripts/container-fuzz.sh` runs **all ten** targets clean (added `fuzz_region`,
44 097 runs / 61 s; extended `fuzz_blocks` to machines 3/4, 45 930 runs / 61 s;
the eight others unchanged, all PASS, no crash). `sh scripts/container-reproduce.sh`
completed: clean `--locked --release` build, **566 passed / 0 failed**, then
`sh scripts/validate-evidence.sh` → **28** evidence JSON files `OK`. The verify gate
(`cargo fmt --check`, `clippy -D warnings`, `cargo test --all-features`) is green at
**578 passed** (incl. the 13 R6 tests).

### Residual risk (Revision 6)

* **No defect found this pass.**
* The static `estimate_physical_bytes`/`estimate_physical_bytes_for_core` is the
  machine-1 view: it under-counts a machine-2/4 wide checkpoint and over-counts a
  regionally-packed profile. The **hard cap** is still enforced (post-build
  re-check), but `select_profile_within_budget` filters on the estimate alone and
  can therefore reject a body that would have fit (documented trade-off).
* `pinned_block_count(hot_bytes = 0, block, total)` returns `1`, not `0`
  (documented "at least one"); a zero hot budget therefore pins one block.
* `Engine`/`reader` trust a body only as far as `verify` does: a tampered but
  unverified history/bloom still drives reads. `verify`/`verify_deep` is the gate.
* A `HistorySegmented` section is validated and accounted by the file reader but is
  not represented in the in-memory `EkbloomBody`, so `EkbloomBody::physical_bytes()`
  omits it (out of scope of M1–M5; unchanged by this round).

## Revision 7 — N1–N7: core-identity binding, bounded windows, machines 5/6, OrFS pointer views, budget-aware optimiser (verdict **TWO DEFECTS, BOTH FIXED**)

Independent, executable attacks on the N-round surface, pinned by
`tests/adversarial_r7.rs` (11 tests, all green) plus the two new fuzz targets
`fuzz_pack`/`fuzz_seg`. Every byte assertion is against the authoritative machine,
`reader::read`, or an independent walk; every crafted container must be a clean
typed `Err`, never a panic and never a wrong byte.

### Finding G (**High**, FIXED) — `BlockIndex::read_range_counted` doubled the output for machines 5/6

`BlockIndex::read_range_counted` pre-sized `out = vec![0u8; len]` and then, in its
`_` fallback arm, called `Prepared::decode_into`, which **appends** rather than
fills. Machines 1/2/3/4 have tight arms that fill the buffer; the newly-added
cursor==position machines 5 (`OrANS-PACK`) and 6 (`OrANS-SEG`) fell into `_`, so a
block-index read returned `2 * len` bytes whose first `len` were zeros. Because
`Engine::read` returns the dispatch buffer verbatim, a machine-5/6 body with a
block index returned a zero-prefixed buffer of the wrong length (an integrity/
length violation, and a data-duplication vector).

**Fix (`src/blocks.rs`, one line):** `out.clear();` before the `_`-arm
`Prepared::decode_into`, so the append yields exactly `len` bytes. Failing-then-
passing test: `block_index_reads_are_length_and_byte_exact_for_machines_five_and_six`.

### Finding H (**Medium**, FIXED) — unchecked `index + 1` overflow panic in machines 5/6

`PackContext::byte_at` (`src/machine_pack.rs`) and `SegContext::locate`
(`src/machine_seg.rs`) built the `CoreTooShort` diagnostic with `index + 1` on an
attacker-controlled public `u64` index. `byte_at(core, u64::MAX)` (a natural
one-past-the-end probe) panicked with `attempt to add with overflow` in any
overflow-checked build — a remote OOM/DoS-adjacent crash, not a typed error. The
`fuzz_pack` campaign found it (`crash-b6cab43d…`, input `[7,1,0,7,1,6f×6]`).

**Fix (two one-line `saturating_add(1)` changes):** both sites. Failing-then-
passing test: `positional_reads_past_the_end_are_typed_errors_not_overflow_panics`.

### Attacks that held (Revision 7)

* **N1 plan identity** — for machines 5 and 6, planning on `A` and reading `B` (a
different object, whose core hash differs) and a newer *generation* of the same
object are both rejected with `EngineBodyMismatch` before any decode;
`ReadyObject` binds body+engine structurally. Same body+generation is byte-exact.
* **N2 bounded window** — the encoded-window bound holds at block boundaries
(`fetched ≤ 2·(steps+len)+64` via the `fetched_bytes` counter); truncation and a
cursor at the core end are typed `Err`; machines 1/3 are on the unaffected
whole-core/rebased paths (`tests/window.rs`).
* **N3 machine 5** — extraction is exact at every position for every width 1–8
(including non-power-of-two and partial words), constant (k=1) and raw/identity
fallbacks are exact; `bits>8`, `k>2^bits`, duplicate alphabet, truncation and
trailing bytes, and an out-of-alphabet payload index are all typed `Err`.
* **N4 machine 6** — aliased/overlapping/out-of-range physical ranges, a logical
layout that over-spans a neighbour, wrong version/magic, truncation, a content-id
tamper and a directory-hash mismatch are all typed `Err`; a segment with an
unknown machine id is rejected (no silent reinterpretation of machine 2/4 cores);
fixed and variable directories are exact at every boundary.
* **N5 OrFS pointer views** — `inspect_physical` equals an independent `du`-style
walk; the authoritative core occurs in exactly one concealed file (sidecars <512
bytes, pointer views only); `read_machine` reconstructs the machine for a `Path`
object that *does* carry a hot surface, byte-exact vs the source and the dispatch
path; FUSE `ReadMode::Machine` attaches no surface.
* **N6 optimiser** — chosen plans are built then re-checked against the hard cap
(`committed ≤ B`); only `all_certified` plans are evaluated; the recorded
`held_out_fingerprint` is `trace_fingerprint(trace)`, the one trace passed in
(held-out discipline is by contract: the evaluation trace is never passed); the
result is deterministic.

### Fuzz + evidence (Revision 7)

`sh scripts/container-fuzz.sh` runs **thirteen** targets clean (added `fuzz_pack`
and `fuzz_seg` for machines 5/6, plus the N8 `fuzz_resource`; wired into
`fuzz/Cargo.toml` and the campaign script). `sh scripts/container-reproduce.sh`
completed: clean `--locked --release` build, **649 passed / 0 failed**, then
`sh scripts/validate-evidence.sh` → **32** evidence JSON files `OK`. The verify
gate (`cargo fmt --check`, `clippy -D warnings`, `cargo test --all-features`) is
green at **663 passed**.

### Residual risk (Revision 7)

* **Two defects fixed** (Findings G and H); no unfixed read-path defect found.
* **Whole-object APIs trust a core-declared length — ADDRESSED (N8).** The
  unbounded declared-length allocation is now bounded by the [`ResourcePolicy`]
  (default 1 GiB, `DEFAULT_MAX_SINGLE_ALLOCATION`): every whole-object site calls
  `checked_default`/`checked_capacity` immediately before sizing a `Vec` and
  refuses an over-cap request with `AllocationTooLarge` before reserving. Guarded
  sites: `OrMachine::decode`/`state_log`, `Prepared::decode_into`,
  `block_restarts_with` (`machine.rs`); `RansContext`/`RansIContext::decode_into`
  and `Rans0`/`Rans0I::decode`/`state_log` (`machine_rans*.rs`);
  `PackContext::unpack_into` (`machine_pack.rs`); `SegContext::from_core`/
  `decode_into` (`machine_seg.rs`); `History::from_bytes`/`extend_with`
  (`history.rs`); `CheckpointSet::from_bytes_with` (`checkpoint.rs`); the
  `PathBloom`/region build vectors (`bloom_path.rs`, `bloom_region.rs`). The
  bounded read/window paths are unaffected (they never allocate a declared
  whole-object length). Pinned by `tests/resource.rs` and `fuzz_resource`.
  This was the integrity-not-authenticity boundary applied to length; the residual
  now is only that a caller must opt into a policy for lengths it deems legitimate.
* **Forged restart cursor with an in-range state is not independently re-derived at
  read time.** A restart record whose `state` is legal but whose `cursor` is
  inconsistent would decode wrong bytes; the file path binds it via the section CRC
  + `core_hash` linkage (integrity, not authenticity), and `verify`/`certify` is the
  gate for in-memory bodies. Unchanged trust boundary.
* A public `OrMachine::reveal`/`peek` with a *crafted* `Snapshot.cursor == u64::MAX`
  still overflows `p + 1` in the single-state machines (`NL2`, `STORE`); not reached
  by any read path and pre-existing (not N-introduced). Noted, not fixed.

## Revision 8 — O1–O2: machines 7/8, the SIMD kernel, the footprint gate, OrFS framing (verdict **TWO DEFECTS, BOTH FIXED**)

Independent, executable attacks pinned by `tests/adversarial_o.rs` (9 tests, green)
plus the two new fuzz targets `fuzz_rans_w`/`fuzz_rans_il`. Machines 1–6 stay
wire-hash-pinned; every crafted container is a clean typed `Err`.

### Finding I (**Medium**, FIXED) — unchecked `cursor + 1` overflow in machines 7/8 and the SIMD kernel

`CoreTooShort { needed: … }` was built with a non-saturating `cursor + 1` (and `+ 2`
in machine 7's `read_word`) on an **attacker-controlled cursor**: machine 7
`read_word` (`machine_rans_w.rs`), machine 8 `IlContext::step`/`decode_into_slice`
(`machine_rans_il.rs`), and `rans_decode8_independent_scalar`/`_avx2`/
`rans_decode8_shared` (`simd.rs`). A cursor of `u64::MAX` panicked with `attempt to
add with overflow` in any overflow-checked build.

**Reachable from a read path for machine 7.** Machine 7 is cursor-divergent but
`Prepared::is_windowed()` is `false` for it, so it is **not** on the cursor-bounded
windowed path; `read_via_checkpoint` loads the checkpoint entry's cursor verbatim
(`Snapshot::unpack`) with no `validate_restart` and no `cursor <= core_len` bound
(only machine 1's implicit cursor is enforced in `from_bytes_with`). A crafted
`.ekb` whose machine-7 checkpoint carries `cursor = u64::MAX` (CRC recomputed —
integrity, not authenticity) therefore reaches `read_word(u64::MAX)`. Machine 8 and
the kernel are reached through the public `reveal`/`peek`/kernel API with a crafted
`Snapshot`/`IlState`/cursor array (the same boundary noted for `NL2`/`STORE` in R7,
but **new code** here).

**Fix (one-line `saturating_add` per site, mirroring Finding H):** `machine_rans_w.rs`
(`p + 1`, `p + 2`), `machine_rans_il.rs` (three sites), `simd.rs` (five sites).
Failing-then-passing: `machine7_bogus_checkpoint_cursor_is_a_typed_error`,
`machine7_reveal_with_max_cursor_is_a_typed_error`,
`machine8_reveal_with_max_lane_cursor_is_a_typed_error`,
`simd_kernel_max_cursor_is_a_typed_error` (all panicked before the fix).

### Finding J (**Medium**, FIXED) — the footprint gate aborted when only the base representation fit

On uniform/incompressible data the documented contract is *machine 3, no
structures, `CommittedBytes(plan) <= raw`* (the footprint floor). But `optimize`
built the cold geometries (`Checkpoint`/`Block` only — no `None`) **first** and
returned `AllocationTooLarge { requested: raw, limit: raw }` when none fit the
budget, *before* Phase 1b computed the structureless base representation. So a
budget of exactly `raw` (or `raw` plus less than the smallest structure overhead)
on uniform data — the one case the gate exists for — **errored instead of returning
the ≤-raw base plan**. Minimal repro: uniform 64 KiB, `budget = raw` →
`AllocationTooLarge { requested: 65536, limit: 65536 }`.

**Fix (`src/optimize.rs`):** compute the base representation (Phase 1b) **before**
the cold-geometry guard, and gate the two emptiness checks on the base reference:
`if bases.is_empty() && base_reference.is_none()` and
`if evaluated.is_empty() && base_reference.is_none()`. The base candidate is then
seeded into the evaluated set by the existing Phase 3a gate and chosen.
Failing-then-passing: `optimizer_uniform_tight_budget_stays_leq_raw` (failed before:
`AllocationTooLarge`). Behavior for budgets where a cold geometry fits is unchanged.

### Attacks that held (Revision 8)

* **Machines 7/8 exactness** — all categories × lane counts 1–8 round-trip; byte-
exact vs `reader::read` over full/partial/boundary ranges before and after
serialize + reopen; complete-state machine-7 checkpoints/blocks and machine-8
`il_block_restarts` resume exactly at every block boundary; crafted/truncated
headers (short fixed header, `K∉[1,8]`, overrunning/short lane payload) are clean
`Err`; machines 1–6 registry + FNV wire hashes unchanged.
* **SIMD kernel** — the unwired decoder kernel equals the scalar reference for
adversarial cursors/boundaries and is now total; it is exported but called by no
other `src` module (unwired, not on any default path); `unsafe` remains confined to
the three sanctioned modules (`simd`, `async_io`, `mapped`).
* **OrFS** — `inspect_physical` still equals an independent `du`-style walk, the
authoritative payload is stored exactly once (sidecars < 512 B), and the compacted
views/framing round-trip, recover and reject corruption (existing tests, green).
Views are **write-only** (no reader consumes them), so there is no forged-view
surface to accept.
* **Optimiser** — with the fix, budget (committed ≤ `B`), certification
(`all_certified` only) and the footprint invariant (uniform plan ≤ raw) hold at both
tight and loose budgets; a compressible/constant corpus does not expand past raw.

### Fuzz + evidence (Revision 8)

`sh scripts/container-fuzz.sh`: **fifteen** targets PASS (`fuzz_rans_w`,
`fuzz_rans_il` added and wired into `fuzz/Cargo.toml` + the campaign).
`sh scripts/container-reproduce.sh`: clean `--locked --release` build, **687
passed / 0 failed**, **33** evidence JSON files OK. Verify gate (`fmt --check`,
`clippy -D warnings`, `cargo test --all-features`) green at **701 passed**.

### Residual risk (Revision 8)

* **Both defects fixed** (I, J); no unfixed read-path defect found.
* The pre-existing **`NL2`/`STORE` `reveal`/`peek` `cursor + 1`** overflow (R7) is
  the same class as Finding I but was left unchanged (public-API-only, not on a read
  path). Recommend the same `saturating_add` for consistency.
* Release builds (`overflow-checks` off) already turn the Finding-I overflow into a
  clean `Err`; the DoS was limited to overflow-checked builds. A restart cursor that
  is in range but state-inconsistent is still not re-derived at read time (integrity,
  not authenticity) — unchanged.

## Revision 9 — P1–P8 (verdict **NO LIBRARY DEFECT FOUND; one fuzz-coverage gap fixed, two doc defects reported**)

Independent, executable attacks in `tests/adversarial_p.rs` (**7 tests**, green;
written against the public API, not the round's own helpers). Verified:
`container-verify.sh` (safety gate + `fmt --check` + `clippy -D warnings` +
all-features tests **779 passed / 0 failed**), `run-gates.sh` **G1–G5 all PASS**,
`container-fuzz.sh` **17/17 targets PASS**, `container-reproduce.sh` clean
`--locked --release` build + **46** evidence JSON OK; release suite **765 passed /
0 failed**.

### Finding K (**Low**, fuzz coverage, FIXED) — `fuzz_roundtrip` bypassed the new machine-6 decode

`fuzz/fuzz_targets/fuzz_roundtrip.rs` still carried the pre-fix workaround that
skipped machine 6 and called `SegContext::decode_into` directly, because the trait
`decode` was `O(N^2)`. After the SEG-linear commit (`Seg::decode`/`state_log`
overrides) that workaround silently stopped fuzzing the new code. Removed it: the
target now calls `machine.decode(core, n)` for every machine 1–9, so the linear
override is fuzzed through the public trait. (fuzz-only edit; campaign re-run green.)

### Attacks that held

* **P1 versioning** — write→read→verify is byte-exact for machines **1–9** (the
  shipped `format_version.rs` covers only 1–8); a future `format_minor` and a future
  per-machine `core_version` are typed rejects for **every** machine incl. the
  shared-dictionary machine 9 (`UnsupportedVersion` / `UnsupportedCoreVersion`); a
  legacy (`core_version = 0`) core still reads exactly. `supported_core_versions`
  includes the DICT id.
* **P2 streaming** — `read_at`/`read_sequential` are byte-exact at adversarial
  offsets through a **variable** (mid-stream `checkpoint`) layout, every block
  boundary, the final partial block and the empty-at-end case; tampered payload
  (content-id), tampered directory (BLAKE3) and truncation are typed `Err`. The
  builder holds one block plus the documented O(blocks) directory.
* **P5 dictionary** — absent/unregistered id, tampered embedded table, relabelled
  frame, out-of-range state and length mismatch are all typed `Err`; embedded and
  referenced cores are byte-exact; the shipped dictionary tests are green.
* **P3 corruption** — an independent single-bit sweep (core + manifest payload +
  the whole 64-byte footer; **>2000** flips) detects **every** flip;
  padding/inert metadata remain uninterpreted.
* **P4 unsafe/alloc gate** — `check-safety.sh` confines `unsafe` to
  `{simd,async_io,mapped}.rs` and fails closed on a new module; the production
  allowlist is exactly the four audited `.expect(` sites (0 `.unwrap()`, 0
  panic-family) — minimal.
* **SEG linearity** — `Seg::decode`/`state_log` equal the *unmodified* per-symbol
  `reveal` reference byte-for-byte (n = 1…65537, incl. non-power-of-two), reject a
  declared-length mismatch, and are used by `verify`, `reader` and the stream
  `Segmented` policy.

### Residual risk (Revision 9)

* **`read_sequential` reserves before the policy guard.** It does
  `Vec::with_capacity(block logical_len)` (attacker-controlled up to `block_bytes`,
  `u32`) *before* the per-machine `checked_default(n)` inside `decode_into`. The
  semantic decode still returns `AllocationTooLarge`, but the reservation itself is
  not policy-bounded: a crafted container can transiently request up to ≈4 GiB of
  address space. Recommend a `checked_default(e.logical_len)?` before the
  reservation.
* **Dictionary id is not cryptographically bound in-core.** Relabelling a
  referenced core to a *different registered* id is caught by the manifest
  `core_hash` linkage in a manifested body, but the bare machine API would decode
  against the wrong model — the documented **integrity ≠ authenticity** boundary;
  only `AUTH` closes it.
* **Documentation drift (reported, not fixed — outside review scope).**
  `docs/MIGRATION.md` still says machines 1–8 and its worked example allocates
  `MACHINE_NL2P = 9`, an id now taken by `OrANS-DICT` (P5); `docs/CORE-LAYOUT.md`
  (intro, §1.3) likewise says "machines 1–8". Correct to include machine 9 and
  re-point the example at an unused id.
 
