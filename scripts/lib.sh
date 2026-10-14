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

# Resolve the project root robustly. The scripts are normally run as files
# (`sh scripts/x.sh`) or sourced *from* such a script, so `$0` names a file under
# the repository and its directory is the anchor. When `lib.sh` is instead sourced
# from an interactive shell, `$0` is the shell itself (`sh`, `bash`, ...), whose
# dirname is `.` or `/`; anchoring on that resolves the wrong root and would try to
# create `//.cargo-home`. In that case fall back to a current directory that
# actually contains `scripts/lib.sh`, and fail loudly if there is no anchor at all.
resolve_project_dir() {
    if [ -n "${0:-}" ]; then
        _ekb_anchor="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || _ekb_anchor=""
        if [ -n "$_ekb_anchor" ] && [ -f "$_ekb_anchor/lib.sh" ]; then
            (cd "$_ekb_anchor/.." && pwd)
            return 0
        fi
    fi
    if [ -f "./scripts/lib.sh" ]; then
        pwd
        return 0
    fi
    # Walk up from the current directory, so sourcing `lib.sh` from a
    # subdirectory still anchors on the repository root.
    _ekb_dir="$(pwd)"
    while [ "$_ekb_dir" != "/" ] && [ -n "$_ekb_dir" ]; do
        if [ -f "$_ekb_dir/scripts/lib.sh" ]; then
            printf '%s\n' "$_ekb_dir"
            return 0
        fi
        _ekb_dir="$(dirname "$_ekb_dir")"
    done
    return 1
}

PROJECT_DIR="$(resolve_project_dir)" || {
    echo "ekbloom: cannot locate the project root; run the scripts from the repository root." >&2
    exit 1
}

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
