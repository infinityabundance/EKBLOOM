#!/bin/sh
# Competitive experiment: EKBLOOM region-scoped manifested regions vs an ordinary
# byte-budgeted LRU cache vs raw/no-cache — under ONE corpus, ONE access trace and
# ONE byte budget.
#
# Produces:
#   evidence/bench/competitive.json — hot/cold P50/P99, post-thrash behaviour, and
#   close-&-reopen persistence for all three candidates, plus the region-adapt
#   selection/certification summary.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh). The
# 4 MiB corpus and its 32 KiB hot prefix stay well inside the 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example competitive
