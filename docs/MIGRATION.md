# EKBLOOM wire-format migration and compatibility

**Owners:** `src/format.rs`, `src/machine*.rs`, `src/reader.rs`, `src/writer.rs`,
`src/error.rs`. **Companion:** [`CORE-LAYOUT.md`](CORE-LAYOUT.md) (exact byte layout),
[`CODEC.md`](CODEC.md) (machine semantics).

This document records the version history, the exact read/write compatibility
matrix, and the migration policy that makes stored `.ekb` data safe across releases.

---

## 1. Version tags

EKBLOOM carries several independent version tags. A reader validates each one and
**refuses any value it does not know** with a typed error — it never falls back to
best-effort parsing.

| tag | where | this build writes | refusal error |
|---|---|---|---|
| `format_major` / `format_minor` | `.ekb` genesis header (`u16` at 8 / 10) | `0` / `3` | `UnsupportedVersion { major, minor }` |
| `core_version` | `.ekb` genesis header (`u16` at 68) | `1` | `UnsupportedCoreVersion { machine_id, core_version }` |
| `machine_id` | `.ekb` genesis header (`u32` at 64) | machine-specific `1..9` | `UnknownMachine { id }` |
| manifest record version | `MANIFEST` payload (first `u32`) | `1` | `UnsupportedManifestVersion { version }` |
| checkpoint container | `EKBCKPT\0` `u8` version | `2` (plus legacy magic-less) | `BadSection` |
| block index | `EKBKBLKS` `u8` version | `1` | `BadSection` |
| segment container | `ESEG` `u16` version | `1` | `BadSection` |

The `.ekb` header is fixed at 80 bytes. `core_version` occupies what were two
formerly-reserved bytes; a file written before the field existed leaves them zero,
which decodes as `core_version = 0` and is accepted as the **legacy/implicit**
version. No machine's *core payload* layout changed to introduce this field: the
field lives in the container header, so every machine's `encode` output is
byte-identical to before.

---

## 2. Read/write compatibility matrix

This build is **release 0.6.0 on wire format line 0.3**.

| artifact | versions this build **READS** | this build **WRITES** |
|---|---|---|
| `.ekb` container (`format_major.minor`) | `0.0`, `0.1`, `0.2`, `0.3` | `0.3` |
| machine `core_version` (machines 1–9, incl. `OrANS-DICT`) | `0` (legacy), `1` (current) | `1` |
| manifest record | `1` | `1` |
| checkpoint container | `2` and legacy magic-less | `2` |
| block index | `1` | `1` |
| segment container (`ESEG`) | `1` | `1` |

Consequences a decoder must implement:

- **Unknown `.ekb` format version → refuse.** `format_major != 0` or
  `format_minor > 3` is a hard error. Future versions are not read speculatively.
- **Unknown core version → refuse.** For a known `machine_id`, a `core_version`
  outside `{0, 1}` is `UnsupportedCoreVersion`. For an unknown `machine_id` the
  machine lookup fails with `UnknownMachine`; the version check never masks it.
- **Unknown manifest record version → refuse** with `UnsupportedManifestVersion`.
- A **legacy** file (`core_version = 0`) reads and verifies exactly; the reader
  records the version it saw but does not alter the bytes.

---

## 3. Migration policy

1. **A machine's semantics are identified by `machine_id` (+ `core_version`).**
   The step function `F`, the permutation `S(v)`, the genesis state, and any
   transition define what a symbol *is*. They are frozen per `machine_id`.

2. **Changing `S(v)` or any transition requires a NEW `machine_id`** (or, for a
   purely physical/layout change with identical semantics, a bumped
   `core_version`). An old core is **never** reinterpreted under new semantics:
   - identical semantics, changed byte layout → keep `machine_id`, bump
     `core_version`, and list both versions in the machine's supported set;
   - changed semantics (different `S`, different transition, different genesis) →
     allocate a **new `machine_id`** and register it; the old `machine_id` keeps its
     original decode forever.

3. **Adding a structure or section kind is additive.** New structures are written
   as a new generation; existing sections, the header, the footer, and the recovery
   model are unchanged. An unknown *section kind* is rejected, so a reader that does
   not implement a new structure does not silently ignore it.

