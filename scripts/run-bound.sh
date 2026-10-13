#!/bin/sh
# Worst-case bound verification — EKBLOOM's core promise vs rANS.
#
# Drives the bounded engine over 1,000,000 random positions on a body with a
# large 1 MiB block restart (plus a decode-bound rANS pass and a multi-path
# cold -> hot -> cold pass) and asserts, for EVERY sample,
# `measured positioning steps <= worst_case_steps_for(request)`, byte-checking
# the decoded output. The violation count must be zero.
#
# Produces:
#   evidence/bench/bound.json — per pass: samples, max measured steps, max
#   claimed bound, violations, mismatches.
#
# Runs only inside the pinned, memory-capped container (scripts/lib.sh), capped
# at 6 GiB so a runaway allocation cannot exhaust the host.
set -eu

EKB_MEM_LIMIT=6g
EKB_MEMSWAP_LIMIT=6g
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example worstcase_bound
