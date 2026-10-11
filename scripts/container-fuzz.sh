#!/bin/sh
# Bounded libFuzzer campaign for EKBLOOM — runs ONLY inside a pinned nightly
# container, never on the host.
#
# The nightly image is published under `rustlang/rust` (the official `rust`
# repository carries no `nightly` tags on this registry). The tag order is
# `nightly-bookworm-slim` (not `nightly-slim-bookworm`). Digest resolved with
# `docker pull` + `docker inspect`; it is never fabricated:
#
#   docker pull rustlang/rust:nightly-bookworm-slim
#   docker inspect --format '{{.RepoDigests}}' rustlang/rust:nightly-bookworm-slim
#   -> rustlang/rust@sha256:2fe113dd3b9f9f36f3993f3720e9dbd16ddf9bd1365e8ef6a4b7b2dc316d13d6
#
# The `-slim` image ships a C compiler but no C++ compiler; `libfuzzer-sys`
# compiles libFuzzer's C++ sources, so `g++` is installed in-container (the
# container runs as root, as the base image does by default, because apt and the
# mounted workspace need it). cargo-fuzz is installed into a container-local
# cargo home under the gitignored `/work/target`; created artifacts are chowned
# back to the invoking host user so the tree stays clean.
#
# `RUSTUP_TOOLCHAIN` is pinned to the dated toolchain baked into the image so
# cargo does not re-sync the `nightly` channel over the network on every run.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

NIGHTLY_IMAGE="rustlang/rust:nightly-bookworm-slim@sha256:2fe113dd3b9f9f36f3993f3720e9dbd16ddf9bd1365e8ef6a4b7b2dc316d13d6"
NIGHTLY_TOOLCHAIN="nightly-2026-10-10"

# OOM / fork-bomb protection (mirrors scripts/lib.sh conventions).
EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-8g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-8g}"
EKB_CPUS="${EKB_CPUS:-8}"
EKB_PIDS="${EKB_PIDS:-2048}"

FUZZ_CARGO_HOME="/work/target/nightly-cargo-home"
FUZZ_SECS="${EKB_FUZZ_SECS:-60}"
FUZZ_RUNS="${EKB_FUZZ_RUNS:-100000}"

echo "ekbloom-fuzz: pulling pinned nightly image ($NIGHTLY_IMAGE)"
docker pull "$NIGHTLY_IMAGE" >/dev/null

echo "ekbloom-fuzz: campaign (-runs=$FUZZ_RUNS -max_total_time=$FUZZ_SECS per target)"
docker run --rm \
    --memory="$EKB_MEM_LIMIT" \
    --memory-swap="$EKB_MEMSWAP_LIMIT" \
    --cpus="$EKB_CPUS" \
    --pids-limit="$EKB_PIDS" \
    --ulimit core=0 \
    --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -w /work \
    -e HOME=/tmp \
    -e CARGO_HOME="$FUZZ_CARGO_HOME" \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e RUSTUP_TOOLCHAIN="$NIGHTLY_TOOLCHAIN" \
    -e CARGO_TERM_COLOR=never \
    -e EKB_FUZZ_RUNS="$FUZZ_RUNS" \
    -e EKB_FUZZ_SECS="$FUZZ_SECS" \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    "$NIGHTLY_IMAGE" sh -c '
        set -eu

        if ! command -v g++ >/dev/null 2>&1; then
            echo "ekbloom-fuzz: installing g++ (needed by libfuzzer-sys)"
            apt-get update -qq
            apt-get install -y -qq --no-install-recommends g++ >/dev/null
        fi

        export PATH="$CARGO_HOME/bin:$PATH"
        if [ ! -x "$CARGO_HOME/bin/cargo-fuzz" ]; then
            echo "ekbloom-fuzz: installing cargo-fuzz into the container-local home"
            cargo install cargo-fuzz --locked
        fi

        cd /work/fuzz
        failed=0
        for TARGET in fuzz_machine fuzz_open fuzz_bloom fuzz_history_store; do
            start=$(date +%s)
            echo "ekbloom-fuzz: running $TARGET"
            if cargo fuzz run "$TARGET" -- -runs="$EKB_FUZZ_RUNS" -max_total_time="$EKB_FUZZ_SECS"; then
                now=$(date +%s)
                echo "ekbloom-fuzz: PASS $TARGET ($((now - start))s)"
            else
                now=$(date +%s)
                echo "ekbloom-fuzz: FAIL $TARGET ($((now - start))s)" >&2
                failed=1
            fi
        done

        # Hand created artifacts back to the invoking host user.
        for p in /work/target/nightly-cargo-home /work/fuzz/target /work/fuzz/Cargo.lock /work/fuzz/corpus /work/fuzz/artifacts; do
            [ -e "$p" ] && chown -R "$HOST_UID:$HOST_GID" "$p" 2>/dev/null || true
        done

        if [ "$failed" -ne 0 ]; then
            echo "ekbloom-fuzz: one or more targets FAILED" >&2
            exit 1
        fi
        echo "ekbloom-fuzz: all targets PASS"
    '
