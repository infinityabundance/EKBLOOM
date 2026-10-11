#!/bin/sh
# Run the packed 16-bit restart/history benchmark inside the pinned,
# memory-capped container.
#
# Produces:
#   evidence/bench/packed.json -- for strides K in {8,16,32,64,128,256} and corpus
#   sizes {256 KiB, 1 MiB, 4 MiB}: physical bytes of the packed restart set
#   (2*count) vs the existing 12-byte checkpoints (12*count) and of the packed
#   history (2*(N+1)) vs the 4-byte history (4*(N+1)); random-position p50 latency
#   of a packed restart + bounded replay vs the existing checkpoint restore and
#   replay-from-genesis; a locality note (dense restarts within a per-position
#   budget); and `state_from_hot` correctness + per-call cost.
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB); the 4 MiB corpus and
# its packed forms stay well within those bounds.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example packed
