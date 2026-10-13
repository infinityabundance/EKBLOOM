#!/bin/sh
# Streaming-container benchmark with **bounded builder memory**.
#
# Streams a large deterministic synthetic input through `StreamBuilder` (which
# retains at most one block plus a directory, never the whole stream) and measures
# peak RSS, construction throughput, positional-read latency (`read_at` p50/p99),
# sequential-read throughput and the container footprint.
#
# Produces:
#   evidence/bench/stream.json — stream/container/payload/directory sizes,
#   physical-over-logical ratio, build and sequential throughput, `read_at`
#   p50/p99, peak RSS, and byte-exactness flags.
#
# Runs only inside the pinned, memory-capped container. The cap is set to 4 GiB
# (overriding lib.sh's 8 GiB default) so the bounded-memory property is shown under
# a tight ceiling while streaming a stream far larger than the block size.
# Override the stream size / block size with EKB_STREAM_BYTES /
# EKB_STREAM_BLOCK_BYTES (defaults: 2 GiB, 64 KiB).
set -eu
EKB_MEM_LIMIT=4g
EKB_MEMSWAP_LIMIT=4g
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example stream
