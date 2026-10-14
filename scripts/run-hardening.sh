#!/bin/sh
# Worst-case adversarial-input resource measurement.
#
# For each adversarial input class (malformed declared lengths, allocation
# exhaustion near/past the `ResourcePolicy` ceiling, dictionary substitution,
# authentication downgrade/rollback, and mapped/file truncation under an active
# reader), measures the WORST-CASE peak resident memory and wall-clock the
# opener/reader consumes *before returning a typed error*. Each class runs in its
# own child process so peak RSS is attributable, and the whole run executes inside
# the pinned container with a hard 4 GiB cap (`--memory=4g --memory-swap=4g`), so a
# runaway allocation cannot reach the host.
#
# Produces: evidence/bench/hardening.json (validated by scripts/validate-evidence.sh).
# Run: sh scripts/run-hardening.sh
set -eu

# Hard 4 GiB memory cap for this measurement (an input must never exceed the 1 GiB
# resource-policy ceiling, and the harness must never exhaust the host).
EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-4g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-4g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
echo "== adversarial-input worst-case memory/CPU measurement =="
docker_run cargo run --release --example hardening

echo "== validate evidence JSON =="
sh "$PROJECT_DIR/scripts/validate-evidence.sh"
echo "== wrote evidence/bench/hardening.json =="
