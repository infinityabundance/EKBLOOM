#!/bin/sh
# **Repeated, interleaved multirun court** — distributions and 95% CIs.
#
# Runs `examples/multirun.rs` inside the pinned, memory-capped container. The
# harness measures every contender over `EKB_MULTIRUN_REPS` fresh repetitions in a
# seed-randomised interleaved order, sweeps 1/2/4/8/16/32/64 concurrent readers
# against a publishing writer, and writes:
#
#   evidence/bench/multirun.json         — the distributions + CIs
#   evidence/bench/config-manifest.json  — the full configuration manifest
#
# The corpora are the container's own real text/ELF files (bounded by
# `EKB_MULTIRUN_CAP`, default 1 MiB); nothing large is downloaded or committed.
#
# Hard rule: all project tooling runs inside the container, never on the host, and
# every `docker run` is memory-capped (6 GiB) and CPU-capped.
#
# Run: sh scripts/run-multirun.sh
set -eu

# Cap every container (including the page-size probe) at 6 GiB, per the hard rule.
EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-6g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-6g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT
. "$(dirname "$0")/lib.sh"

MEM="6g"
CPUS="${EKB_MULTIRUN_CONTAINER_CPUS:-8}"
PIDS="${EKB_MULTIRUN_PIDS:-4096}"
REPS="${EKB_MULTIRUN_REPS:-15}"
OUT="$PROJECT_DIR/evidence/bench"
mkdir -p "$OUT"

# --- provenance the container cannot see for itself -----------------------------
IMAGE_ID="$(docker image inspect --format '{{.Id}}' "$IMAGE")"
DOCKERFILE_SHA="$(sha256sum "$PROJECT_DIR/Dockerfile" | cut -d' ' -f1)"
BASE_IMAGE="$(sed -n 's/^RUST_BASE_IMAGE="\([^"]*\)".*/\1/p' "$PROJECT_DIR/scripts/lib.sh")"

# Page size, probed inside the container (the host page size may differ).
PAGE="$(docker_run getconf PAGESIZE 2>/dev/null | tr -d '[:space:]' || true)"
[ -n "$PAGE" ] || PAGE=unknown

# The exact command lines, recorded verbatim in the manifest.
INNER_CMD="cargo run --release --example multirun"
HOST_CMD="docker run --rm --memory=$MEM --memory-swap=$MEM --cpus=$CPUS --pids-limit=$PIDS --ulimit core=0 --cap-drop=ALL --security-opt=no-new-privileges -v $PROJECT_DIR:/work -w /work -u $(id -u):$(id -g) -e HOME=/tmp -e CARGO_HOME=/work/.cargo-home -e RUSTUP_HOME=/usr/local/rustup -e CARGO_TARGET_DIR=/work/target -e CARGO_TERM_COLOR=never -e RUSTUP_TOOLCHAIN=1.99.0 -e EKB_PAGE_SIZE=$PAGE $IMAGE $INNER_CMD"

echo "== multirun: reps=$REPS, image $IMAGE ($IMAGE_ID), page $PAGE, mem $MEM, cpus $CPUS =="

# Optionally pin with taskset (default 0..CPUS-1) so the numbers are not smeared
# across the host's CPUs. `EKB_MULTIRUN_TASKSET=''` disables pinning.
TASKSET="${EKB_MULTIRUN_TASKSET-0-$((CPUS - 1))}"
if [ -n "$TASKSET" ] && docker_run sh -c 'command -v taskset >/dev/null 2>&1'; then
    RUN_CMD="taskset -c $TASKSET $INNER_CMD"
    MANIFEST_TASKSET="$TASKSET"
else
    RUN_CMD="$INNER_CMD"
    MANIFEST_TASKSET="none"
fi

docker run --rm \
    --memory="$MEM" --memory-swap="$MEM" --cpus="$CPUS" \
    --pids-limit="$PIDS" --ulimit core=0 \
    --cap-drop=ALL --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -w /work -u "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME=/work/.cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR=/work/target \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    -e EKB_PAGE_SIZE="$PAGE" \
    -e EKB_IMAGE_REF="$IMAGE" \
    -e EKB_IMAGE_ID="$IMAGE_ID" \
    -e EKB_BASE_IMAGE="$BASE_IMAGE" \
    -e EKB_DOCKERFILE_SHA256="$DOCKERFILE_SHA" \
    -e EKB_CONTAINER_MEMORY="$MEM" \
    -e EKB_CONTAINER_CPUS="$CPUS" \
    -e EKB_COMMAND_LINES="$HOST_CMD
pinned: $MANIFEST_TASKSET
in-container: $RUN_CMD" \
    -e EKB_MULTIRUN_REPS="$REPS" \
    -e EKB_MULTIRUN_CAP="${EKB_MULTIRUN_CAP:-}" \
    -e EKB_MULTIRUN_SEED="${EKB_MULTIRUN_SEED:-}" \
    -e EKB_MULTIRUN_CONC_REPS="${EKB_MULTIRUN_CONC_REPS:-}" \
    "$IMAGE" sh -c "$RUN_CMD"

echo "--- evidence/bench/multirun.json (head) ---"
sed -n '1,6p' "$OUT/multirun.json" 2>/dev/null || echo "(no evidence file)"
echo "--- evidence/bench/config-manifest.json (head) ---"
sed -n '1,8p' "$OUT/config-manifest.json" 2>/dev/null || echo "(no evidence file)"
