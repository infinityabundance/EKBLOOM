#!/bin/sh
# Online workload-adaptation controller (src/controller.rs): a moving-hot-window
# workload where the controller additively manifests certified hot tiles, compared
# at equal budget against a static plan, a TinyLFU admission cache and an
# LZ4 codec + LRU cache. Writes evidence/bench/controller.json.
#
# Runs only inside the pinned container, memory-capped at 6 GiB (see scripts/lib.sh).
#
# Run: sh scripts/run-controller.sh
set -eu

# This harness fixes the container cap at 6 GiB (both memory and swap) before
# lib.sh resolves its defaults, so the whole run is bounded.
EKB_MEM_LIMIT=6g
EKB_MEMSWAP_LIMIT=6g
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example controller
