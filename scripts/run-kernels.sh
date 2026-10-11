#!/bin/sh
# Reconstruction kernels (directive §6) — Kernel A (short restart windows) and
# Kernel B (independent long blocks), measured at equal residency with the court
# harness. Writes evidence/bench/kernels.json.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh).
#
# Run: sh scripts/run-kernels.sh
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example kernels
