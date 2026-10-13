# EKBLOOM — trust-building test infrastructure

**Runners:** [`scripts/container-verify.sh`](../scripts/container-verify.sh) ·
[`scripts/container-fuzz.sh`](../scripts/container-fuzz.sh) ·
[`scripts/container-aarch64.sh`](../scripts/container-aarch64.sh) ·
[`scripts/run-gates.sh`](../scripts/run-gates.sh) ·
[`scripts/run-scale.sh`](../scripts/run-scale.sh) ·
[`scripts/container-reproduce.sh`](../scripts/container-reproduce.sh)
**Gates:** [`docs/GATES.md`](GATES.md) · **Seeds:** [`fuzz/seeds/`](../fuzz/seeds)

This document describes the four layers of EKBLOOM's trust-building test
infrastructure: **fuzz roundtrips**, **cross-architecture determinism**, the
**scale job**, and the **acceptance gates G1–G5**. Every number below is from a
real run inside the pinned, memory-capped container; no project tooling ever runs
on the host.

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
| `cargo test --all-features` (debug) | **770** | 0 |
| `cargo test --release --locked` (reproduce) | **756** | 0 |

(The release run has fewer tests because it does not enable the optional
`fuse`/`io-uring` features.) New in this work: `tests/gates.rs` (G1–G5) and
`tests/arch_determinism.rs`.

---

## 2. Fuzz roundtrips

Two new libFuzzer targets join the existing 15 (17 total):

| target | property |
|---|---|
| `fuzz_roundtrip` ([`fuzz_targets/fuzz_roundtrip.rs`](../fuzz/fuzz_targets/fuzz_roundtrip.rs)) | For arbitrary input, `encode` then `decode` across **all** machines (ids 1–9); when `encode` succeeds, `decode` must succeed and equal the source byte-for-byte, and a shorter requested length must return the source prefix. |
| `fuzz_stream` ([`fuzz_targets/fuzz_stream.rs`](../fuzz/fuzz_targets/fuzz_stream.rs)) | Arbitrary bytes into the streaming reader are total (never panic, incl. `u64::MAX`); a built `StreamBuilder` container must round-trip exactly (sequential + sampled positional reads). |

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
`scripts/container-fuzz.sh` copies the seeds into `fuzz/corpus/<target>/` before
the campaign (that directory is gitignored).

### Campaign

```sh
sh scripts/container-fuzz.sh    # pulls the pinned nightly, runs every target
```

Each target runs `-runs=100000 -max_total_time=60`. All **17/17 targets PASS**
(bounded libFuzzer campaign, no crashes). Final libFuzzer coverage for the two new
targets:

| target | coverage (`cov`) | features (`ft`) | corpus | exec/s | result |
|---|---:|---:|---:|---:|---|
| `fuzz_roundtrip` | 1548 | 4449 | 213 | 390 | PASS (61 s) |
| `fuzz_stream` | 1066 | 5845 | 380 | 491 | PASS (62 s) |

(`cov` = edges, `ft` = feature counters, at `-runs=100000 -max_total_time=60`.)

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

## 4. Scale job (nightly, opt-in)

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

## 5. Acceptance gates G1–G5

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

## 6. Reproduce everything

```sh
sh scripts/container-verify.sh    # GREEN: safety + fmt + clippy(-D warnings) + full suite
sh scripts/container-fuzz.sh      # 17/17 libFuzzer targets PASS
sh scripts/run-gates.sh           # G1–G5 all PASS; writes evidence/gates/*.json
sh scripts/container-aarch64.sh   # x86_64 == arm64 (QEMU); writes aarch64-determinism.json
sh scripts/run-scale.sh           # nightly 2^30 build (opt-in: EKB_RUN_SCALE=1)
sh scripts/container-reproduce.sh # clean release build + suite + evidence validation
```

Every command runs only inside the pinned, memory-capped container, and every
`docker run` is bounded by `--memory`/`--memory-swap` (see `scripts/lib.sh`).
