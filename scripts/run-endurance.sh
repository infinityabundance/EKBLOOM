#!/bin/sh
# Machine-9 long-run endurance, memory-fragmentation, and physical-byte growth
# study inside the pinned, memory-capped container.
#
# Runs a single time-compressed process (default 120 s) that sustains a mixed
# read (decode) / write (publish) / append (non-withdrawal) workload over a
# deterministic 20,000 x 100-byte corpus shared through a trained OrANS-DICT
# dictionary, sampling RSS and a counting global allocator after every batch.
# The example emits the evidence JSON on stdout; this script captures it.
#
# Produces:
#   evidence/bench/endurance.json
#
# Runs only inside the pinned container (scripts/lib.sh) with a 6 GiB memory cap.
set -eu

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-6g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-6g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

EKB_ENDURANCE_SECONDS="${EKB_ENDURANCE_SECONDS:-120}"
EVIDENCE="$PROJECT_DIR/evidence/bench/endurance.json"
LOG="$(mktemp)"

mkdir -p "$PROJECT_DIR/evidence/bench"
rm -f "$EVIDENCE"

# `cargo run` progress and any harness diagnostics go to stderr; stdout is the
# evidence JSON only. tee it so the run is visible live and captured to $LOG.
docker_run cargo run --release --example endurance -- --seconds "$EKB_ENDURANCE_SECONDS" | tee "$LOG"

# The harness prints the JSON only on success (any gate failure is a non-zero
# exit with a message on stderr), so a final top-level `}` line is the success gate.
if [ ! -s "$LOG" ] || ! grep -q '^}$' "$LOG"; then
    echo "run-endurance: harness produced no complete JSON on stdout" >&2
    rm -f "$LOG"
    exit 1
fi

cp "$LOG" "$EVIDENCE"
rm -f "$LOG"

echo "--- evidence/bench/endurance.json (summary) ---"
grep -E '"(records|hot_fraction|elapsed_s|decode_ops|publish_ops|append_generations|decode_per_s|sentinel_mismatches|ended_reason|rss_drift_bytes|rss_max|leak_bytes|alloc_in_use_end|plateau|append_monotone|append_reopen_verified|seven_day_decode_ops)"' "$EVIDENCE" | tr -d ' ' | tr '\n' ' '
echo
