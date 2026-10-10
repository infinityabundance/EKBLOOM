# EKBLOOM `.ekb` / wire-form security review

Scope: untrusted-input handling on the `.ekb` read path — `format`, `reader`,
`recovery`, `history`, `checkpoint`, all bloom `from_bytes`/`from_shell`,
`filesystem` index parsing, and publication races. Adversarial method:
read the parsers, then craft concrete byte images and feed them to
`open_bytes`/`from_bytes`/`verify`, asserting a clean `Err` (never panic, never
OOM). No `unsafe` exists in the crate (verified by grep; only prose mentions in
`asyncpub.rs`). All tooling ran via `scripts/container-verify.sh` (8 GiB cap).

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

## Residual risk: corruption detection is not authenticity
`.ekb` uses **unkeyed** crc32/BLAKE3 (and CRC-32 for section/footer linkage). These
detect accidental corruption only. A malicious forger with write access can
rewrite `core` **and** every structure consistently; the result is merely a
*different internally valid object*, not a detection. In particular:

- `CheckpointSet` carries **no** `core_hash` field, so a forged checkpoint state
  (cursor left legal) is caught only by replay — `verify` / `verify_deep` — never
  at `open`. The same is true of forged `hot` symbols inside an otherwise
  `Certified` path bloom.
- Therefore `open` is a *structural* validator, not an authenticity boundary.

Adversarial authenticity requires a **keyed MAC** over the committed bytes
(e.g. keyed BLAKE3/HMAC) with a key the forger lacks — explicitly out of scope
for this review and noted as future work.

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
- No `unsafe`, no `unwrap`/panic reachable from parsed bytes found by inspection
  or mutation (`mutated_and_truncated_images_never_panic`).
- Not exhaustively fuzzed here; a dedicated campaign (`fuzz/fuzz_targets`) plus
  Miri/sanitizers would be stronger evidence. `memmap2` is a declared dependency
  but unused in `src` (no mmap of untrusted offsets). Finding C is now mitigated
  for `core_hash` linkage at open; state-forgery authenticity (checkpoints, path
  `hot`) is not a memory-safety issue and needs a keyed MAC — see the residual
  risk section above.
