# EKBLOOM — trust-building test infrastructure

**Runners:** [`scripts/container-verify.sh`](../scripts/container-verify.sh) ·
[`scripts/container-fuzz.sh`](../scripts/container-fuzz.sh) ·
[`scripts/run-fuzz-seeds.sh`](../scripts/run-fuzz-seeds.sh) ·
[`scripts/run-faults.sh`](../scripts/run-faults.sh) ·
[`scripts/run-crossfs.sh`](../scripts/run-crossfs.sh) ·
[`scripts/container-aarch64.sh`](../scripts/container-aarch64.sh) ·
[`scripts/run-matrix.sh`](../scripts/run-matrix.sh) ·
[`scripts/run-scalar.sh`](../scripts/run-scalar.sh) ·
[`scripts/run-gates.sh`](../scripts/run-gates.sh) ·
[`scripts/run-scale.sh`](../scripts/run-scale.sh) ·
[`scripts/container-reproduce.sh`](../scripts/container-reproduce.sh)
**Gates:** [`docs/GATES.md`](GATES.md) · **Seeds:** [`fuzz/seeds/`](../fuzz/seeds) · **Corpus:** [`evidence/fuzz/`](../evidence/fuzz)

This document describes the trust-building test infrastructure: **fuzz
roundtrips**, **cross-architecture determinism**, the **cross-platform CI matrix**
(the scalar fallback and the aarch64 build), the **scale job**, and the
**acceptance gates G1–G5**. Every number below is from a real run inside the
pinned, memory-capped container; no project tooling ever runs on the host.

---

## 1. The correctness suite

```sh
sh scripts/container-verify.sh    # safety/alloc confinement + fmt + clippy(-D warnings) + full suite
sh scripts/container-reproduce.sh # clean release build + release suite + evidence validation
```

