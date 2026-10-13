# EKBLOOM corruption model — detection guarantees

**Modules:** [`src/format.rs`](../src/format.rs) (structural scan),
[`src/verify.rs`](../src/verify.rs) (deep verifier + located errors),
[`src/reader.rs`](../src/reader.rs) (read path), [`src/error.rs`](../src/error.rs)
(typed errors) · **Court:** [`examples/corruption_court.rs`](../examples/corruption_court.rs)
· **Evidence:** [`evidence/bench/corruption.json`](../evidence/bench/corruption.json)
· **Tests:** [`tests/corruption.rs`](../tests/corruption.rs) ·
**Reproduce:** `sh scripts/run-corruption.sh`

This document states **what a single bit flip in each `.ekb` structure does**:
whether it is detected, at what granularity, and with which typed error. It is the
companion to [`SECURITY.md`](SECURITY.md): unkeyed files give **corruption
detection**; only `AUTH` gives **authenticity** (integrity ≠ authenticity).

Everything below is measured by the corruption court: a rich body is built from a
deterministic corpus and, for **each** structure, **1024 random single-bit flips**
are applied and the detection behaviour recorded. No number is estimated.

---

## 1. The two detection channels

| channel | input | mechanism | scope |
|---|---|---|---|
| **fast path** | the serialized `.ekb` bytes | [`reader::open_bytes`](../src/reader.rs) → `format::scan` | per-section frame CRC-32, core BLAKE3 `core_hash`, footer CRC-32 |
| **deep verifier** | the parsed [`EkbloomBody`](../src/manifest.rs) | [`verify`](../src/verify.rs) / `verify_deep` / `verify_deep_located` | every structure re-derived from the authoritative machine |

A single bit flip in a structure's **serialized payload** is caught by the fast
path: every section payload is covered by its 40-byte frame's `checksum` (CRC-32,
IEEE, `crc32fast`), the core is covered by the manifest's BLAKE3 `core_hash` (and
the footer's linkage to the manifest), and the footer is covered by its own CRC-32
over `[0,56) ++ [60,64)`. The deep verifier independently catches a flip in the
bytes it actually *reads*, and reports the logical offset of the divergence.

## 2. Per-structure coverage

`crc32` = per-section frame checksum; `blake3` = a BLAKE3 content digest;
`core_hash` = BLAKE3 over the visible core, recorded in the manifest.

| structure | covers it | detection granularity | deep offset semantics |
|---|---|---|---|
| core (`V`) | `core_hash` (manifest) + footer linkage | the whole core | `0` (core is not position-addressable) |
| history (`L`) | section frame `crc32` | the section payload | first divergent logical symbol |
| checkpoint | section frame `crc32` | the section payload | the failing restart ordinal `j·K` |
| transition bloom `T_D` (universal) | section frame `crc32` (+ deep: presence mask + present slots) | the section payload | `0` |
| transition bloom `T_D` (contextual) | section frame `crc32` | the section payload | `0` |
| transition bloom `T_D` (regional) | section frame `crc32` | the section payload | `0` |
| path bloom `P_32` | section frame `crc32` | the section payload | first divergent symbol |
| tiles (hot tiles) | section frame `crc32` + BLAKE3 content `digest` | the section payload | `0` (rejected at parse) |
| tile directory | section frame `crc32` + content `digest` | the section payload | `0` |
| blocks | section frame `crc32` + BLAKE3 content `digest` | the section payload | first divergent restart `j·B` |
| block directory | section frame `crc32` + content `digest` | the section payload | `0` |
| manifest | section frame `crc32` + footer BLAKE3 `manifest_hash` | the section payload | `0` |
| commit footer | footer `crc32` over `[0,56) ++ [60,64)` | the 64-byte footer | `0` |
| section directory | section frame `crc32` (`kind`/`offset`/`length`/`checksum`) | the 40-byte header | `0` |
| AUTH (opt-in) | keyed BLAKE3 MAC over `file[0..auth_header_offset)` | the whole covered prefix | `0` |

**Inert bytes (deliberately not covered, never interpreted):** alignment padding
between sections, and the section-header fields `flags`, `reserved` and
`generation`, and a regional table's `table_offset`. Flipping one of these is a
**no-op**: the body still opens, still verifies, and reads byte-for-byte
identically. They carry no information the system acts on, so `open`/`verify`
neither reject nor reinterpret them. They are still inside the `AUTH` covered
range, so a keyed read *does* catch them (see §5).

## 3. Typed errors and offset semantics

- `EkbloomError::ChecksumMismatch { section }` — a section/frame CRC-32 (or the
  footer CRC-32, or the manifest `manifest_hash`) mismatched. `section` is the
  stable [`SectionKind::name`](../src/format.rs) (`"history"`, `"checkpoints"`,
  `"bloom-global"`, `"tiles"`, `"manifest"`, `"commit-footer"`, …).
- `EkbloomError::CorruptGeneration { generation, reason }` — a generation failed
  *semantic* validation at open (e.g. `core_hash` mismatch, a structure disagreeing
  with the manifest, the component count disagreeing).
- **`EkbloomError::Corrupt { section: &'static str, offset: u64 }`** *(added for
  this work)* — the deep verifier's precise verdict: the containing
  section/frame and the **logical offset** of the first divergence. Produced by
  [`verify::verify_deep_located`](../src/verify.rs); `offset` is a symbol position
  for history / checkpoint / path / block checks and `0` where the check is not
  position-specific.
