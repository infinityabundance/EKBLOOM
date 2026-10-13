#!/bin/sh
# Generate (regenerate) the committed, curated fuzz corpus under evidence/fuzz/.
#
# Deterministic: re-running produces byte-identical seed files. Each subdirectory
# is the corpus for one libFuzzer target; see examples/fuzz_seeds.rs and
# evidence/fuzz/README.md. Runs ONLY inside the pinned, memory-capped container.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/fuzz"
echo "== generating curated fuzz corpus under evidence/fuzz/ =="
docker_run cargo run --release --example fuzz_seeds
echo "== done =="
