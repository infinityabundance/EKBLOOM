#!/bin/sh
# Persistent verified handles, concurrent readers under publication, and persisted
# tile reads — measured inside the pinned, memory-capped container.
#
# Produces:
#   evidence/bench/handles.json — (a) cached-handle read vs reopen + reparse per
#   request (latency per read for 1-byte and 4 KiB reads, plus the ObjectCache
#   hit path); (b) concurrent readers at 1/2/4/8/16 threads while a background
#   thread publishes new generations (throughput and p50/p99/p99.9 with and
#   without background construction); (c) a persisted hot tile range served from
#   an `.ekb` through `FileBody` by bounded pread vs the in-memory tile set.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh). The
# 4 MiB corpus and its persisted tile set stay well inside the 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example handles
