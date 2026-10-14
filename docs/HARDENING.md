# EKBLOOM adversarial-input hardening — measured worst case

**Scope.** The untrusted boundary: anything an attacker can put into a `.ekb`
image, a machine core, a dictionary record, or a backing file. This document is
the threat matrix (**input class → expected typed error → measured worst-case
memory/CPU**), the **AUTH downgrade/rollback** analysis, the **mapped-file
truncation** handling, and the **residual risks**. Every number is measured by
`examples/hardening.rs` and recorded in
[`evidence/bench/hardening.json`](../evidence/bench/hardening.json); every claim
is pinned by [`tests/hardening.rs`](../tests/hardening.rs).

**Modules:** [`src/reader.rs`](../src/reader.rs),
[`src/format.rs`](../src/format.rs), [`src/resource.rs`](../src/resource.rs),
[`src/dictionary.rs`](../src/dictionary.rs),
[`src/machine_dict.rs`](../src/machine_dict.rs), [`src/mapped.rs`](../src/mapped.rs).
**Companion:** [`SECURITY.md`](SECURITY.md) (AUTH model), [`CORRUPTION.md`](CORRUPTION.md)
(bit flips), [`ALLOC.md`](ALLOC.md) (allocation audit).

**Reproduce.**

```sh
sh scripts/container-verify.sh     # fmt + clippy(-D warnings) + full suite (incl. tests/hardening.rs)
sh scripts/container-fuzz.sh       # libFuzzer, incl. the five hardening targets
sh scripts/run-hardening.sh        # writes evidence/bench/hardening.json (memory-capped 4g)
sh scripts/container-reproduce.sh  # clean build + release suite + evidence validation
```

---

## 1. The invariant

> **Every adversarial input yields a typed `Err` — never a panic, never an OOM,
> never a wrong byte — and its peak memory and CPU stay bounded.**

Concretely, for one adversarial input:

* **Memory.** The input never causes an allocation over the
  [`ResourcePolicy`](../src/resource.rs) ceiling (`1 GiB`,
  `DEFAULT_MAX_SINGLE_ALLOCATION`). A declared length past the ceiling is refused
  *before* any `Vec::with_capacity`/`reserve`/`resize`, as
  `EkbloomError::AllocationTooLarge`. The only allocation proportional to the
  input is the input itself (`reader::open` reads it with `std::fs::read`); the
  streaming [`FileBody`](../src/reader.rs) path avoids even that.
* **CPU.** The error path is `O(input)`: no unbounded loop, no nontermination. A
  crafted non-renormalising decoder stream is cut off with
  `EkbloomError::NonTermination` by a per-byte step budget
  (`machine_dict::step_cap`); every declared count/length is checked against the
  available bytes with checked arithmetic before it drives work.

---

## 2. Threat matrix (measured)

`sh scripts/run-hardening.sh` runs each class in its own child process, so the
recorded peak RSS is attributable. All numbers are from
`evidence/bench/hardening.json`; the `ResourcePolicy` ceiling is **1 GiB =
1,073,741,824 B** and the container cap is **4 GiB**.

| adversarial input class | crafted input | expected typed error | measured peak RSS | measured worst-case CPU |
|---|---|---|---|---|
| **malformed declared lengths** | `core_len = u64::MAX`; a section header `length = u64::MAX`; history `count = u64::MAX`; checkpoint `count = u64::MAX`; block index with `logical_len = u64::MAX` (`block_bytes = 1`); raw junk | `Overflow { .. }`, `Truncated { .. }`, `BadSection { .. }` | **2,174,976 B (2.07 MiB)** | **1.8 µs** |
| **allocation exhaustion** | machine-5 core declaring `u64::MAX`; machine-6 fixed core declaring `u64::MAX`; declared `DEFAULT_MAX_SINGLE_ALLOCATION + 1`; caller lengths `u64::MAX` for machines 2/4/9 | `AllocationTooLarge { requested, limit }` (320×), `Truncated` (64×) | **2,064,384 B (1.97 MiB)** | **1.9 µs** |
| **dictionary substitution** | forged/unregistered referenced id; `dict_len = u32::MAX`; embedded dict B relabelled as A; a body whose core is a mismatched machine-9 record | `BadSection` (unknown/unregistered id, embedded id mismatch), `CorruptGeneration(core hash mismatch)`, `Truncated` | **2,134,016 B (2.04 MiB)** | **3.8 µs** |
| **authentication downgrade** | AUTH stripped; AUTH payload flipped; wrong key; unkeyed file; rollback to an older authenticated generation | `CorruptGeneration(authenticated open requires an AUTH section)`, `ChecksumMismatch(auth)`, `BadSection(footer marker missing)`; rollback **accepted** (§3) | **6,590,464 B (6.29 MiB)** | **93.9 µs** |
| **mapped/file truncation** | a backing `.ekb` truncated under an active `FileBody`; a mapped file shrunk under a live `MappedSurface`; a file truncated to zero | `Io(_)`, `OutOfBounds { .. }`, `InvalidArgument { .. }`; retained-prefix reads exact | **2,125,824 B (2.03 MiB)** | **1.7 µs** |

