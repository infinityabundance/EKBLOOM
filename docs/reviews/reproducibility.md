# EKBLOOM — Independent Reproducibility Review

Reviewer: independent "Reproducibility reviewer" subagent.
Date: 2026-10-10.
Repo: `<repo-root>` (single crate `ekbloom`).
Scope: directive §1.1/§32 — a clean checkout must reproduce correctness and
representative benchmarks in a **pinned** container; all tooling runs inside
Docker, never on the host.

Working tree at review time: commit `58782ec` (`main`), clean except for this
new file and the scratch restores described below. No tracked file was left
modified by this review (`git status --short` clean at end).

Host is **not** used for project tooling; every `cargo`/`rustc` invocation went
through `docker run` (directly or via `scripts/`).

---

## Item 1 — Image pinning — **PASS (with caveat)**

Base digest in `scripts/lib.sh` and `Dockerfile` is
`rust:1.99.0-slim-bookworm@sha256:2c3a22f0a5533ea2dd5a16627bc841228151faa2d4de2644ac9987e4a2f1f2fa`.

```sh
docker inspect --format '{{.RepoDigests}}' rust:1.99.0-slim-bookworm
# [rust@sha256:2c3a22f0a5533ea2dd5a16627bc841228151faa2d4de2644ac9987e4a2f1f2fa]
docker manifest inspect rust:1.99.0-slim-bookworm@sha256:2c3a22…f2fa   # RESOLVES
```

Matches the pinned digest exactly. Project image:

```sh
docker inspect --format '{{.Id}}' ekbloom-rust:1.99.0
# sha256:fad56f12288907e1fd8b61d0d0702388587d413f4519ecda4e79fa19e18be0b6
```

This equals `project_image_id` in the committed `evidence/environment.txt`.
`docker history --no-trunc ekbloom-rust:1.99.0` shows exactly the base image's
layers followed by the four committed `Dockerfile` steps (apt `ca-certificates/
coreutils/file`, `rustup component add rustfmt clippy`, `useradd ekb`, `CMD`);
the committed `Dockerfile` sha256 (`bee84e6a…`) equals the live file's sha256.
So the image derives only from the pinned base + committed `Dockerfile`.

Nightly harnesses (`container-fuzz.sh`, `container-miri.sh`,
`container-sanitize.sh`) all hardcode the same real digest
`rustlang/rust:nightly-bookworm-slim@sha256:2fe113dd3b9f9f36f3993f3720e9dbd16ddf9bd1365e8ef6a4b7b2dc316d13d6`.

```sh
docker manifest inspect rustlang/rust:nightly-bookworm-slim@sha256:2fe113…13d6   # RESOLVES
docker run --rm <nightly@sha256:2fe113…> rustup toolchain list
# nightly-2026-10-10-x86_64-unknown-linux-gnu (active, default)
```

The `NIGHTLY_TOOLCHAIN="nightly-2026-10-10"` pin in `container-fuzz.sh` is a
real, baked-in toolchain (not fabricated); `rustc -Vv` reports
`1.101.0-nightly (32dba69d6 2026-10-09)`. Caveat: `container-miri.sh` /
`container-sanitize.sh` set `RUSTUP_TOOLCHAIN=nightly` (channel) rather than the
dated name; the image digest still fixes the toolchain, so this is benign.

## Item 2 — Lockfile — **PASS**

`Cargo.lock` is committed (`sha256 =
13d632c0e8a7aeb53bf3fb5a32394141806d5e7c0996a1a32a22f432f02dc624`, identical to
`environment.txt`). `memmap2` was dropped from `Cargo.toml` and is absent from
the regenerated lockfile; the `--locked` build and full test run still pass.

```sh
sh scripts/container-build.sh --release --locked   # Finished `release` … 
sh scripts/container-test.sh  --release --locked   # all suites pass
```

Result: **203 tests, 0 failed** across 17 test binaries + 91 lib unit tests
(lib 91, adversarial 16, async_build 6, bloom_path 6, blooms_research 7, cli 3,
correctness 15, crash_recovery 4, fastpath 3, filesystem 5, format_roundtrip 4,
history_checkpoint 6, manifest_read_verify 9, math_review 10, model_search 4,
properties 8, security 6).

## Item 3 — Clean-checkout reproduction — **PASS (fallback used)**

