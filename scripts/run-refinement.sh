#!/bin/sh
# Mechanical re-check of the refinement claims in docs/REFINEMENT.md.
#
# Runs the concrete check each proof obligation (§11) cites — the acceptance gates
# G1-G5 plus the named focused tests — inside the pinned, memory-capped container,
# and prints a PASS/FAIL table mapping every row back to its obligation. Exits
# non-zero if any obligation fails. No project tooling runs on the host.
#
# Every docker run is memory-capped at 4 GiB (the Loom directive); export the caps
# before sourcing lib.sh so docker_run applies them.
set -eu

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-4g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-4g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

RESULTS="$(mktemp)"
trap 'rm -f "$RESULTS"' EXIT INT TERM
failed=0

# run one obligation: id, human obligation text (no '|'), then the command.
check() {
    id="$1"
    obligation="$2"
    shift 2
    echo ""
    echo "== [$id] $obligation =="
    if "$@"; then
        status=PASS
        echo "[$id] $obligation: PASS"
    else
        status=FAIL
        echo "[$id] $obligation: FAIL" >&2
        failed=1
    fi
    printf '%s|%s|%s\n' "$id" "$obligation" "$status" >>"$RESULTS"
}

# The Loom row needs its own RUSTFLAGS/env; mirror scripts/container-loom.sh. It is
# run last so the `--cfg loom` rebuild does not perturb the other (non-loom) builds.
loom_rcu() {
    docker_run env \
        RUSTFLAGS=--cfg\ loom \
        "LOOM_MAX_PREEMPTIONS=${EKB_LOOM_MAX_PREEMPTIONS:-2}" \
        cargo test --release --test loom_rcu -- --test-threads=1
}

echo "=================================================================="
echo " EKBLOOM refinement re-check (docs/REFINEMENT.md §11)"
echo " image=$IMAGE  memory=${EKB_MEM_LIMIT}/${EKB_MEMSWAP_LIMIT}"
echo "=================================================================="

# --- acceptance gates G1-G5 ---------------------------------------------------
check O1 "invertibility (G1, machines 1-9)" \
    docker_run cargo test --release --test gates g1_ -- --nocapture --test-threads=1
check O2 "affine prefix composition (G2)" \
    docker_run cargo test --release --test gates g2_ -- --nocapture --test-threads=1
check O3 "worst-case step bound (G3)" \
    docker_run cargo test --release --test gates g3_ -- --nocapture --test-threads=1
check O4 "corruption detection (G4)" \
    docker_run cargo test --release --test gates g4_ -- --nocapture --test-threads=1
check O5 "exact physical accounting (G5)" \
    docker_run cargo test --release --test gates g5_ -- --nocapture --test-threads=1

# --- resource / state / window / restart -------------------------------------
check O6 "estimate >= actual physical bytes" \
    docker_run cargo test --release --test budget estimate_never_underestimates_actual_physical_bytes
check O7 "allocation caps (ResourcePolicy)" \
    docker_run cargo test --release --test resource
check O8 "complete resumable state, machines 4 and 8" \
    docker_run cargo test --release --test machine4_restarts
check O9 "bounded encoded window (cursor-divergent)" \
    docker_run cargo test --release --test window
check O10 "certified restarts equal authoritative state" \
    docker_run cargo test --release --test history_checkpoint --test blocks --test region_adapt --test machine_rans_i

# --- cross-arch / scalar==SIMD / version gate / faults / persistence ---------
check O11 "cross-architecture determinism (x86_64 half)" \
    docker_run cargo test --release --test arch_determinism
check O12 "scalar == SIMD equivalence" \
    docker_run cargo test --release --test simd
check O13 "version-gated read compatibility" \
    docker_run cargo test --release --test format_version --test compat_fixtures
check O14 "fault-atomic publication" \
    docker_run cargo test --release --test faults
check O16 "structure persistence round-trip" \
    docker_run cargo test --release --test persist_all

# --- Loom RCU (last: it rebuilds the lib with --cfg loom) ---------------------
check O15 "Loom RCU publication" loom_rcu

# --- summary table ------------------------------------------------------------
echo ""
echo "==================== refinement obligation summary ===================="
printf '%-6s %-52s %s\n' "OBLIG" "obligation" "RESULT"
printf '%-6s %-52s %s\n' "-----" "----------------------------------------------------" "------"
sort -t'|' -k1.2n "$RESULTS" | while IFS='|' read -r id obligation status; do
    printf '%-6s %-52s %s\n' "$id" "$obligation" "$status"
done

if [ "$failed" -ne 0 ]; then
    echo "refinement: FAIL" >&2
    exit 1
fi
echo "refinement: ALL PASS"
