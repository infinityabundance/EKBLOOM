#!/bin/sh
# Physical-budget-aware JOINT planner: joint vs greedy/fixed-profile vs machine
# selection alone, at two equal budgets on a heterogeneous corpus (skewed +
# uniform + one hot region). Writes evidence/bench/optimizer.json.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh). The
# 4 MiB corpus stays well inside the 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example optimizer
