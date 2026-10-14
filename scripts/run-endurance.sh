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
# Also runs one separate process per phase (decode / publish / append) over the
# same corpus, to isolate which phase any residual RSS creep comes from. Those
# sub-run JSONs are embedded verbatim in evidence/bench/endurance-phases.json.
#
# Produces:
#   evidence/bench/endurance.json
#   evidence/bench/endurance-phases.json
#
# Runs only inside the pinned container (scripts/lib.sh) with a 6 GiB memory cap.
set -eu

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-6g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-6g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

EKB_ENDURANCE_SECONDS="${EKB_ENDURANCE_SECONDS:-120}"
EKB_ENDURANCE_PHASE_SECONDS="${EKB_ENDURANCE_PHASE_SECONDS:-45}"
EVIDENCE="$PROJECT_DIR/evidence/bench/endurance.json"
PHASES_EVIDENCE="$PROJECT_DIR/evidence/bench/endurance-phases.json"
LOG="$(mktemp)"

mkdir -p "$PROJECT_DIR/evidence/bench"
rm -f "$EVIDENCE" "$PHASES_EVIDENCE"

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
grep -E '"(records|hot_fraction|elapsed_s|decode_ops|publish_ops|append_generations|decode_per_s|sentinel_mismatches|ended_reason|rss_drift_bytes|rss_max|leak_bytes|alloc_in_use_end|plateau|rss_creep_bytes_per_s_last_half|alloc_flat_after_ms|append_monotone|append_reopen_verified|seven_day_decode_ops)"' "$EVIDENCE" | tr -d ' ' | tr '\n' ' '
echo

# --- per-phase breakdown -------------------------------------------------------
# The same harness in three more processes, one phase each, so a residual RSS
# creep can be attributed to the read path, the publication churn, or the append
# path. Each phase JSON is a complete, gated harness artifact; it is embedded
# verbatim below -- nothing is reformatted, merged field-by-field, or invented.
PH="$(mktemp -d)"
trap 'rm -rf "$PH"' EXIT INT TERM

for ph in decode publish append; do
    echo "== endurance phase: $ph (${EKB_ENDURANCE_PHASE_SECONDS}s) =="
    docker_run cargo run --release --example endurance \
        -- --seconds "$EKB_ENDURANCE_PHASE_SECONDS" --phase "$ph" > "$PH/$ph.json"
    if ! grep -q '^}$' "$PH/$ph.json"; then
        echo "run-endurance: phase '$ph' produced no complete JSON on stdout" >&2
        exit 1
    fi
done

{
    printf '{\n'
    printf '  "generated_by": "scripts/run-endurance.sh (phase matrix)",\n'
    printf '  "method": "one separate process per phase over the same deterministic corpus; each phase object is embedded verbatim from the gated harness",\n'
    printf '  "phase_seconds": %s,\n' "$EKB_ENDURANCE_PHASE_SECONDS"
    printf '  "decode": '
    cat "$PH/decode.json"
    printf ',\n'
    printf '  "publish": '
    cat "$PH/publish.json"
    printf ',\n'
    printf '  "append": '
    cat "$PH/append.json"
    printf '\n}\n'
} > "$PHASES_EVIDENCE"
rm -rf "$PH"
trap - EXIT INT TERM

echo "--- evidence/bench/endurance-phases.json (creep by phase) ---"
grep -E '"(rss_max|rss_creep_bytes_per_s_last_half|alloc_flat_after_ms|alloc_in_use_end|leak_bytes|plateau)"' "$PHASES_EVIDENCE" | tr -d ' ' | tr '\n' ' '
echo