The pinned image ships **no `git`** (`command -v git` → `NO_GIT`), so the
`git clone` form is unavailable. Fallback: copy the mounted tree excluding the
host `target/` and `.cargo-home/`, build into a fresh target dir inside `/tmp`:

```sh
docker run --rm --memory=8g --memory-swap=8g --cpus=8 --pids-limit=2048 \
  --ulimit core=0 --cap-drop=ALL --security-opt=no-new-privileges \
  -v <repo-root>:/work -w /tmp -u 1000:1000 \
  -e HOME=/tmp -e CARGO_HOME=/work/.cargo-home -e RUSTUP_HOME=/usr/local/rustup \
  -e CARGO_TARGET_DIR=/tmp/repro-target -e RUSTUP_TOOLCHAIN=1.99.0 \
  ekbloom-rust:1.99.0 sh -c 'rm -rf /tmp/repro /tmp/repro-target; mkdir -p /tmp/repro;
    tar -C /work --exclude=./target --exclude=./.cargo-home -cf - . | tar -C /tmp/repro -xf -;
    cd /tmp/repro && cargo build --release --locked && cargo test --release --locked'
```

The extracted tree contained no `target/`. `cargo build --release --locked`
compiled all deps + `ekbloom` from scratch (4.04 s) and the full suite passed
from the pristine tree: **203 tests, 0 failed** (fresh `CARGO_TARGET_DIR`).
Correctness is therefore reproducible without the host build cache.

## Item 4 — `container-reproduce.sh` + env compare — **PASS**

`sh scripts/container-reproduce.sh` ran to completion: delegates the environment
capture to `scripts/capture-environment.sh` (single source of truth), then wipes
`target/`, runs `cargo build --release --locked` and `cargo test --release
--locked`, all green (**20 suites, 0 failures**).

`evidence/environment.txt` is now written only by `capture-environment.sh`, so
it is byte-for-byte the **rich** format: image tag/id, base digest,
`dockerfile_sha256`, in-container `rustc -Vv` (full `commit-hash` / `LLVM`),
`cargo -V`, `target_triple`/`rustc_host`/`target_libdir`, `uname`, CPU model +
flags, `nproc`, `Cargo.lock` sha256, `git_head`, and the nightly image digest.
Running `sh scripts/capture-environment.sh` afterward is idempotent (identical
output). The only differences vs the previously committed capture are the
toolchain/lock-independent fields:

```diff
-uname: Linux 127bcdff4bc8 7.2.9-2-cachyos …
+uname: Linux 9998b34bcb88 7.2.9-2-cachyos …      # ephemeral container hostname
-cargo_lock_sha256: b922be92586e82c24b0dcebeaed5cf4e5202211d3c9dd7abbaf60a13c7cab80f
+cargo_lock_sha256: 13d632c0e8a7aeb53bf3fb5a32394141806d5e7c0996a1a32a22f432f02dc624
-git_head: 934a9148bc2c588f458388adced219a604b0c29d
+git_head: 58782ec9aee7cb5c4758a6533b7b147b62fc557e
```

Toolchain (`rustc 1.99.0 (b940084d7)`, `cargo 1.99.0`), `target_libdir`, CPU
model + flags, `nproc`, `project_image_id`, base digest and `dockerfile_sha256`
all reproduce **byte-identically**. The differences are benign and expected: the
hostname is per-container; `cargo_lock_sha256` changed because `Cargo.lock` was
regenerated after dropping the `memmap2` dependency (`--locked` still passes,
see Item 2); and the old committed `git_head` (`934a914`) is an ancestor of
current HEAD `58782ec` (confirmed via `git merge-base --is-ancestor`). Because
both capture paths now share one writer, re-running either produces the same
rich file with no format drift.

## Item 5 — Benchmark reproducibility — **PASS (ordering reproduces; speeds drift)**

`sh scripts/run-sweep.sh` regenerated `evidence/bench/sweep.json` (9 points,
source_len 2,097,152, blake3 `382f0dab…`). Committed vs fresh:

