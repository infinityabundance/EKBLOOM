#!/bin/sh
# Batched cold-read benchmark: io_uring batch vs a serial pread loop vs the cached
# handle, across queue depths and request sizes.
#
# Writes evidence/bench/io-batch.json. Runs only inside the pinned, memory-capped
# container, relaxing exactly one policy so io_uring_setup(2) is permitted:
#
#   --security-opt seccomp=unconfined
#
# io_uring_setup(2) needs no Linux capability, so --cap-drop=ALL is retained. If
# the ring still cannot be created the example records io_uring_usable: false and
# measures the transparent pread fallback instead. Nothing runs on the host.
set -eu

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
EVIDENCE_REL="evidence/bench/io-batch.json"

docker run --rm \
    --memory="$EKB_MEM_LIMIT" \
    --memory-swap="$EKB_MEMSWAP_LIMIT" \
    --cpus="$EKB_CPUS" \
    --pids-limit="$EKB_PIDS" \
    --ulimit core=0 \
    --cap-drop=ALL \
    --security-opt=no-new-privileges \
    --security-opt seccomp=unconfined \
    -v "$PROJECT_DIR":/work \
    -w /work \
    -u "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME="$EKB_CARGO_HOME_CONTAINER" \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR="$EKB_TARGET_DIR_CONTAINER" \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    -e EKB_IO_BATCH_EVIDENCE="/work/$EVIDENCE_REL" \
    "$IMAGE" \
    cargo run --release --features io-uring --example io_batch

echo "--- $EVIDENCE_REL ---"
sed -n '1,12p' "$PROJECT_DIR/$EVIDENCE_REL" 2>/dev/null || echo "(no evidence file)"
