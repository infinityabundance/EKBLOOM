#!/bin/sh
# Machine 2 (`OrANS-RANS0`) prepared-context decode benchmark inside the pinned,
# memory-capped container.
#
# Produces:
#   evidence/bench/rans.json -- prepared-context decode vs per-symbol context
#   rebuild (the pre-fix behaviour): ns, GB/s, speedup, context-build cost, and
#   byte-exactness, for a skewed (V < N) and a uniform (V > N) corpus.
#
# The 512 KiB corpora stay well inside the container's 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example rans
