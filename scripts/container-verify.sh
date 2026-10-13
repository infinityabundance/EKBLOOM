#!/bin/sh
# Verification gate: safety/alloc confinement, fmt check, clippy (deny warnings),
# tests.
set -eu
. "$(dirname "$0")/lib.sh"
docker_run sh scripts/check-safety.sh
docker_run cargo fmt --all -- --check
docker_run cargo clippy --all-targets --all-features -- -D warnings
docker_run cargo test --all-features
