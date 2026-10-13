#!/bin/sh
# RocksDB-style skewed KV access court inside the pinned, memory-capped container.
#
# Builds a deterministic dataset of 100-byte values with an 80/20 key skew and
# compares, at an equal on-disk budget (the raw size), EKBLOOM (the best plan the
# joint optimiser admits, and machine 9 `OrANS-DICT` via a trained dictionary)
# against raw, block-indexed LZ4, seekable Zstandard, and Zstandard with a
# trained dictionary (the same training set). It reports the retained footprint,
# the bytes fetched per read (I/O amplification), the get / 4 KiB p50/p99/p99.9,
# the hot-set and cold get latencies, and decode throughput.
#
# Produces:
#   evidence/bench/rocksdb-skew.json
#
# Runs only inside the pinned container (scripts/lib.sh) with a 6 GiB memory cap.
set -eu

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-6g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-6g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example rocksdb_skew

echo "--- evidence/bench/rocksdb-skew.json (head) ---"
sed -n '1,10p' "$PROJECT_DIR/evidence/bench/rocksdb-skew.json" 2>/dev/null || echo "(no evidence file)"
