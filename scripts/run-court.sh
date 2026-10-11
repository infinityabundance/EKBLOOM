#!/bin/sh
# Rigorous performance court (directive §12).
#
# Runs `examples/court.rs` inside the pinned, memory-capped container and, where
# available, pins it with `taskset` so the numbers are not smeared across the
# host's CPUs. Writes evidence/bench/court.json.
#
# Hard rule: all project tooling runs inside the container, never on the host.
#
# Run: sh scripts/run-court.sh
set -eu
. "$(dirname "$0")/lib.sh"

# CPUs to pin to (inside the container). The container itself is capped by
# EKB_CPUS (default 8); pinning keeps the measured process off other work.
EKB_COURT_CPUS="${EKB_COURT_CPUS:-0-3}"
OUT="$PROJECT_DIR/evidence/bench"
mkdir -p "$OUT"

# Page size, probed inside the container (the host page size may differ).
PAGE="$(docker_run getconf PAGESIZE 2>/dev/null | tr -d '[:space:]' || true)"
[ -n "$PAGE" ] || PAGE=unknown

if docker_run sh -c 'command -v taskset >/dev/null 2>&1'; then
    echo "== court: pinned to CPUs $EKB_COURT_CPUS, page size $PAGE =="
    docker_run env EKB_PAGE_SIZE="$PAGE" taskset -c "$EKB_COURT_CPUS" \
        cargo run --release --example court
else
    echo "== court: taskset unavailable, running unpinned, page size $PAGE =="
    docker_run env EKB_PAGE_SIZE="$PAGE" cargo run --release --example court
fi

echo "== court wrote $OUT/court.json =="
