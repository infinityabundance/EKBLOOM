#!/bin/sh
# Workload-directed background manifestation planner (directive §4) — a
# moving-hot-window workload where the background planner promotes the new hot
# region with certified tiles. Writes evidence/bench/planner-bg.json.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh).
#
# Run: sh scripts/run-planner-bg.sh
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example planner_bg
