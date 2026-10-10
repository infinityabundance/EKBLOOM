#!/bin/sh
# Reproducible clean build + test + environment capture from a pristine checkout.
#
# Removes build artifacts, rebuilds from Cargo.lock, runs the correctness suite,
# and refreshes the environment record. The environment capture is delegated to
# scripts/capture-environment.sh (the single source of truth for
# evidence/environment.txt); this script only owns the clean build + test run.
set -eu
. "$(dirname "$0")/lib.sh"

echo "== capturing environment =="
# Single source of truth for evidence/environment.txt: delegate to the rich
# capture script so the reproduce path and the manual capture never diverge.
sh "$PROJECT_DIR/scripts/capture-environment.sh"

echo "== clean build =="
rm -rf "$PROJECT_DIR/target"
docker_run cargo build --release --locked

echo "== tests =="
docker_run cargo test --release --locked

echo "== reproduce complete =="
