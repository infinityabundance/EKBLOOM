#!/bin/sh
# Validate every committed `evidence/**/*.json` against a strict JSON grammar and
# FAIL (non-zero) on any malformed file, so invalid artifacts cannot be published.
#
# Uses the dependency-free `validate_evidence` example, which runs inside the same
# pinned container as every other gate (never on the host).
set -eu
. "$(dirname "$0")/lib.sh"

docker_run cargo run --release --example validate_evidence
