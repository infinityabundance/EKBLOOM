#!/bin/sh
# Shared container settings for EKBLOOM. Sourced by the other scripts.
#
# Hard rule: project tooling runs ONLY inside this container, never on the host.
set -eu

# Base image: official Docker Hub Rust slim image, pinned by exact digest.
RUST_BASE_IMAGE="rust:1.99.0-slim-bookworm@sha256:2c3a22f0a5533ea2dd5a16627bc841228151faa2d4de2644ac9987e4a2f1f2fa"
# Project image derived from ./Dockerfile: the pinned base plus rustfmt+clippy.
# Content identity is fixed by (base digest ++ Dockerfile bytes); the resulting
# local image id is recorded at build time in evidence/environment.txt.
IMAGE="ekbloom-rust:1.99.0"

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# OOM protection: cap memory (and swap) so a runaway allocation cannot exhaust
# the host. 8 GiB is comfortably above peak for the full harness while bounded.
EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-8g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-8g}"
EKB_CPUS="${EKB_CPUS:-8}"
EKB_PIDS="${EKB_PIDS:-2048}"

# Cargo caches live under the mounted project (gitignored) so they persist
# between runs without polluting anything outside the repository. These must be
# *container* paths (inside the /work bind mount), never host absolute paths.
EKB_CARGO_HOME_CONTAINER="/work/.cargo-home"
EKB_TARGET_DIR_CONTAINER="/work/target"

mkdir -p "$PROJECT_DIR/.cargo-home" "$PROJECT_DIR/target"

# Fail fast if the project image is missing, with a clear build instruction.
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ekbloom: project image '$IMAGE' not found." >&2
    echo "Build it with: docker build -t $IMAGE -f Dockerfile ." >&2
    exit 1
fi

# docker_run: run a command in the pinned image as the invoking host user.
#   * memory/swap capped (OOM protection)
#   * pid cap (fork-bomb protection)
#   * no privileged mode
#   * source tree bind-mounted read-write
docker_run() {
    docker run --rm \
        --memory="$EKB_MEM_LIMIT" \
        --memory-swap="$EKB_MEMSWAP_LIMIT" \
        --cpus="$EKB_CPUS" \
        --pids-limit="$EKB_PIDS" \
        --ulimit core=0 \
        --cap-drop=ALL \
        --security-opt=no-new-privileges \
        -v "$PROJECT_DIR":/work \
        -w /work \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME="$EKB_CARGO_HOME_CONTAINER" \
        -e RUSTUP_HOME=/usr/local/rustup \
        -e CARGO_TARGET_DIR="$EKB_TARGET_DIR_CONTAINER" \
        -e CARGO_TERM_COLOR=never \
        -e RUSTUP_TOOLCHAIN=1.99.0 \
        "$IMAGE" "$@"
}

# docker_run_fg: like docker_run but keeps stdin/tty for interactive use.
docker_run_fg() {
    docker run --rm -it \
        --memory="$EKB_MEM_LIMIT" \
        --memory-swap="$EKB_MEMSWAP_LIMIT" \
        --cpus="$EKB_CPUS" \
        --pids-limit="$EKB_PIDS" \
        -v "$PROJECT_DIR":/work \
        -w /work \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME="$EKB_CARGO_HOME_CONTAINER" \
        -e RUSTUP_HOME=/usr/local/rustup \
        -e CARGO_TARGET_DIR="$EKB_TARGET_DIR_CONTAINER" \
        -e CARGO_TERM_COLOR=never \
        "$IMAGE" "$@"
}
