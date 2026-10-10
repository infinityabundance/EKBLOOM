#!/bin/sh
# Run the manifestation sweep inside the pinned, memory-capped container.
#
# Produces evidence/bench/sweep.json (the measured speed-vs-manifested-space
# frontier) and evidence/bench/cpu.json (CPU features + source provenance).
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example sweep