**Summary (from the artifact).** `all_typed_errors = true`,
`all_bounded = true`, `max_peak_rss_bytes = 6,590,464` (**6.29 MiB**, 0.006× the
1 GiB ceiling), `max_wall_ns = 93,916` (**93.9 µs**). No class approaches the
policy ceiling, and every crafted input returned a typed error.

---

## 3. AUTH downgrade / rollback analysis

The strict entry points (`open_require_auth`, `open_bytes_require_auth`,
`verify_file_require_auth`) require a valid `AUTH` section; the unkeyed `open`
deliberately ignores one. Measured behaviour:

| attempt | result | typed error / note |
|---|---|---|
| **strip AUTH** (remove the trailing section) | **rejected** | `CorruptGeneration(authenticated open requires an AUTH section)` |
| **flip AUTH to absent** (corrupt the tag; its frame CRC fails) | **rejected** | the malformed candidate is treated as absent |
| **wrong key** | **rejected** | `ChecksumMismatch { section: "auth" }` |
| **downgrade authenticated → unkeyed** | strict mode **rejected**; the *unkeyed* `open` succeeds (by design) | `open_require_auth` on an unkeyed file → `CorruptGeneration`; there is no unauthenticated fallback in strict mode |
| **rollback to an older authenticated generation** | **accepted** — residual risk | the older generation's MAC still covers its own (unchanged) prefix, so it verifies under the same key |

The load-bearing fuzzing invariant (`fuzz_auth_downgrade`): **if no commit
contains an `AUTH` section, strict mode must return `Err`.** The target also
transforms a genuine authenticated image (strip / truncate / flip / splice) and
re-asserts the same invariant, so no crafted byte sequence silently downgrades an
authenticated read.

**Rollback is honest, not fixed.** The `AUTH` tag binds **bytes**, not **time or
freshness** (see [`SECURITY.md`](SECURITY.md) §6, T5): an adversary who serves a
wholly genuine *older* file — or truncates the file at an earlier generation
boundary — presents an object that still authenticates. EKBLOOM does not
implement anti-rollback; a deployment that needs it must supply freshness out of
band (a trusted monotonic tip pointer, a signed generation log, or an
append-only external witness).

---

## 4. Mapped-file truncation

Two distinct paths, handled differently:

* **The untrusted read path is the safe positional-pread `FileBody`** (and
  `OpenObject`) — no `mmap`, no `unsafe` (`src/reader.rs`, `src/handle.rs`). If
  the backing file is truncated *under an active reader*, a read either fails
  with a typed error (`Io(_)` from a short `pread`, or `Truncated`/`OutOfBounds`
  from the extent check) or returns the **correct** bytes; it never panics,
  over-reads, or returns a wrong byte. This is what `tests/hardening.rs` and
  `fuzz_mapped_truncate` assert.
