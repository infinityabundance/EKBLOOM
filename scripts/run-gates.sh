#!/bin/sh
# EKBLOOM acceptance gates G1–G5 — machine-checked, executable.
#
# Runs `tests/gates.rs` (release) one gate at a time. Each gate asserts its
# property with the public API (reusing the same oracle as the focused tests /
# benches), prints `GATE G<n> <name>: PASS — <evidence>` and writes
# `evidence/gates/G<n>.json`; a violation panics, so the targeted run exits
# non-zero and this script reports FAIL for that gate. The script continues after
# a FAIL so every gate's verdict is printed, and exits non-zero if any failed.
#
# Gate map (see docs/GATES.md):
#   G1 invertibility        encode∘decode = id, machines 1–9
#   G2 prefix composition   affine block composition exact (tests/prefix.rs)
#   G3 worst-case bound     measured_steps <= worst_case over 1,000,000 positions
#   G4 corruption detection single-bit flips detected (tests/corruption.rs)
#   G5 physical accounting  physical_bytes() vs to_bytes().len() (tests/accounting.rs)
#
# Runs ONLY inside the pinned, memory-capped container (scripts/lib.sh).
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/gates"

failed=0
gate() {
    filter="$1"
    id="$2"
    name="$3"
    echo "== $id $name =="
    if docker_run cargo test --release --test gates "$filter" -- --nocapture --test-threads=1; then
        echo "$id $name: PASS"
    else
        echo "$id $name: FAIL" >&2
        failed=1
    fi
}

gate g1_ G1 invertibility
gate g2_ G2 prefix-composition
gate g3_ G3 worst-case-bound
gate g4_ G4 corruption-detection
gate g5_ G5 physical-accounting

if [ "$failed" -ne 0 ]; then
    echo "gates: FAIL" >&2
    exit 1
fi
echo "gates: ALL PASS"