- [`verify::section_span_at(bytes, offset)`](../src/verify.rs) maps an absolute
  file offset back to its containing section and payload span — the *reporting*
  half of the model (the court uses it to confirm granularity).

A caller therefore gets: **detected?** yes (an `Err`), **which section?** the
typed error's `section`, and **where?** `offset` — the containing section/frame
(CRC granularity) or, for the deep verifier, the exact divergent logical offset.

**Granularity.** A detected flip is attributed to the containing section/frame
(the unit its CRC-32 covers). Because the court also records the exact flipped
byte, the reported location is the `(section, exact byte)` pair — **localized to
a ≤ 4096-byte window for every covered structure** (the `reported_window_bytes`
field in `corruption.json` is ≤ 4096 for all of them; `tile_directory`,
`block_directory`, the section-header frames and the footer are far smaller). The
deep verifier narrows further to the exact logical symbol offset.

## 4. Measured results (`evidence/bench/corruption.json`)

Corpus: seed `0xEB100000C0DE2026`, `logical_len = 4096`, machine 1, core 4096 B,
**584152 B** file, 18 sections, **1024 flips per structure**.

**Fast path** (serialized bit flip → `open`) — every covered structure detects
**1024/1024 = 1.0000**:

| structure | detected | dominant error |
|---|---:|---|
| core | 1024/1024 | `CorruptGeneration(core hash mismatch)` |
| history | 1024/1024 | `ChecksumMismatch(history)` |
| checkpoint | 1024/1024 | `ChecksumMismatch(checkpoints)` |
| bloom-global / contextual / regional / path | 1024/1024 each | `ChecksumMismatch(bloom-*)` |
| tiles / tile_directory | 1024/1024 | `ChecksumMismatch(tiles)` |
| blocks / block_directory | 1024/1024 | `ChecksumMismatch(blocks)` |
| manifest | 1024/1024 | `ChecksumMismatch(manifest)` |
| commit_footer | 1024/1024 | `ChecksumMismatch(commit-footer)` |
| section_directory | 1024/1024 | `Truncated` / `BadSection` / `ChecksumMismatch` |
| alignment_padding | **0/1024** | *(inert — uninterpreted)* |
| header_inert_metadata | **0/1024** | *(inert — uninterpreted)* |

**Deep verifier** (in-memory tamper → `verify_deep_located`) — **1024/1024 =
1.0000** for every structure, with located offsets:

| structure | detected | located section | offsets non-zero |
|---|---:|---|---:|
| core | 1024/1024 | `core` | 0 |
| history | 1024/1024 | `history` | 1024 |
| checkpoint | 1024/1024 | `checkpoint` | 1005 |
| bloom-global | 1024/1024 | `bloom-global` | 0 |
| bloom-contextual | 1024/1024 | `bloom-context` | 0 |
| bloom-regional | 1024/1024 | `bloom-region` | 0 |
| bloom-path | 1024/1024 | `bloom-path` | 1019 |
| blocks | 1024/1024 | `blocks` | 0 |

Acceptance booleans in the artifact are all `true`:
`fast_path_all_detected`, `deep_verify_all_detected`,
`inert_bytes_all_non_interpreted`.

## 5. What needs `verify`, and what needs `AUTH`

- **Fast path (`open`)** rejects a flip in any *covered* serialized structure —
  this is the cheap, always-on guard, and it is 100% in the measured court.
- **`verify` / `verify_deep`** additionally re-derive every structure from the
  authoritative machine, so they also catch a *semantically* corrupt structure
  whose bytes are individually checksum-consistent (an in-memory tamper, a
  forged-but-CRC-recomputed payload). `verify_deep` is the 100% guarantee; the
  fast path already covers the serialized case.
- **`AUTH`** is the *only* thing that covers a **consistent rewrite** by an
  adversary who recomputes every unkeyed CRC/BLAKE3: the keyed BLAKE3 MAC over
  `file[0..auth_header_offset)` binds every covered byte — core, padding, every
  section, and the manifest. Without the key, no rewrite is accepted. The court
  demonstrates this: a padding flip is *ignored* by the unkeyed read but
  *rejected* by `open_require_auth` (see `tests/corruption.rs`).

## 6. Honest limits

1. **Integrity ≠ authenticity.** Unkeyed `.ekb` files detect *corruption* — an
   accidental single-bit flip anywhere covered — at the containing
   section/frame granularity. They do **not** detect an adversarial, fully
   self-consistent rewrite; only `AUTH` does (see [`SECURITY.md`](SECURITY.md)).
2. **Inert bytes are not protected.** Alignment padding and the section-header
   `flags`/`reserved`/`generation` fields (and a regional `table_offset`) are not
   covered by any unkeyed check. A flip there is a no-op, never interpreted — but
   it is also not an error in unkeyed mode. `AUTH` closes this gap.
3. **The universal bloom's absent slots** are covered by the section CRC at the
   fast path, but the deep verifier reads only the presence mask and the *present*
   slots, so it does not semantically check absent-slot filler (which is never
   interpreted).
4. **Granularity.** The fast path attributes a corruption to the containing
   section/frame (and, when a caller already knows the byte, `section_span_at`
   names the section and span). The deep verifier narrows to a logical offset for
   history / checkpoint / path / block structures.

## 7. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-corruption.sh     # writes evidence/bench/corruption.json
```
