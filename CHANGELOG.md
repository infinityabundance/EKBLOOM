# Changelog

All notable changes to `ekbloom` are documented in this file. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning follows the policy in the
[README](README.md#versioning-msrv-and-the-wire-format): for `0.y.z` a breaking
change to the public Rust API (or an MSRV bump) increments the **MINOR** version.
The **on-disk wire format is versioned independently of the crate** — a crate
release does **not** imply a wire change, and vice versa. Entries that change what
is written to disk are marked **⚠ wire format** and their rules live in
[`docs/MIGRATION.md`](docs/MIGRATION.md).

## [Unreleased]

### Added

- Nothing yet.

## [0.7.0] - 2026-10-14

**Wire format: no change.** The `.ekb` line stays `0.3`; machine cores remain
byte-identical and a 0.7.0 writer emits exactly what 0.6.0 emitted. This is the
**S-round** (S1–S8): a documentation/evidence, hardening and integration release
reviewed as Revision 11 (verdict **no library defect found; all claims held**). The
MINOR bump is for the additive public API (the online `Controller`) and the newly
documented MSRV/semver policy; **no stored byte changes**.

### Added

- **Central I/O accounting (`ReadLedger`)**: every physical `FileBody` read path —
  tile reads, bounded encoded windows, the decoder model (dictionary) header load,
  and section/metadata reads (restart records, path-bloom prefixes, history states)
  — charges one bounds-checked, ledger-counted wrapper, so no path can bypass it.
  `FileBody::fetched_bytes`/`fetched_reads` are now the true totals;
  `tests/ioacct.rs` pins each class and cross-checks `/proc/self/io`, and the
  RocksDB-skew court drops its geometric tile re-addition (S1).
- **Persistence round-trip suite (`tests/persist_all.rs`)**: the writer emits a
  section for **every** manifested structure on both the fresh-write and the append
  path, and each structure round-trips byte-exactly (S2).
- **Repeated, interleaved multirun evidence with bootstrap 95% confidence
  intervals** and a full configuration manifest (`docs/BENCHMARKING.md`,
  `multirun.json`, `config-manifest.json`) (S3).
- **Hardened untrusted-input boundary** (`docs/HARDENING.md`): **five** new fuzz
  targets (**24** total) plus bounded resource measurement (`hardening.json`;
  **5** classes / **86** iterations, peak RSS **6.29 MiB**, every error typed and
  bounded, AUTH rollback residual documented) (S4).
- **Online workload-adaptation controller** (`src/controller.rs`,
  `docs/CONTROLLER.md`) with equal-budget comparisons against a static plan,
  TinyLFU and an LRU block cache (S5).
- **Formal refinement document** and machine-checked obligation runner
  (`docs/REFINEMENT.md`, `scripts/run-refinement.sh`; obligations **O1–O16** all
  PASS) (S6).
- **MSRV/semver + wire-format policy** made explicit in the README and
  `docs/MIGRATION.md` §8 (S7).
- **Real embedded-KV integration** (`docs/DB-INTEGRATION.md`,
  `examples/kv_integration.rs`, `tests/kv_integration.rs`): codec outputs stored as
  `redb` 4.4 values and decoded through a real B-tree, with an honest DB-level
  verdict (S8).

### Changed

- Documentation/evidence and test infrastructure only. **No wire-format change**:
  the `.ekb` line stays `0.3` and machine cores remain byte-identical.

## [0.6.0] - 2026-10-13

**Wire format: no change.** The `.ekb` line stays `0.3`; machine cores remain
byte-identical and a 0.6.0 writer emits exactly what 0.5.0 emitted. This is a
documentation/evidence + public-API release.

### Added

- **Frozen reverse-compatibility fixtures** from the genuine v0.3.0/v0.4.0
  libraries: twelve `.ekb` files checked in under `tests/fixtures/`, pinned by a
  whole-file BLAKE3 and asserted by `tests/compat_fixtures.rs` (Q1).
- **Cross-platform CI matrix** `{x86_64-simd, x86_64-scalar, aarch64}` with
  identical `physical_bytes` across all three jobs (Q2).
- **RocksDB-style skewed KV court**: 100-byte values, 80/20 key skew, footprint,
  I/O amplification and p50/p99.9 vs raw / block LZ4 / Zstd / Zstd-dict (Q3).
- **Loom model of the RCU publication path** (`tests/loom_rcu.rs`; 3 scenarios /
  407 interleavings, no torn read) (Q4).
- **One-line dictionary API** (`Dictionary::train` / `create_with_dict`) (R1).
- **Transactional-publication fault injection** (5,933 fault points, 100 % atomic,
  0 torn containers/generations) and **cross-filesystem determinism**
  (overlayfs / tmpfs / ext2-ext3 byte-identical; xfs/btrfs honestly blocked) (R2).
- Docs: `SECURITY` v2, `docs/OPERATIONS.md`, `docs/MACHINE_CONTRACT.md`.

### Fixed

- `cpu_features` now reports the *effective* (scalar-aware) capability plus
  hardware features.
- Compatibility fixtures hardened with a whole-file BLAKE3 assertion.

## [0.5.0] - 2026-10-13

**⚠ wire format (additive and backward-compatible on read).** The `.ekb` genesis
header gains an explicit `core_version` field, carved from formerly-reserved
bytes. No machine's core payload layout changed, so **every machine's `encode`
output is byte-identical** and a 0.5.0 reader opens every 0.3.0/0.4.0 file
(`core_version` reading as legacy `0`). New typed errors
`UnsupportedCoreVersion` / `UnsupportedManifestVersion`. Version tags, matrix and
policy: [`docs/MIGRATION.md`](docs/MIGRATION.md), [`docs/CORE-LAYOUT.md`](docs/CORE-LAYOUT.md) (P1).

### Added

- **Explicit wire-format versioning**: `core_version` in the genesis header,
  typed refusal of unknown versions, and `docs/MIGRATION.md` + `docs/CORE-LAYOUT.md`.
- **Streaming, downstream-seekable container** (`StreamBuilder`/`StreamReader`)
  with bounded builder memory (2 GiB at 8.6 MiB peak RSS) (P2).
- **Corruption court**: 1024 single-bit flips per structure, 100 % detected,
  localised to ≤4096 B with typed `Corrupt { section, offset }` (P3).
- **Machine 9 `OrANS-DICT`** (`machine_id = 9`): a shared trained order-0
  dictionary for many small records (83.5 B/rec vs machine 2's 590.0) (P5).
- Standard-corpora court + hardware counters + 64-reader concurrency p99 (P6).
- Machine catalogue (`docs/MACHINES.md`), the `choose_plan` budget chooser +
  `docs/BUDGET-GUIDE.md`, and `docs/LIMITATIONS.md` (P7).
- Machine-checked acceptance gates **G1–G5** (`docs/GATES.md`) (P8).

### Changed

- Machine 6 `OrANS-SEG` whole-object `decode` made linear (was `O(n²)`: 415 s →
  0.5 ms at `n = 2^20`); byte-identical to the per-symbol reference.
- `unsafe`/alloc audit: `unsafe` confined to three sanctioned modules
  (`src/simd.rs`, `src/async_io.rs`, `src/mapped.rs`); Miri over 61 unit tests (P4).

### Wire-visible

- **New `machine_id = 9`** (`OrANS-DICT`). A pre-0.5.0 reader rejects it with
  `UnknownMachine { id }`; a legacy file (`core_version = 0`) still reads exactly.

## [0.4.0] - 2026-10-13

**Wire format: no change.** Still the `.ekb` line `0.3`; `format_major.minor` is
`0.3` and `core_version` remains absent (formerly-reserved bytes, decoded as
legacy `0`). The frozen v0.4.0 fixtures are byte-identical to the v0.3.0 ones.

### Added

- **Machines 5–8**: `OrANS-PACK` (id 5, directly-addressable fixed-width packing),
  `OrANS-SEG` (id 6, versioned segmented container), `OrANS-RANS0W` (id 7, 16-bit
  word renorm) and `OrANS-RANS0IL` (id 8, independent lanes).
- Machine 4 (`OrANS-RANS0I`) completed as a first-class cold engine (complete
  successor state) with a request-level bounded-work guarantee (M1–M5).
- Machine-2 independently restartable blocks + a compact packed checkpoint
  container; the unified bounded-access engine; the certified access planner.
- Selective exact hot tiles, persistent verified handles + RCU publication, SIMD
  reconstruction kernels, and a read-only `mmap` surface.
- N-round: bounded encoded-window reads (625× fewer bytes), `OrFS` single
  authoritative store + FUSE projection, joint budget-aware optimiser, and the
  end-to-end real-data proof.
- O-round: decoder court (machines 7/8 and an AVX2 rANS kernel — all rejected,
  best decoder stays machine 4 `K = 8`), uniform-footprint gate, OrFS compaction.

### Fixed

- Two O-round defects (a `cursor + N` overflow and the tight-budget base abort);
  14 unchecked cursor sites hardened across machines 1–4.

### Wire-visible

- **New `machine_id = 5..8`**. A 0.3.0 reader rejects them with
  `UnknownMachine { id }`; the container format itself is unchanged.

## [0.3.0] - 2026-10-12

Initial published release (`ekbloom` 0.3.0 on crates.io).

**⚠ wire format: establishes line `0.3`.** The first published `.ekb` format:
fixed 80-byte genesis header (`EKBLOOM\0`), checksummed, bounds-checked sections,
`MANIFEST`-terminated commit blocks with a `CommitFooter` (`EKBCOMM\0`),
payload → manifest → footer publication ordering, generation selection and crash
recovery. Machine cores carry no separate `core_version` (the field does not yet
exist).

### Added

- **Machines 1–4**: `OrANS-NL2` (id 1), `OrANS-RANS0` (id 2, order-0 rANS,
  compressing), `OrANS-Store` (id 3, verbatim identity, `V = N`) and
  `OrANS-RANS0I` (id 4, interleaved rANS).
- Non-withdrawal history, exact checkpoint restarts, and
  path/contextual/regional/global blooms with opt-in `AUTH` (keyed MAC).
- Adaptive shells and radial readers; budgets as transactional hard caps
  (`AllocationTooLarge`, exact upper-bound `estimate_physical_bytes`).
- In-process `OrFS` logical namespace, asynchronous construction (CPU workers +
  sync fallback), optional `io_uring` publication backend, and the `ekbloom` CLI
  (nine commands).
- Streaming segmented history store at 2^30 scale with bounded builder memory.
- Packed 16-bit restart/history storage (1/6 and 1/2 the bytes), parallel-prefix
  reconstruction, selective hot tiles, persistent verified handles, region-scoped
  manifestation, the rigorous performance court and certified planner, SIMD
  reconstruction kernels, and `mmap` read surfaces.
- Correctness court, property tests, fuzzing and Miri/ASan coverage; the measured
  speed-vs-space frontier and conventional-baseline comparison.

[Unreleased]: https://github.com/infinityabundance/EKBLOOM/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/infinityabundance/EKBLOOM/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/infinityabundance/EKBLOOM/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/infinityabundance/EKBLOOM/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/infinityabundance/EKBLOOM/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/infinityabundance/EKBLOOM/releases/tag/v0.3.0
