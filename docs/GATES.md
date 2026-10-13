# EKBLOOM — machine-checked acceptance gates G1–G5

**Runner:** [`scripts/run-gates.sh`](../scripts/run-gates.sh) ·
**Implementations:** [`tests/gates.rs`](../tests/gates.rs) ·
**Evidence:** [`evidence/gates/G1.json`](../evidence/gates/G1.json) …
[`G5.json`](../evidence/gates/G5.json)

These are the executable acceptance gates for EKBLOOM, analogous to the OrFS
trust-building gates A–N: each gate is a property the crate must keep, checked by
running code — not by prose. Every gate asserts its property against the public
API, prints `GATE G<n> <name>: PASS — <evidence>`, and writes its own evidence
JSON; a violation **panics**, so the runner exits non-zero and reports FAIL. A
gate never passes by returning wrong bytes: every byte-exact gate compares the
decode against the source.

```sh
sh scripts/run-gates.sh   # runs G1–G5 inside the pinned container; exits non-zero on any FAIL
```

`run-gates.sh` runs each gate as its own `cargo test --release --test gates`
invocation (single-threaded) so a failure in one gate does not hide the others,
prints a `PASS`/`FAIL` line per gate, and exits non-zero if any failed. The gate
body reuses the same public-API relations as the focused tests and benches noted
below (and gate G3 literally reuses the shared `tests/common/corpora.rs` oracle).

---

## The gates

| gate | property | mirrors |
|---|---|---|
| **G1** | invertibility: `decode(encode(x)) == x`, machines 1–9 | `tests/dictionary.rs`, `tests/machine_*` |
| **G2** | prefix composition: affine block composition is exact & associative | `tests/prefix.rs` |
| **G3** | worst-case bound: `measured_steps <= worst_case_steps` over 1,000,000 positions | `tests/corpora_bench.rs`, `examples/worstcase_bound.rs` |
| **G4** | corruption detection: single-bit flips in covered sections are detected | `tests/corruption.rs`, `examples/corruption_court.rs` |
| **G5** | physical accounting: `physical_bytes()` vs `to_bytes().len()` per structure | `tests/accounting.rs` |

---

## G1 — invertibility (`encode∘decode = id`, all machines)

**Property.** For every registered machine (`machine_id` 1–9) and a spread of
logical lengths up to `2^20`, decoding an encoded core returns the source
byte-for-byte. `G1` also checks the affine-prefix property where it applies.

**How.** For each id in `1..=9` and length in `{1,2,3,17,255,256,257,4095,4096,4097,65535,65536,65537,262144,1048575,1048576}`: `m.encode(src)`, then `m.decode(core, n)` and byte-compare.

**Result (PASS).**

```text
GATE G1 invertibility: PASS — machines=1..9 cases=144 max_len=1048576 seg_large_path=SegContext::decode_into
```

**Honest note (library performance issue, not a correctness failure).**
`OrANS-SEG` (`machine_id = 6`) does **not** override the reference
`OrMachine::decode`; its `Seg::reveal` re-parses the whole core on **every**
symbol (`src/machine_seg.rs`), so the trait `decode` is `O(n^2)` — measured at
**415 s for n = 2^20**. The linear segment-wise `SegContext::decode_into` (the
path the read API uses) is used for the large lengths in G1. Invertibility holds
on both paths; the trait `decode` is the slow one. This is reported, not patched
(the library modules are owned elsewhere).

## G2 — prefix composition (affine block composition exact)

**Property.** A block's affine summary equals repeated machine steps; the linear
part is `A^len`; the associative prefix scan reconstructs start states exactly and
drives a byte-exact reconstruction equal to `NL2::decode`; composition is
associative with an identity element.

**Result (PASS).**

```text
GATE G2 prefix-composition: PASS — affine_checks=4811 byte_exact=true
```

## G3 — worst-case bound holds (1,000,000 positions)

**Property.** Every read served from a certified restart performs
`positioning <= worst_case_steps_for(request)`. G3 drives the bounded engine over
**1,000,000** uniformly random positions on a body with a 1 MiB block restart
(per-region bound `B-1 = 1,048,575`), asserting the inequality for **every**
sample while byte-checking the output.

**Result (PASS).**

```text
GATE G3 worst-case-bound: PASS — samples=1000000 max_measured=1048575 max_bound=1048575 violations=0 mismatches=0 per_region_bound=1048575
```

**Zero violations, zero mismatches across 1,000,000 samples.**

## G4 — corruption detection (single-bit flips)

**Property.** A single bit flip anywhere in a covered section payload is detected
by the fast path (`reader::open_bytes` → CRC-32 / core BLAKE3 / footer CRC-32),
and is localizable to the containing section at a **≤ 4 KiB granularity** (the
reported window is the section payload length capped at the 4 KiB grid).

**Result (PASS).**

```text
GATE G4 corruption-detection: PASS — sections=18 flips=1152 detected=1152 max_reported_window=4096
```

A rich body (logical length 4,096) is serialized, then **1152 single-bit flips**
(64 per covered section, 18 sections) are applied across the payloads; all 1152
are rejected at `open_bytes`, and the worst reported localization window is
exactly 4096 bytes. See [`CORRUPTION.md`](CORRUPTION.md) for the full
1024-flips-per-structure court.

## G5 — physical accounting exact

**Property.** Every manifested structure's accounted `physical_bytes()` equals its
serialized length, or differs from it by **exactly one documented fixed container
header** — never by a re-counted *shared* header (the rANS model header lives once
in the core).

**Result (PASS).**

```text
GATE G5 physical-accounting: PASS — structures=46 exact_equality=18 fixed_header_relation=28
```

Across machines 1–9: the root body is exactly its core (`physical_bytes() ==
core.len()`); `BlockIndex` (and the packed forms) satisfy exact equality; `History`,
`CheckpointSet`, the path `BloomShell` and the packed forms satisfy `to_bytes().len()
== physical_bytes() + <fixed header>`; and `inspect` attributes the same total to
the same components. 18 structures at exact equality, 28 at the documented
fixed-header relation.

---

## Reproduce

```sh
sh scripts/container-verify.sh    # fmt + clippy(-D warnings) + full suite
sh scripts/run-gates.sh           # G1–G5 (this document); writes evidence/gates/*.json
sh scripts/container-reproduce.sh # clean release build + release suite + evidence validation
```

The gates are also exercised as ordinary tests by the full suite
(`cargo test --all-features` / `cargo test --release`), so a regression fails CI
even without `run-gates.sh`.
