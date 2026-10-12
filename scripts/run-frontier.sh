#!/bin/sh
# Competitive frontier: the unified bounded-access engine vs established indexed
# codecs (block-indexed LZ4, chunked/seekable Zstandard — each with no cache, a
# conventional LRU hot-block cache, and a PINNED-HOT set decompressed once and
# retained permanently) and raw storage, under one equal total physical budget per
# corpus.
#
# Produces:
#   evidence/bench/frontier.json — per corpus (skewed, mixed, uniform, 4 MiB each):
#   retained footprint, construction time, exact random 4 KiB read p50/p99 (hot),
#   cold p50/p99, and the post-scan ("thrashed") hot p50/p99 that shows the
#   non-eviction property of manifested structures.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh). The
# three 4 MiB corpora stay well inside the 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example frontier
