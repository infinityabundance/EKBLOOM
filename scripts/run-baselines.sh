#!/bin/sh
# Run the conventional direct-access baseline comparison inside the pinned,
# memory-capped container.
#
# Produces:
#   evidence/bench/baselines.json — conventional baselines (RawVec, RawFile,
#   Predecoded, IndexedArray) vs the EKBLOOM profiles Root, Path{D=4} and Full,
#   under one deterministic ~1 MiB corpus with identical seeds: real retained
#   bytes, construction time, sequential throughput, random-1b p50/p99, 4 KiB
#   random-range p50, and the retained-capabilities annotation.
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB) and CPUs; the 1 MiB
# corpus stays comfortably within those bounds and the run completes in seconds.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example baselines
