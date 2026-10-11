#!/bin/sh
# Build a 2^30-symbol segmented state projection inside the pinned, memory-capped
# container and record the bounded-memory evidence.
#
# The example writes ~4 GiB of segments to the CONTAINER's /tmp (never under the
# repository) and removes them before exiting. Evidence is small:
#   evidence/bench/history-2p30.json   build time, state bytes, peak RSS, samples
#   docs/RESULTS.md                    appended one-paragraph summary
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB). The streaming build
# keeps peak RSS to a few MiB, so the cap is never approached. The run is I/O
# bound (~4 GiB written) and may take a while.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example large_history
