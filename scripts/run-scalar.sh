#!/bin/sh
# x86_64 CI job: the SIMD-vs-scalar equality suite, the acceptance gates G1–G5,
# and the bounded Silesia-derived court — run twice from `scripts/run-matrix.sh`:
#
#   sh scripts/run-scalar.sh simd     # native AVX2 (control arm)  -> x86_64-simd
#   sh scripts/run-scalar.sh scalar   # EKB_NO_SIMD=1 (the scalar fallback) -> x86_64-scalar
#
# The `scalar` arm runs the identical commands with `EKB_NO_SIMD=1` exported into
# the container, so every SIMD dispatch in `src/simd.rs` takes its scalar
# reference and the forced-scalar equality test in `tests/simd.rs` certifies the
# two paths are byte-identical *on AVX2 hardware*. This is the primary-arch arm
# of the cross-platform matrix; see docs/TESTING.md.
#
# Runs ONLY inside the pinned, memory-capped container (scripts/lib.sh); every
# docker run is bounded by --memory/--memory-swap. Writes
# evidence/bench/ci-x86_64-<mode>.json for `scripts/run-matrix.sh` to aggregate.
set -eu
. "$(dirname "$0")/lib.sh"

MODE="${1:-scalar}"
case "$MODE" in
    scalar) JOB="x86_64-scalar" FORCE=1 ;;
    simd)   JOB="x86_64-simd"   FORCE=0 ;;
    *) echo "run-scalar: mode must be 'simd' or 'scalar' (got '$MODE')" >&2; exit 2 ;;
esac

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM
mkdir -p "$PROJECT_DIR/evidence/bench"

# Run a cargo command in the pinned container; the scalar arm forces the scalar
# path process-wide with EKB_NO_SIMD=1.
run() {
    if [ "$FORCE" = "1" ]; then
        docker_run sh -c "EKB_NO_SIMD=1 $*"
    else
        docker_run "$@"
    fi
}

echo "== $JOB: SIMD-vs-scalar equality (tests/simd.rs, forced-scalar = $FORCE) =="
equality=false
if run cargo test --release --test simd -- --nocapture >"$TMP/simd.raw" 2>&1; then
    equality=true
else
    tail -n 30 "$TMP/simd.raw" >&2
fi
grep -E 'forced_scalar_is_byte_identical|test result:' "$TMP/simd.raw" || true

echo "== $JOB: acceptance gates G1-G5 =="
gates=false
gate_ok=1
for gate in g1_ g2_ g3_ g4_ g5_; do
    if run cargo test --release --test gates "$gate" -- --nocapture --test-threads=1 \
            >"$TMP/gate-$gate.raw" 2>&1; then
        echo "  $gate: PASS"
    else
        echo "  $gate: FAIL" >&2
        tail -n 20 "$TMP/gate-$gate.raw" >&2
        gate_ok=0
    fi
done
[ "$gate_ok" = "1" ] && gates=true

echo "== $JOB: bounded Silesia-derived court (tests/ci_bench.rs) =="
court=false
if run cargo test --release --test ci_bench -- --nocapture >"$TMP/court.raw" 2>&1; then
    court=true
else
    tail -n 30 "$TMP/court.raw" >&2
fi
grep '^CIBENCH' "$TMP/court.raw" || echo "  (no CIBENCH lines)" >&2

# Extract the aggregate court line into named metrics.
AGG="$(grep '^CIBENCH-AGG ' "$TMP/court.raw" | tail -n1 || true)"
metric() {
    printf '%s\n' "$AGG" | awk -v k="$1" '{
        for (i = 1; i <= NF; i++) { n = split($i, a, "="); if (a[1] == k) { print a[2]; exit } }
    }'
}
corpora="$(metric corpora)"; corpora="${corpora:-0}"
cn="$(metric n)"; cn="${cn:-0}"
physical="$(metric physical)"; physical="${physical:-0}"
e_v="$(metric e_v)"; e_v="${e_v:-0}"
samples="$(metric samples)"; samples="${samples:-0}"
p50="$(metric byte_at_p50_ns)"; p50="${p50:-0}"
p99="$(metric byte_at_p99_ns)"; p99="${p99:-0}"
p999="$(metric byte_at_p99_9_ns)"; p999="${p999:-0}"
p9999="$(metric byte_at_p99_99_ns)"; p9999="${p9999:-0}"

if [ "$equality" = "true" ] && [ "$gates" = "true" ] && [ "$court" = "true" ]; then
    pass=true
else
    pass=false
fi

if [ "$FORCE" = "1" ]; then forced=true; else forced=false; fi

{
    printf '{\n'
    printf '  "generated_by": "scripts/run-scalar.sh",\n'
    printf '  "job": "%s",\n' "$JOB"
    printf '  "platform": "linux/amd64",\n'
    printf '  "simd_forced_scalar": %s,\n' "$forced"
    printf '  "pass": %s,\n' "$pass"
    printf '  "equality_pass": %s,\n' "$equality"
    printf '  "gates_pass": %s,\n' "$gates"
    printf '  "court_pass": %s,\n' "$court"
    printf '  "footprint_bytes": %s,\n' "$physical"
    printf '  "byte_at_p50_ns": %s,\n' "$p50"
    printf '  "byte_at_p99_ns": %s,\n' "$p99"
    printf '  "byte_at_p99_9_ns": %s,\n' "$p999"
    printf '  "byte_at_p99_99_ns": %s,\n' "$p9999"
    printf '  "court": {"corpora": %s, "n": %s, "physical_bytes": %s, "e_v": %s, "samples": %s, "byte_at_p50_ns": %s, "byte_at_p99_ns": %s, "byte_at_p99_9_ns": %s, "byte_at_p99_99_ns": %s},\n' \
        "$corpora" "$cn" "$physical" "$e_v" "$samples" "$p50" "$p99" "$p999" "$p9999"
    printf '  "note": "bounded, offline, Silesia/Calgary-derived court (tests/ci_bench.rs); byte_at percentiles over deterministic samples, each read byte-checked"\n'
    printf '}\n'
} >"$PROJECT_DIR/evidence/bench/ci-$JOB.json"

echo "== $JOB: pass=$pass  physical=$physical  e_v=$e_v  byte_at p50=${p50}ns p99.9=${p999}ns p99.99=${p9999}ns =="
echo "   evidence/bench/ci-$JOB.json"

[ "$pass" = "true" ]
