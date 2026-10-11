#!/bin/sh
# Selective hot-region byte tiles vs whole-object P_32 vs regional transition
# tables vs raw baselines — under one corpus, one access trace and one byte
# budget.
#
# Produces:
#   evidence/bench/tiles.json — hot/cold p50/p99, added physical bytes, directory
#   bytes and "hot-read bytes per added manifestation byte" for: EKBLOOM
#   selective 1/4/16/64 KiB tiles (Flat and TwoLevelRadix directories), the
#   existing whole-object P_32 output surface, the previous regional transition
#   table, and raw-vec / raw-file baselines.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh). The
# 4 MiB corpus stays well inside the 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example tiles
