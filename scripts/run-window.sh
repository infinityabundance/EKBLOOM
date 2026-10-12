#!/bin/sh
# Bounded encoded-window read benchmark inside the pinned, memory-capped
# container.
#
# Produces:
#   evidence/bench/window.json -- for 1-byte, 4 KiB and 64 KiB file-backed reads
#   over ~4 MiB and ~64 MiB skewed objects (machines 2 and 4): encoded bytes
#   fetched before (whole-core) vs after (bounded encoded window), read-path
#   syscalls, read-path buffer allocation bytes, reconstruction steps and
#   p50/p99 latency. Set EKBLOOM_WINDOW_LARGE=1 to also run the ~1 GiB object.
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB); the 64 MiB object and
# its ~4 KiB restart index stay well within those bounds.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example window
