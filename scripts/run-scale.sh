#!/bin/sh
# Nightly scale job: the 2^30 streaming/history build (bounded memory).
#
# `examples/large_history` streams a procedural machine-1 core over 2^30 logical
# positions into 1 MiB segments — writing exactly 4*(2^30+1) bytes of `u32` state
# projection — while peak RSS stays at a few MiB (the committed evidence records
# ~2.9 MiB), far below the 4 GiB the full projection would occupy in RAM. It
# writes `evidence/bench/history-2p30.json`.
#
# OPT-IN: this multi-GiB build is NOT part of the default reproduce. Set
# `EKB_RUN_SCALE=1` to run it; otherwise this script reports SKIPPED and exits 0,
# so an ordinary `sh scripts/run-scale.sh` is a no-op.
#
# Runs ONLY inside the pinned, memory-capped container (scripts/lib.sh), capped at
# `EKB_SCALE_MEM` (default 4g) so a runaway allocation cannot exhaust the host.
set -eu

if [ "${EKB_RUN_SCALE:-0}" != "1" ]; then
    echo "run-scale: SKIPPED — set EKB_RUN_SCALE=1 to run the 2^30 scale job"
    exit 0
fi

# Override lib.sh's default cap with the (tighter) scale cap before sourcing.
EKB_MEM_LIMIT="${EKB_SCALE_MEM:-4g}"
EKB_MEMSWAP_LIMIT="$EKB_MEM_LIMIT"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
echo "== 2^30 streaming/history scale build (bounded, memory=$EKB_MEM_LIMIT) =="
docker_run cargo run --release --example large_history
echo "== wrote evidence/bench/history-2p30.json =="
