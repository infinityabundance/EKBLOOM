#!/bin/sh
# Shared-dictionary small-record benchmark inside the pinned, memory-capped
# container.
#
# Produces:
#   evidence/bench/dictionary.json -- on a corpus of many ~100-byte text-like
#   records, the TOTAL footprint (the shared dictionary amortised) and
#   bytes/record for (a) per-record machine 2, (b) the dictionary machine 9 (one
#   shared model + compact per-record cores), (c) raw records, (d) zstd with a
#   trained dictionary and (e) plain per-record zstd; the record count at which
#   the shared dictionary breaks even against per-record machine 2; a record-size
#   sweep; and per-record decode latencies.
#
# Runs only inside the pinned container (see scripts/lib.sh). Memory is capped at
# 4 GiB for this run.
set -eu

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-4g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-4g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example dictionary