4. **A reader rejects unknown versions with a typed error**, never guesses and never
   partially decodes.

5. **Writers must emit the current version** for the machine they use
   (`format::core_version_for_machine`). A writer must not claim a version it does
   not implement.

---

## 4. Worked example — a hypothetical `S(v)` change

Suppose a future release wants a different byte permutation, e.g.
`S'(v) = v ^ (v<<1) ^ (v<<2)` instead of machine 1's
`S(v) = v ^ (v<<1) ^ (v<<3)`.

**What is forbidden:** editing machine 1's `S` in place. Old machine-1 cores were
encoded against `S`; reinterpreting them against `S'` would silently return wrong
symbols for every stored object, with no error.

**What is required:**

1. Keep `MACHINE_NL2 = 1` decoding with `S` unchanged.
2. Allocate a new identifier, e.g. `MACHINE_NL2P = 10`, whose `encode`/`reveal` use
   `S'`; register it in `machine_by_id`, and give it a supported core-version set
   `{1}` (it has no legacy cores). (Machine 9 is already `OrANS-DICT`; a new machine
   must take a fresh id.)
3. Register its `core_version` set in
   `format::supported_core_versions` (`{CORE_VERSION_CURRENT}`); an old reader that
   does not know machine 10 rejects the file with `UnknownMachine`, and a reader that
   knows machine 10 but not a future version rejects with `UnsupportedCoreVersion`.
4. Document the new machine here and in `CODEC.md`.

The stored bytes are then unambiguous: `(machine_id = 1)` ⇒ `S`,
`(machine_id = 10)` ⇒ `S'`. No existing file changes meaning, and no reader has to
guess.

**A layout-only change** (same semantics) is cheaper: keep the `machine_id`, bump
its `core_version` from `1` to `2`, add `2` to the supported set, and document the
byte-layout difference in `CORE-LAYOUT.md`. Readers accept versions `0`, `1`, `2`;
writers emit `2`.

---

## 5. Version history

| release | wire format | machine core `core_version` | notes |
|---|---|---|---|
| 0.3 | `0.3` | (field absent; reads as `0`) | machines 1–8 (machine 9 arrives in 0.5); checkpoints v2, blocks v1, tiles, segments v1 |
| 0.5 | `0.3` | `1` written, `{0,1}` read | adds machine 9 (`OrANS-DICT`, the shared-dictionary record machine) and explicit `core_version` (formerly-reserved header bytes), plus typed `UnsupportedCoreVersion` / `UnsupportedManifestVersion` errors; machine cores byte-identical; machine semantics unchanged |
| 0.6 | `0.3` | `1` written, `{0,1}` read | **no wire change**: a docs/evidence release adding the frozen v0.3.0/v0.4.0 reverse-compat fixtures (§6), the `{x86_64-simd, x86_64-scalar, aarch64}` CI matrix, the RocksDB-style skew court, Loom-checked RCU publication, the one-line dictionary API (`Dictionary::train`/`create_with_dict`), transactional-publication fault injection and cross-filesystem determinism; the `.ekb` wire line stays `0.3` and machine cores remain byte-identical |

The `0.5` change is **backward-compatible on read**: a 0.5 reader opens every 0.3/0.4
file (`core_version` reads as legacy `0`), and the only byte difference a 0.5 writer
introduces is the two-byte `core_version` field in the genesis header (plus the
reserved bytes it no longer needs). The `0.4` release was a documentation/packaging
release on the same wire line `0.3` (machines 1–8).

The `0.6` release is a **documentation/evidence + API** release on the **same wire
line `0.3`**: it changes **no stored byte** (the machine cores stay byte-identical and
a 0.6 writer emits exactly what 0.5 emitted). It pins reverse-compatibility with the
12 frozen v0.3.0/v0.4.0 fixtures below and adds the Q/R evidence; the frozen-fixture
rule (§6.4) is unchanged.

---

## 6. Frozen fixtures

The compatibility claims in §2 and §5 are not just prose: they are pinned by
**frozen `.ekb` fixtures written by older releases**, checked in under
[`tests/fixtures/`](../tests/fixtures) and asserted by
[`tests/compat_fixtures.rs`](../tests/compat_fixtures.rs).

