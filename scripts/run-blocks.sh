#!/bin/sh
# Machine-2 independently restartable block-index benchmark inside the pinned,
# memory-capped container.
#
# Produces:
#   evidence/bench/blocks.json — for block sizes 256 B, 1 KiB, 4 KiB, 16 KiB and
#   64 KiB on a compressed (skewed) machine-2 object: random 4 KiB read p50/p99,
#   the manifested footprint (model header + directory + restart records), the
#   amortised construction cost, and the packed checkpoint-format integration
#   (legacy vs compact bytes with backward-compatible reads).
#
# The container caps memory (EKB_MEM_LIMIT, default 8 GiB); the 4 MiB source and
# its block indexes stay well within those bounds.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example blocks