`container-verify.sh` runs the safety/alloc gate, `cargo fmt --all -- --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, and the full test
suite. `container-reproduce.sh` does a clean release rebuild from `Cargo.lock`,
runs the release suite, and validates every `evidence/**/*.json`.

Measured on the pinned image (`rust:1.99.0`, `ekbloom-rust:1.99.0`):

| run | tests passed | failed |
|---|---:|---:|
| `cargo test --all-features` (debug) | **847** | 0 |
| `cargo test --release --locked` (reproduce) | **833** | 0 |

(The release run has fewer tests because it does not enable the optional
`fuse`/`io-uring` features.) The S-round adds `tests/ioacct.rs` (central ledger),
`tests/persist_all.rs` (structure round-trips), `tests/hardening.rs` (the untrusted
boundary), `tests/controller.rs` (the online controller) and `tests/kv_integration.rs`
(the real `redb` integration); counts are the S-round Revision 11 (verify **847**, release
**833**, **58** evidence JSON). Earlier rounds added `tests/faults.rs` and
`tests/crossfs.rs` (see §8/§9).

---

## 2. Fuzz roundtrips

The campaign now runs **24** libFuzzer targets. Earlier rounds added the
machine-roundtrip and streaming targets, then the **authenticated-open** and
**dictionary** targets; the S-round adds five **hardening** targets
([`HARDENING.md`](HARDENING.md) §5):

| target | property |
|---|---|
| `fuzz_roundtrip` ([`fuzz_targets/fuzz_roundtrip.rs`](../fuzz/fuzz_targets/fuzz_roundtrip.rs)) | For arbitrary input, `encode` then `decode` across **all** machines (ids 1–9); when `encode` succeeds, `decode` must succeed and equal the source byte-for-byte, and a shorter requested length must return the source prefix. |
| `fuzz_stream` ([`fuzz_targets/fuzz_stream.rs`](../fuzz/fuzz_targets/fuzz_stream.rs)) | Arbitrary bytes into the streaming reader are total (never panic, incl. `u64::MAX`); a built `StreamBuilder` container must round-trip exactly (sequential + sampled positional reads). |
| `fuzz_open_auth` ([`fuzz_targets/fuzz_open_auth.rs`](../fuzz/fuzz_targets/fuzz_open_auth.rs)) | `[key: 32 bytes][image]` into `open_bytes_authenticated` and the strict `open_bytes_require_auth`: total, never panicking, and a missing/mismatching `AUTH` is rejected — never silently accepted. |
| `fuzz_dictionary` ([`fuzz_targets/fuzz_dictionary.rs`](../fuzz/fuzz_targets/fuzz_dictionary.rs)) | `Dictionary::from_bytes` (bad magic/version, bad frequency sum, zero, id mismatch) is total; a validated record can be registered, looked up and used to decode a forged machine-9 core without an unbounded allocation or panic. |
| `fuzz_declared_lengths` ([`fuzz_targets/fuzz_declared_lengths.rs`](../fuzz/fuzz_targets/fuzz_declared_lengths.rs)) | The `.ekb` scan plus every raw structure parser from arbitrary bytes (huge/overflowing counts and lengths) returns a typed error — never a panic or an unbounded allocation. |
| `fuzz_alloc_exhaustion` ([`fuzz_targets/fuzz_alloc_exhaustion.rs`](../fuzz/fuzz_targets/fuzz_alloc_exhaustion.rs)) | Whole-object `decode`/`state_log`/`bulk_unpack`/`unpack_range`/`decode_into` for machines 2/4/5/6/9 under a small `ResourcePolicy`: a declared length past the ceiling is `AllocationTooLarge`. |
| `fuzz_dict_substitution` ([`fuzz_targets/fuzz_dict_substitution.rs`](../fuzz/fuzz_targets/fuzz_dict_substitution.rs)) | `OrANS-DICT` framing: forged/relabelled/oversized ids and dictionary/record mismatches are typed rejects. |
| `fuzz_auth_downgrade` ([`fuzz_targets/fuzz_auth_downgrade.rs`](../fuzz/fuzz_targets/fuzz_auth_downgrade.rs)) | `open_bytes_authenticated`/`open_bytes_require_auth`: strict mode rejects any image with no `AUTH`; a genuine image transformed (strip/truncate/flip/splice) never downgrades silently. |
| `fuzz_mapped_truncate` ([`fuzz_targets/fuzz_mapped_truncate.rs`](../fuzz/fuzz_targets/fuzz_mapped_truncate.rs)) | A backing file truncated under an active `FileBody`, and a live `MappedSurface` reading a retained prefix: a typed error or the correct bytes, never a panic. |

`fuzz_roundtrip` reads the input as `[selector: up to 3 bytes][body]` and derives
a requested length in `[0, 2^20]`, materialized by cyclically repeating the body.
Per-iteration materialization is capped at 4 KiB (`FUZZ_LEN`) for fuzz throughput;
the full `2^20` domain is machine-checked deterministically by gate **G1**
(`docs/GATES.md`). `OrANS-SEG` uses the linear `SegContext::decode_into` path
because the trait `decode` is quadratic (see G1).

### Seed corpus

`fuzz/seeds/` holds five committed 4 KiB prefixes of the real **Calgary**
(`book1`, `bib`) and **Silesia** (`dickens`, `mozilla`, `xml`) corpora — see
[`fuzz/seeds/README.md`](../fuzz/seeds/README.md) for provenance and sha256s.
Only these tiny prefixes are committed; the corpora themselves are downloaded and
extracted inside a memory-capped container and removed on exit.

### Committed curated corpus

[`evidence/fuzz/`](../evidence/fuzz) additionally holds a small, curated, committed
corpus (53 files, ~86 KiB) generated deterministically by
[`examples/fuzz_seeds.rs`](../examples/fuzz_seeds.rs) (`sh scripts/run-fuzz-seeds.sh`).
Each subdirectory is the corpus for one target and puts the campaign on the
interesting paths from the first execution: valid unkeyed `.ekb` images for every
profile (`fuzz_open/`), the same images under `[key: 48-byte][AUTH]` inputs for the
authenticated path (`fuzz_open_auth/`), serialized dictionary records / record
cores / machine-9 frames (`fuzz_dictionary/`), streaming containers built with
**random chunk sizes** over every policy and block size (`fuzz_stream/`),
machine-roundtrip inputs (`fuzz_roundtrip/`) and NL2 cores (`fuzz_machine/`).
`scripts/container-fuzz.sh` copies `fuzz/seeds/*.bin` and then
`evidence/fuzz/<target>/*.bin` into `fuzz/corpus/<target>/` before the campaign
(that directory is gitignored).

### Campaign

```sh
sh scripts/container-fuzz.sh                  # bounded campaign (60 s / 100k runs per target)
EKB_RUN_FUZZ=1 sh scripts/container-fuzz.sh   # NIGHTLY: longer (600 s / 1M runs per target)
```

Each target runs `-runs=100000 -max_total_time=60` by default. The **nightly** job
is opt-in (`EKB_RUN_FUZZ=1`), which raises the per-target budget to
`-runs=1000000 -max_total_time=600` (still overridable with `EKB_FUZZ_RUNS` /
`EKB_FUZZ_SECS`). A run of the bounded campaign: **24/24 targets PASS**, no crash,
emitted by the pinned nightly (`rustlang/rust:nightly-bookworm-slim`), every
docker run memory-capped. Final libFuzzer coverage (`cov` = edges, `ft` =
feature counters, `corp` = corpus entries) for the pre-S-round 19 targets (the
five hardening targets were added later and each PASS the bounded campaign):

| target | `cov` | `ft` | `corp` | target | `cov` | `ft` | `corp` |
|---|---:|---:|---:|---|---:|---:|---:|
| `fuzz_machine` | 65 | 211 | 16 | `fuzz_pack` | 296 | 929 | 166 |
| `fuzz_open` | 1330 | 1894 | 57 | `fuzz_seg` | 577 | 2089 | 207 |
| `fuzz_bloom` | 194 | 238 | 46 | `fuzz_resource` | 250 | 630 | 76 |
| `fuzz_history_store` | 1188 | 2204 | 94 | `fuzz_rans_w` | 321 | 965 | 186 |
| `fuzz_packed` | 291 | 688 | 63 | `fuzz_rans_il` | 576 | 1518 | 246 |
| `fuzz_tiles` | 976 | 2862 | 204 | `fuzz_roundtrip` | 1546 | 4529 | 216 |
| `fuzz_blocks` | 1118 | 3998 | 413 | `fuzz_stream` | 1409 | 6375 | 385 |
| `fuzz_batch` | 338 | 1061 | 106 | `fuzz_open_auth` | 1300 | 1765 | 55 |
| `fuzz_rans_i` | 541 | 1416 | 256 | `fuzz_dictionary` | 264 | 294 | 11 |
| `fuzz_region` | 2198 | 6795 | 580 | | | | |

---

## 3. Cross-architecture determinism

```sh
sh scripts/container-aarch64.sh
```

Runs [`tests/arch_determinism.rs`](../tests/arch_determinism.rs) on native
**x86_64** and on **`--platform linux/arm64`** (QEMU) over the **same corpus**
(the committed `fuzz/seeds/*.bin` prefixes plus deterministic generated sources).
For every `(corpus, machine 1–9)` it prints a canonical line with the root body's
`physical_bytes()`, the BLAKE3 `core_hash`, and an FNV-1a hash of the decoded
bytes, plus the `auto`/`Full` plan's chosen machine and physical bytes. The script
diffs the two sorted outputs and asserts they are **byte-identical**.

**Result (PASS): identical.**

```text
amd64 lines=70  sha256=0fc33046d0bd97ceb7e22c688d1750052d3f51a4771644969e1d4b5780fcb49c
arm64 lines=70  sha256=0fc33046d0bd97ceb7e22c688d1750052d3f51a4771644969e1d4b5780fcb49c
== cross-arch determinism: identical=true ==
```

Evidence: [`evidence/bench/aarch64-determinism.json`](../evidence/bench/aarch64-determinism.json)
(`identical: true`, both output sha256s equal).

**QEMU requirement.** arm64 emulation is provided by `qemu-aarch64` via
`binfmt_misc`. If it is absent, `docker run --platform linux/arm64 …` fails with
`exec format error`; the script detects this, prints the **exact** docker error,
lists what can still be verified (the x86_64-only `run-gates.sh` / full suite),
and exits non-zero — it never fakes the comparison. To enable it (host
privileges required):

```sh
docker run --privileged --rm tonistiigi/binfmt --install arm64
```

Every docker run in the script is memory-capped; a separate `CARGO_TARGET_DIR`
(`/work/target/aarch64`) keeps the arm64 build from disturbing the amd64 tree.

---

## 4. Cross-platform CI matrix — the scalar fallback and aarch64

```sh
sh scripts/run-matrix.sh          # the CI entry point: {x86_64-simd, x86_64-scalar, aarch64}
sh scripts/run-scalar.sh scalar   # just the x86_64 scalar-fallback job
sh scripts/container-aarch64.sh   # determinism + the arm64 CI job (QEMU)
```

`run-matrix.sh` exists to make two things impossible to rot: the **scalar
(no-AVX2) fallback** and the **aarch64 build**. It runs three jobs and writes a
single `evidence/bench/ci-matrix.json`:

| job | platform | forces scalar | what it proves |
|---|---|---|---|
| `x86_64-simd` | `linux/amd64` | no | control arm: the native AVX2 dispatchers are correct |
| `x86_64-scalar` | `linux/amd64` | `EKB_NO_SIMD=1` | the scalar fallback is correct (and not-too-slow) **on AVX2 hardware** |
| `aarch64` | `linux/arm64` (QEMU) | n/a (no AVX2) | the scalar fallback and the whole crate stay correct on an AVX2-less architecture |

**The scalar switch.** [`src/simd.rs`](../src/simd.rs) exposes a documented,
performance-only runtime override: `force_scalar()` reports whether it is active
and `set_force_scalar(bool)` toggles it; the process-wide `EKB_NO_SIMD=1` (or
`EKB_FORCE_SCALAR=1`) does the same from the environment. When active, every
`*_available()` probe returns `false`, so each dispatcher (`gather_bytes`,
`gather_bytes_prefetch`, `gather_bytes_avx512`, `rans_step8_avx2`,
`rans_decode8_independent`, `nl2_windows16_avx2`) takes its byte-identical scalar
fallback. It never changes a result — `tests/simd.rs::
forced_scalar_is_byte_identical_to_scalar_reference_for_every_kernel` asserts each
kernel equals its scalar reference **and** the native result it produced before
forcing.

**What every job runs** (all inside the pinned, memory-capped containers):

* the **SIMD-vs-scalar equality** suite (`cargo test --release --test simd`),
* the **acceptance gates G1–G5** (`cargo test --release --test gates`, one gate at
a time),
* the **bounded Silesia-derived court** (`tests/ci_bench.rs`): the `auto` body over
the committed real Silesia/Calgary 4 KiB prefixes in `fuzz/seeds/`, every read
byte-checked, printing `byte_at` p50/p99/p99.9/p99.99 and the retained footprint.

That court is bounded and **offline** on purpose: it stays feasible under QEMU and
never overwrites the committed `evidence/bench/corpora.json`. The *full*
standard-corpora court remains `scripts/run-corpora.sh` (it downloads multi-MiB
corpora and is the P6 evidence), so it is not re-run here.

The `aarch64` job additionally runs the **bounded Miri subset** — the same test
set `scripts/container-miri.sh` covers — natively on arm64 (`cargo test --release
--lib -- machine:: machine_dict:: … resource::`). It runs that set with the same
three Miri-infeasible tests skipped; the x86_64 Miri run reports **73** passed.
True Miri is a bytecode interpreter: executing it under QEMU emulation did not
complete within a 13-minute budget, so it is opt-in (`EKB_ARM64_MIRI=1`) and
expected to be slow; true Miri remains an x86_64 guarantee
(`sh scripts/container-miri.sh`).

**Recorded matrix** (one run; exact values in `evidence/bench/ci-matrix.json`, and
percentiles are run-to-run noisy):

| job | status | footprint (`physical_bytes`, `E_V`) | `byte_at` p50 | p99.9 | p99.99 |
|---|---|---:|---:|---:|---:|
| `x86_64-simd` | PASS | 15,478 B, `E_V` 0.756 | 1,583 ns | 6,292 ns | 14,237 ns |
| `x86_64-scalar` | PASS | 15,478 B, `E_V` 0.756 | 1,593 ns | 5,209 ns | 34,434 ns |
| `aarch64` | PASS | 15,478 B, `E_V` 0.756 | 14,507 ns | 68,628 ns | 84,507 ns |

The footprint is **identical to the byte** across all three jobs (over the same
20,480 bytes of real Silesia/Calgary seed data) — the structural proof that forcing
the scalar path and cross-compiling to aarch64 do not change a result. The arm64
`byte_at` p50 is ~**9×** the x86_64 number (and its QEMU tail correspondingly wider),
which is QEMU emulation overhead, not the code (the `physical_bytes`/hash equality
above is the correctness claim). The committed per-job artifacts are
`ci-x86_64-simd.json` / `ci-x86_64-scalar.json` / `ci-aarch64.json` (rolled up in
`ci-matrix.json`).

**QEMU requirement / best effort.** The `aarch64` job needs `qemu-aarch64` via
`binfmt_misc`; if it is absent the job is recorded **`SKIPPED`** (not FAIL) and the
matrix still passes on x86_64. `scripts/container-aarch64.sh` exits `3` for this
case so the runner can tell "unavailable" from "failed".

---

## 5. Scale job (nightly, opt-in)

```sh
sh scripts/run-scale.sh                 # SKIPPED by default
EKB_RUN_SCALE=1 sh scripts/run-scale.sh # runs the 2^30 build
```

The **nightly** scale job runs `examples/large_history`: it streams a procedural
machine-1 core over **2^30** logical positions into 1 MiB segments — writing
exactly `4·(2^30+1)` bytes of `u32` state projection — while peak RSS stays at a
few MiB (**~2.9 MiB**, far below the 4 GiB the full projection would occupy in
RAM). It writes `evidence/bench/history-2p30.json` (committed: 4,294,967,300 state
bytes, 3,067,904 B peak RSS, 1,025 sampled checks, `status: PASS`).

The multi-GiB build is **opt-in** (`EKB_RUN_SCALE=1`) so the default reproduce
does not run it every time; without the flag the script reports `SKIPPED` and
exits 0. It runs inside the pinned container under a tight memory cap
(`EKB_SCALE_MEM`, default `4g`).

---

## 6. Acceptance gates G1–G5

See [`docs/GATES.md`](GATES.md) for the full definition and the recorded results.
Summary (`sh scripts/run-gates.sh`, all PASS):

```text
G1 invertibility       machines=1..9 cases=144 max_len=1048576
G2 prefix-composition  affine_checks=4811 byte_exact=true
G3 worst-case-bound    samples=1000000 max_measured=1048575 max_bound=1048575 violations=0 mismatches=0
G4 corruption-detection sections=18 flips=1152 detected=1152 max_reported_window=4096
G5 physical-accounting structures=46 exact_equality=18 fixed_header_relation=28
```

---

## 7. Reproduce everything

```sh
sh scripts/container-verify.sh    # GREEN: safety + fmt + clippy(-D warnings) + full suite
sh scripts/container-fuzz.sh      # 24/24 libFuzzer targets PASS
sh scripts/run-fuzz-seeds.sh      # regenerate evidence/fuzz/ (curated corpus)
sh scripts/run-hardening.sh       # hardened untrusted-input boundary; evidence/bench/hardening.json (S4)
sh scripts/run-controller.sh      # online controller vs static/TinyLFU/LRU; evidence/bench/controller.json (S5)
sh scripts/run-kv-integration.sh  # real embedded-redb KV integration; evidence/bench/kv-integration.json (S8)
sh scripts/run-faults.sh          # transactional-publication fault injection; evidence/bench/faults.json
sh scripts/run-crossfs.sh         # cross-filesystem determinism; evidence/bench/crossfs.json
sh scripts/run-gates.sh           # G1–G5 all PASS; writes evidence/gates/*.json
sh scripts/container-aarch64.sh   # x86_64 == arm64 (QEMU); writes aarch64-determinism.json + ci-aarch64.json
sh scripts/run-scalar.sh scalar   # x86_64 with EKB_NO_SIMD=1; writes ci-x86_64-scalar.json
sh scripts/run-matrix.sh          # the {x86_64-simd, x86_64-scalar, aarch64} CI matrix; writes ci-matrix.json
sh scripts/run-scale.sh           # nightly 2^30 build (opt-in: EKB_RUN_SCALE=1)
sh scripts/container-reproduce.sh # clean release build + suite + evidence validation
```

Every command runs only inside the pinned, memory-capped container, and every
`docker run` is bounded by `--memory`/`--memory-swap` (see `scripts/lib.sh`).

---

## 8. Transactional-publication fault injection

**Test:** [`tests/faults.rs`](../tests/faults.rs) · **Runner:**
[`scripts/run-faults.sh`](../scripts/run-faults.sh) · **Evidence:**
[`evidence/bench/faults.json`](../evidence/bench/faults.json)

Proves `.ekb`/stream publication is atomic under I/O faults: a **failing `Write`**
(`WriteZero`, `ENOSPC`, `EIO`) is injected after exactly `N` accepted bytes, and
the **byte-exact crash image** (the committed prefix) is injected into the
path-based writers. For every fault point the three invariants hold:

* **(a)** no partially written generation is ever readable;
* **(b)** the previous committed generation remains intact, verifies, and reads
  byte-identical;
* **(c)** `recover`/`truncate_to_valid` restore a clean state.

`StreamBuilder` is generic over `std::io::Write`, so the failing writer is injected
directly into `write`/`checkpoint`/`finish` over **every** byte offset of a
container build (header, payloads, directory, footer). The path-based writers
(`writer::write_body_*`, `writer::append_generation*`) construct their own `File`
internally and no library seam is added (runtime behaviour is frozen), so the
injection is the exact on-disk crash image a failing write leaves: for `append`
that is the final file truncated to every `N` in the appended region; for
`publish` the destination is only touched by the atomic `rename`, so the
filesystem-boundary faults (blocked temp path, rename failure, stale torn temp)
cover every point. **Result: 5,933 fault points, 100 % pass** — 5,151 stream
(incl. 2,832 in the `finish` directory/footer phase, 0 torn containers readable),
778 append (previous generation intact 778/778; 774 torn tails recovered clean),
4 publish boundaries (destination absent or the previous valid version, never torn).

---

## 9. Cross-filesystem determinism

**Test:** [`tests/crossfs.rs`](../tests/crossfs.rs) · **Runner:**
[`scripts/run-crossfs.sh`](../scripts/run-crossfs.sh) · **Evidence:**
[`evidence/bench/crossfs.json`](../evidence/bench/crossfs.json)

The same `.ekb` (root + sparse gen-2) and the same stream container (random chunk
sizes) are written on every available filesystem and reopened from each; the file
BLAKE3, `physical_bytes()`, `byte_at` samples and the container bytes must be
**identical**. Measured **3 filesystems**: the container **overlayfs** (`/tmp`), the
container **tmpfs** (`/dev/shm`) and the **ext4** bind mount
(`/work/target`; `stat -f` reports the ext family as `ext2/ext3`). All three give
`physical_bytes = 65,666`, `.ekb` BLAKE3
`5480d5bf99a6e7a5b03a18684101741e9eeab6b0f71802e1813b8d5d15adb477`, container
BLAKE3 `e9ea4ea5f1908ce22bd1c96a5c7f8ea6e51d1b26576e74e793730c19c7cdbeb6` —
`identical = true`. A loopback **xfs/btrfs** attempt is made
(`scripts/crossfs-inner.sh`): it is **blocked** because `mkfs.xfs`/`mkfs.btrfs` are
not installed and the container is offline for `apt` (and `mount` needs
`CAP_SYS_ADMIN`, i.e. `EKB_CROSSFS_PRIVILEGED=1`). The blocker is recorded verbatim
in the artifact. x86_64-vs-aarch64 determinism is covered separately by
[`scripts/container-aarch64.sh`](../scripts/container-aarch64.sh) (Q2: identical).