### 6.1 What is checked in

`tests/fixtures/expected.json` records, per fixture, the writer release and the
expectations the current build must reproduce:

| file | writer | source | machine | profile | core_version | logical_len | physical_bytes |
|---|---|---|---|---|---:|---:|---:|
| `v0.3_elf_root_m1.ekb` | 0.3.0 | 256-B ELF prefix | 1 (`OrANS-NL2`) | root | 0 | 256 | 256 |
| `v0.3_text_root_m2.ekb` | 0.3.0 | 512-B text | 2 (`OrANS-RANS0`) | root | 0 | 512 | 872 |
| `v0.3_skew_root_m2.ekb` | 0.3.0 | 4-KiB skewed 4-symbol | 2 | root | 0 | 4096 | 1371 |
| `v0.3_skew_path_m2.ekb` | 0.3.0 | 4-KiB skewed 4-symbol | 2 | path(D=4) | 0 | 4096 | 9798 |
| `v0.3_empty_root_m1.ekb` | 0.3.0 | empty | 1 | root | 0 | 0 | 0 |
| `v0.3_one_root_m1.ekb` | 0.3.0 | 1 byte (`0x5A`) | 1 | root | 0 | 1 | 1 |
| `v0.4_*` (same six) | 0.4.0 | identical sources | identical | identical | 0 | identical | identical |

`tests/fixtures/expected.json` additionally records a **whole-file BLAKE3**
(`file_blake3`) per fixture, asserted **before** `open` by
[`tests/compat_fixtures.rs`](../tests/compat_fixtures.rs), so any altered byte (even an
otherwise-uninterpreted inert byte) fails the compatibility claim.

The 0.3.0 and 0.4.0 artifacts are byte-identical, which is the frozen evidence
that 0.4.0 was a documentation/packaging release on wire line `0.3`: their
`format_major.minor` is `0.3` and their `core_version` field is `0` (the legacy,
implicit value the old writers left in the formerly-reserved bytes).

### 6.2 Genuine provenance

These are **genuine old-version files**, not current-code output relabelled. They
were written by the old releases' own `create_machine` / `write_body` by checking
their release commits out into git worktrees and running a small emitter built
*against that release's library*:

* `0.4.0` → commit `8a9c217`; `0.3.0` → commit `b26a165` (the crates.io publish).
* the emitter is [`examples/emit_fixtures.rs`](../examples/emit_fixtures.rs); it
  uses only the API surface common to 0.3.0, 0.4.0 and the current build, so the
  same source compiles inside each worktree as `src/bin/emit_fixtures.rs`.
* the process is [`scripts/regen-fixtures.sh`](../scripts/regen-fixtures.sh).

### 6.3 What current code guarantees

For every fixture, `tests/compat_fixtures.rs` asserts that the current reader
`open`s the file and reproduces, byte-for-byte: the genesis-header fields; the
logical bytes at a recorded spread of sampled positions (through `byte_at`), and
the BLAKE3 hash over them; `physical_bytes()`, `logical_len` and `machine_id`;
and that both `verify` and `verify_deep` pass. It also asserts the artifact's
bytes are **unchanged** by opening it. A second test tampers with a copy (a
flipped core byte and a flipped footer byte) and requires `open` to refuse it, so
the fixtures are demonstrably load-bearing.

### 6.4 The rule

**These fixtures are frozen.** They are never rewritten by the test suite; the
only way to change them is to run `scripts/regen-fixtures.sh` deliberately.
Changing the on-disk format therefore requires, in the same change:

1. a new release commit that writes the new format, with the emitter run against
   it (extending the fixture set with `v<next>_*` files and new `expected.json`
   entries) — existing `v0.3_*`/`v0.4_*` files are **never** touched or removed;
2. an entry in §5 version history and the §2 compatibility matrix; and
3. a `tests/compat_fixtures.rs` that opens *both* the old and the new fixtures.

A pull request that changes the wire format without new frozen fixtures, or that
edits an existing fixture, is a migration-policy violation.
