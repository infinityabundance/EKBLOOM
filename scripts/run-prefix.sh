#!/bin/sh
# Run the parallel-prefix reconstruction benchmark inside the pinned,
# memory-capped container.
#
# Produces:
#   evidence/bench/prefix.json -- block sizes {64,256,1024,4096,16384} x
#   strategies {scalar_recurrence, blockwise_scalar, parallel threads
#   1,2,4,8,16}: throughput (GB/s), estimated cycles/byte, ns per block, peak RSS
#   and the byte-exactness oracle vs NL2::decode; the separate block-summary and
#   prefix-scan construction cost (ns/block); the short-input two-pass-vs-scalar
#   comparison; and state_at_via_prefix correctness/speed vs replay-from-genesis.
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB) and CPUs; the 4 MiB
# corpus stays comfortably within those bounds.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example prefix
