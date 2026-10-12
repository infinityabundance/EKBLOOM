#!/bin/sh
# Batched independent reconstruction (docs/GPU.md) — the CPU-threads backend over a
# large batch of independent machine-1 restart windows, measured for throughput
# (windows/s, GB/s) and scaling versus single-thread. Writes
# evidence/bench/batch.json. Runs only inside the pinned, memory-capped container
# (see scripts/lib.sh).
#
# Run: sh scripts/run-batch.sh
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example batch
