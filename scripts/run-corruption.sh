#!/bin/sh
# The corruption court: single-bit-flip detection across every structure.
#
# Builds a rich `.ekb` body and, for each structure kind, flips a single random
# bit — in the serialized payload (fast-path scan) and in the parsed structure
# (deep verifier) — measuring the detection rate, the granularity, and the typed
# error. Writes evidence/bench/corruption.json.
#
# Hard rule: all project tooling runs inside the pinned, memory-capped container,
# never on the host.
#
# Run: sh scripts/run-corruption.sh
set -eu
. "$(dirname "$0")/lib.sh"

OUT="$PROJECT_DIR/evidence/bench"
mkdir -p "$OUT"

echo "== corruption court: bit flips across every structure =="
docker_run cargo run --release --example corruption_court
echo "== corruption court wrote $OUT/corruption.json =="
