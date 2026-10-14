#!/bin/sh
# Real embedded KV-engine integration court inside the pinned, memory-capped
# container.
#
# Builds a deterministic dataset of 100-byte values with an 80/20 key skew and
# stores them, as codec outputs, inside a real embedded KV engine (redb): raw,
# block-indexed LZ4, Zstandard under a trained dictionary, and EKBLOOM machine 9
# (`OrANS-DICT`, one shared trained dictionary + referenced per-record cores). The
# shared dictionary is stored once under a metadata key and counted in the DB
# footprint. It reports database-level point-get p50/p99/p99.9, range-scan
# throughput, reopen cost, cache residency (warm vs cold), write amplification and
# total DB file footprint, plus a record-granular ledger reference.
#
# Produces:
#   evidence/bench/kv-integration.json
#
# Runs only inside the pinned container (scripts/lib.sh) with a 6 GiB memory cap.
set -eu

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-6g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-6g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example kv_integration

echo "--- evidence/bench/kv-integration.json (head) ---"
sed -n '1,10p' "$PROJECT_DIR/evidence/bench/kv-integration.json" 2>/dev/null || echo "(no evidence file)"
