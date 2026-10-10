# EKBLOOM `.ekb` / wire-form security review

Scope: untrusted-input handling on the `.ekb` read path — `format`, `reader`,
`recovery`, `history`, `checkpoint`, all bloom `from_bytes`/`from_shell`,
`filesystem` index parsing, and publication races. Adversarial method:
read the parsers, then craft concrete byte images and feed them to
`open_bytes`/`from_bytes`/`verify`, asserting a clean `Err` (never panic, never
OOM). The crate contains exactly **one** `unsafe` block, in `src/simd.rs`
(feature-gated AVX2 gather); it is independently reviewed in the
"Unsafe review (src/simd.rs)" section below. All other `unsafe` mentions in the
tree are prose. All tooling ran via `scripts/container-verify.sh` (8 GiB cap).

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

The crate's only `unsafe`: `gather_bytes_avx2` (`unsafe fn`, one block) plus its
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
across `src/` matches **only** `src/simd.rs` (the block and call site). Zero
other `unsafe`.

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
  mutation (`mutated_and_truncated_images_never_panic`); the one `unsafe`
  (`src/simd.rs`) is reviewed above and is not on the read path.
- Not exhaustively fuzzed here; a dedicated campaign (`fuzz/fuzz_targets`) plus
  Miri/sanitizers would be stronger evidence. `memmap2` is a declared dependency
  but unused in `src` (no mmap of untrusted offsets). Finding C is now mitigated
  for `core_hash` linkage at open; state-forgery authenticity (checkpoints, path
  `hot`) is not a memory-safety issue and needs a keyed MAC — now provided
  **opt-in** (see the residual risk section above and `docs/SECURITY.md`).