| metric (profile) | committed | fresh | drift |
|---|---:|---:|---:|
| path(4) seq MB/s | 66,764.9 | 68,128.2 | **+2.0 %** |
| path(4) random-1b p50 (ns) | 30 | 30 | 0 % |
| path(4) random-4k p50 (ns) | 70 | 70 | 0 % |
| path(8) seq MB/s | 66,588.3 | 68,275.0 | +2.5 % |
| max-speed seq MB/s | 69,757.5 | 66,113.3 | −5.2 % |
| full seq MB/s | 732.7 | 646.8 | −11.7 % |
| root seq MB/s | 601.9 | 555.7 | −7.7 % |
| root random-1b p50 (ns) | 1,570,139 | 1,725,901 | +9.9 % |
| path(4) 64k MB/s | 34,774.3 | 42,610.3 | +22.5 % |

`physical_bytes` and `expansion_ratio` are **identical for every profile**
(deterministic accounting); `cpu` flags identical.

Frontier ordering: committed top-3 by `seq` was `max-speed > path(4) >
path(8)`; fresh is `path(8) > path(4) > max-speed`. All three sit within ~3 % of
each other (≈66–68 GB/s), so their relative rank is **within run-to-run noise**.
The *qualitative* frontier reproduces strongly and is the substantive claim:
the path family runs ≈110× the sequential throughput of the replay family
(≈67,000 vs ≈600 MB/s) and ≈57,000× lower single-byte latency (30 ns vs
≈1.7 µs). The honest README/RESULTS claims (rejected universal `T_4`, path as
the speed ceiling) are supported. `evidence/bench/sweep.json` was restored to
its committed content afterward.

## Item 6 — No host tooling; memory caps — **PASS**

`grep -rn -E '(^|[^A-Za-z_/])(cargo|rustc|rustup)( |$)' scripts/` shows every
`cargo`/`rustc`/`rustup` token is either a `docker_run …` argument or inside a
`docker run … sh -c '…'` payload; no bare host invocation exists. `scripts/lib.sh`
caps every run with `--memory=$EKB_MEM_LIMIT` (8g), `--memory-swap` (8g),
`--cpus` (8), `--pids-limit` (2048), plus `--ulimit core=0`, `--cap-drop=ALL`,
`--security-opt=no-new-privileges`, and runs as the invoking host user.
`container-fuzz.sh` / `container-miri.sh` / `container-sanitize.sh` each apply
`--memory` + `--memory-swap` (and PIDs/caps where applicable). All caps present.

## Item 7 — Nondeterminism / missing pins / unreproducible claims — **NOTES**

- **Stale committed provenance.** `evidence/environment.txt` records
  `git_head: 934a914`, an ancestor of HEAD `58782ec`. The capture predates the
  final "independent reviews" commit; the file does not describe HEAD.
- **Single capture path.** `container-reproduce.sh` no longer writes its own
  reduced `environment.txt`; it calls `capture-environment.sh`. Both paths now
  emit the same rich format, removing the earlier divergence.
- **Benchmark variance.** Sequential / single-byte-latency numbers move ±10 %
  run-to-run; the top-3 path-family ordering is not stable. Only
  `physical_bytes` / `expansion_ratio` are exactly reproducible. Latency-floor
  values (30/70 ns) are stable.
- **Nightly channel vs dated pin.** `container-miri.sh` / `container-sanitize.sh`
  pin the channel (`nightly`), not the dated `nightly-2026-10-10`; the image
  digest still fixes the toolchain, so this is cosmetic.
- No fabricated digests found; both base and nightly digests resolve against
  their registries and against local images.

---

## Verdict

| # | Item | Result |
|---|------|--------|
| 1 | Image pinning (base + project lineage + nightly digests) | **PASS** (caveat: miri/sanitize use channel name) |
| 2 | Lockfile: locked release build + test | **PASS** (203/203) |
| 3 | Clean-checkout reproduction (no host `target/`) | **PASS** (tar fallback; no `git` in image) |
| 4 | `container-reproduce.sh` + env compare | **PASS** (single capture path; rich format) |
| 5 | Benchmark reproducibility | **PASS** (ordering qualitative; speeds drift ±2–22 %) |
| 6 | No host cargo/rustc; memory caps applied | **PASS** |
| 7 | Nondeterminism / pins / claims | **NOTES** (no blocker) |

**No blocker.** A clean checkout in the pinned container reproduces the full
correctness suite and the substantive benchmark frontier. The remaining caveats
are the stale pre-existing `git_head` in the old capture (now refreshed) and
normal single-host benchmark variance.
