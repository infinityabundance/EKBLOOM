#!/bin/sh
# Run the manifestation sweep inside the pinned, memory-capped container.
#
# Produces:
#   evidence/bench/sweep.json        classic speed-vs-manifested-space frontier
#                                    over a deterministic 2 MiB corpus,
#   evidence/bench/sweep-large.json  extended sweep over a deterministic 4 MiB
#                                    corpus: the budget frontier, region-size /
#                                    depth / checkpoint-interval curves, cold
#                                    starts and cache-resident throughput
#                                    (the cache-resident corpus is ~64 KiB),
#   evidence/bench/cpu.json          CPU features + source provenance.
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB) and CPUs; the 4 MiB and
# 64 KiB corpora stay comfortably within those bounds. The default run completes
# in tens of seconds.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example sweep