* **`MappedSurface`** (`src/mapped.rs`, the third sanctioned `unsafe` module) is
  **never on the untrusted parser path** — the parser does not reference it
  (`SAFETY.md` §4, invariant 4). Its `read_into` bounds every read against the
  file length captured at `open`; a live mapping whose backing file shrank still
  reads the *retained* prefix exactly, and `MappedSurface::open` on a zero-length
  (truncated) file fails cleanly with `InvalidArgument`. Reading past the new EOF
  through a stale mapping is the documented caller responsibility (the mapping
  requires an **immutable** backing file, `SAFETY.md` §4 invariant 1); the tests
  and fuzzers never exercise it, so the harness cannot `SIGBUS`.

---

## 5. Fuzzing targets

Five targets were added to `fuzz/Cargo.toml` and wired into
`scripts/container-fuzz.sh` (each seeded from the closest committed corpus in
`evidence/fuzz/`):

| target | boundary exercised |
|---|---|
| `fuzz_declared_lengths` | the `.ekb` scan plus every raw structure parser from arbitrary bytes (huge/overflowing counts and lengths) |
| `fuzz_alloc_exhaustion` | whole-object `decode`/`state_log`/`bulk_unpack`/`unpack_range`/`decode_into` for machines 2/4/5/6/9 under a small `ResourcePolicy` |
| `fuzz_dict_substitution` | `OrANS-DICT` framing: forged/relabelled/oversized ids and dictionary/record mismatches |
| `fuzz_auth_downgrade` | `open_bytes_authenticated` / `open_bytes_require_auth`, including the no-`AUTH`-⇒-reject invariant |
| `fuzz_mapped_truncate` | a backing file truncated under an active `FileBody`, and a live `MappedSurface` reading a retained prefix |

All targets pass the bounded campaign (`sh scripts/container-fuzz.sh`), and every
target is **total**: it returns a value or a typed error, never a panic.

---

## 6. Tests

`tests/hardening.rs` — one regression per class:

* `malformed_declared_lengths_are_typed_errors` — huge/overflowing history,
  checkpoint, block-index, `core_len` and section-`length` declarations are typed
  errors within a 5 s bound;
* `allocation_exhaustion_is_refused_not_attempted` — forged machine-5/6 cores and
  caller lengths yield `AllocationTooLarge`; the ceiling is inclusive and one past
  is refused; a legitimate bounded decode still works;
* `dictionary_substitution_is_a_typed_error` — forged/unregistered id,
  oversized `dict_len`, relabelled embedded dictionary, and a mismatched
  dictionary/record core inside a body;
* `authentication_downgrade_and_rollback` — strip / flip / wrong-key / unkeyed are
  rejected, and the older-generation rollback is accepted and documented;
* `backing_file_truncation_under_an_active_reader` — a `FileBody` read after the
  backing file shrinks is a typed error or the correct bytes, a retained
  mapped-prefix read is exact, and a zero-length file does not map.

---

## 7. Residual risks

1. **AUTH rollback / replay is not detected** (§3, `SECURITY.md` §6 T5). The tag
   is byte-level, not freshness-level; supply anti-rollback out of band.
2. **`MappedSurface` requires an immutable backing file** (`SAFETY.md` §4). It is
   not on the untrusted path; a stale mapping read past a shrunken EOF is the
   caller's responsibility, and deployments should keep the mapping contract.
3. **Input-proportional amplification is `O(input)`, not constant.** Several
   parsers materialise a structure from a declared count that is bounded only by
   the available bytes — e.g. `BlockIndex`/`CheckpointSet` `Snapshot` vectors
   (packed forms up to ~8× the declared bytes), `History` (`4·count`), and
   `ManifestRecord` (many small `String`s). Each stays `O(input)` and inside the
   policy ceiling, but a caller that admits a very large untrusted image still
   pays proportional memory; prefer the streaming `FileBody`/stream paths, which
   never materialise a whole declared object.
4. **`reader::open` reads the whole input into memory** (`std::fs::read`). This is
   `O(input)` and equal to the input size, not an amplification, and it is not
   over the policy ceiling for inputs within it; the file-backed `FileBody` path
   avoids the copy entirely.

**No reachable panic or OOM was found, and no `src/` change was required:** the
boundary was already total (checked arithmetic, `checked_capacity` guards, and the
`NonTermination` step budget). This work adds the typed-error fuzzers, the
bounded measurement harness, the regression suite, and this analysis.
