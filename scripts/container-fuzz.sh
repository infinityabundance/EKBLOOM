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

# Nightly opt-in: a longer campaign. `EKB_RUN_FUZZ=1` raises the per-target budget
# (still overridable by EKB_FUZZ_SECS / EKB_FUZZ_RUNS).
if [ "${EKB_RUN_FUZZ:-0}" = "1" ]; then
    FUZZ_SECS="${EKB_FUZZ_SECS:-600}"
    FUZZ_RUNS="${EKB_FUZZ_RUNS:-1000000}"
    echo "ekbloom-fuzz: NIGHTLY mode (-runs=$FUZZ_RUNS -max_total_time=$FUZZ_SECS per target)"
fi

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

        # Seed the corpus from the committed small real samples (Silesia/Calgary
        # 4 KiB prefixes; see fuzz/seeds/README.md) so the campaign starts from
        # valid, structured inputs. The corpus directory is gitignored.
        if [ -d /work/fuzz/seeds ]; then
            for TARGET in fuzz_roundtrip fuzz_stream; do
                mkdir -p "corpus/$TARGET"
                cp /work/fuzz/seeds/*.bin "corpus/$TARGET/" 2>/dev/null || true
            done
        fi

        # The curated, committed corpus (evidence/fuzz/<target>/*.bin): valid `.ekb`
        # images, AUTH images, dictionary records, and stream containers built with
        # random chunk sizes. See evidence/fuzz/README.md.
        if [ -d /work/evidence/fuzz ]; then
            for seed_dir in /work/evidence/fuzz/*/; do
                [ -d "$seed_dir" ] || continue
                SEED_TARGET=$(basename "$seed_dir")
                mkdir -p "corpus/$SEED_TARGET"
                cp "$seed_dir"*.bin "corpus/$SEED_TARGET/" 2>/dev/null || true
            done
        fi

        failed=0
        # Every target declared in fuzz/Cargo.toml. Keep this list in sync with
        # that manifest so the campaign covers the whole fuzz surface.
        for TARGET in fuzz_machine fuzz_open fuzz_bloom fuzz_history_store fuzz_packed fuzz_tiles fuzz_blocks fuzz_batch fuzz_rans_i fuzz_region fuzz_pack fuzz_seg fuzz_resource fuzz_rans_w fuzz_rans_il fuzz_roundtrip fuzz_stream fuzz_open_auth fuzz_dictionary; do
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
