#!/bin/sh
# EKBLOOM cross-platform CI matrix — the CI entry point.
#
# Runs three jobs and records a single machine-readable verdict:
#
#   1. x86_64-simd    native AVX2 (the control arm)               -> ci-x86_64-simd.json
#   2. x86_64-scalar  x86_64 with EKB_NO_SIMD=1 (the scalar path) -> ci-x86_64-scalar.json
#   3. aarch64        linux/arm64 under QEMU (no AVX2)            -> ci-aarch64.json
#
# Every job runs the SIMD-vs-scalar equality suite, the acceptance gates G1-G5
# and the bounded Silesia-derived `byte_at` court (tests/ci_bench.rs); the arm64
# job additionally runs the bounded 61-test Miri subset. The point is structural:
# the scalar fallback is exercised on the primary architecture (job 2) and on a
# genuinely AVX2-less architecture (job 3) on every run, so it cannot rot.
#
# QEMU arm64 is best effort: if linux/arm64 emulation is unavailable the aarch64
# job is recorded SKIPPED (not FAIL) and the matrix still passes on x86_64.
#
# Writes evidence/bench/ci-matrix.json and prints a PASS/FAIL summary. Runs only
# inside the pinned, memory-capped containers (scripts/lib.sh,
# scripts/run-scalar.sh, scripts/container-aarch64.sh).
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BENCH_DIR="$PROJECT_DIR/evidence/bench"
mkdir -p "$BENCH_DIR"

echo "################ EKBLOOM CI matrix ################"

echo
echo ">>> job 1/3: x86_64-simd (native AVX2, control arm)"
rc_simd=0
sh "$PROJECT_DIR/scripts/run-scalar.sh" simd || rc_simd=$?

echo
echo ">>> job 2/3: x86_64-scalar (EKB_NO_SIMD=1, the scalar fallback)"
rc_scalar=0
sh "$PROJECT_DIR/scripts/run-scalar.sh" scalar || rc_scalar=$?

echo
echo ">>> job 3/3: aarch64 (linux/arm64 under QEMU; best effort)"
rc_arm=0
sh "$PROJECT_DIR/scripts/container-aarch64.sh" || rc_arm=$?

# Read a flat top-level `"key": value` out of a job JSON (anchored, first match),
# stripping the trailing comma so the value is bare.
jget() {
    [ -f "$1" ] || return 0
    sed -n "s/^[[:space:]]*\"$2\": //p" "$1" | head -n1 | sed 's/,[[:space:]]*$//'
}

simd_json="$BENCH_DIR/ci-x86_64-simd.json"
scalar_json="$BENCH_DIR/ci-x86_64-scalar.json"
arm_json="$BENCH_DIR/ci-aarch64.json"

if [ "$rc_simd" -eq 0 ]; then simd_status=PASS; else simd_status=FAIL; fi
if [ "$rc_scalar" -eq 0 ]; then scalar_status=PASS; else scalar_status=FAIL; fi
case "$rc_arm" in
    0) arm_status=PASS; arm_note="run under QEMU" ;;
    3) arm_status=SKIPPED; arm_note="linux/arm64 emulation unavailable" ;;
    *) arm_status=FAIL; arm_note="arm64 job failed (rc=$rc_arm)" ;;
esac

metric() { # metric <file> <key>
    v="$(jget "$1" "$2")"
    printf '%s' "${v:-null}"
}

simd_fp="$(metric "$simd_json" footprint_bytes)"
simd_p50="$(metric "$simd_json" byte_at_p50_ns)"
simd_p99="$(metric "$simd_json" byte_at_p99_ns)"
simd_p999="$(metric "$simd_json" byte_at_p99_9_ns)"
simd_p9999="$(metric "$simd_json" byte_at_p99_99_ns)"
scalar_fp="$(metric "$scalar_json" footprint_bytes)"
scalar_p50="$(metric "$scalar_json" byte_at_p50_ns)"
scalar_p99="$(metric "$scalar_json" byte_at_p99_ns)"
scalar_p999="$(metric "$scalar_json" byte_at_p99_9_ns)"
scalar_p9999="$(metric "$scalar_json" byte_at_p99_99_ns)"
arm_fp="$(metric "$arm_json" footprint_bytes)"
arm_p50="$(metric "$arm_json" byte_at_p50_ns)"
arm_p99="$(metric "$arm_json" byte_at_p99_ns)"
arm_p999="$(metric "$arm_json" byte_at_p99_9_ns)"
arm_p9999="$(metric "$arm_json" byte_at_p99_99_ns)"

if [ "$simd_status" = "PASS" ] && [ "$scalar_status" = "PASS" ] && [ "$arm_status" != "FAIL" ]; then
    verdict=PASS
else
    verdict=FAIL
fi

{
    printf '{\n'
    printf '  "generated_by": "scripts/run-matrix.sh",\n'
    printf '  "description": "cross-platform CI matrix: {x86_64-simd, x86_64-scalar, aarch64}; each job runs the SIMD-vs-scalar equality suite, gates G1-G5 and the bounded Silesia-derived byte_at court; aarch64 additionally runs the bounded Miri subset",\n'
    printf '  "matrix": ["x86_64-simd", "x86_64-scalar", "aarch64"],\n'
    printf '  "jobs": [\n'
    printf '    {"job": "x86_64-simd", "platform": "linux/amd64", "status": "%s", "simd_forced_scalar": false, "footprint_bytes": %s, "byte_at_p50_ns": %s, "byte_at_p99_ns": %s, "byte_at_p99_9_ns": %s, "byte_at_p99_99_ns": %s, "evidence": "evidence/bench/ci-x86_64-simd.json"},\n' \
        "$simd_status" "$simd_fp" "$simd_p50" "$simd_p99" "$simd_p999" "$simd_p9999"
    printf '    {"job": "x86_64-scalar", "platform": "linux/amd64", "status": "%s", "simd_forced_scalar": true, "footprint_bytes": %s, "byte_at_p50_ns": %s, "byte_at_p99_ns": %s, "byte_at_p99_9_ns": %s, "byte_at_p99_99_ns": %s, "evidence": "evidence/bench/ci-x86_64-scalar.json"},\n' \
        "$scalar_status" "$scalar_fp" "$scalar_p50" "$scalar_p99" "$scalar_p999" "$scalar_p9999"
    printf '    {"job": "aarch64", "platform": "linux/arm64", "status": "%s", "simd_forced_scalar": false, "footprint_bytes": %s, "byte_at_p50_ns": %s, "byte_at_p99_ns": %s, "byte_at_p99_9_ns": %s, "byte_at_p99_99_ns": %s, "note": "%s", "evidence": "evidence/bench/ci-aarch64.json"}\n' \
        "$arm_status" "$arm_fp" "$arm_p50" "$arm_p99" "$arm_p999" "$arm_p9999" "$arm_note"
    printf '  ],\n'
    printf '  "summary": {"x86_64-simd": "%s", "x86_64-scalar": "%s", "aarch64": "%s"},\n' \
        "$simd_status" "$scalar_status" "$arm_status"
    printf '  "verdict": "%s"\n' "$verdict"
    printf '}\n'
} >"$BENCH_DIR/ci-matrix.json"

echo
echo "================ CI matrix summary ================"
printf '  %-14s %-8s footprint=%-8s byte_at p50=%-7s p99.9=%-9s p99.99=%s ns\n' \
    "x86_64-simd" "$simd_status" "$simd_fp" "$simd_p50" "$simd_p999" "$simd_p9999"
printf '  %-14s %-8s footprint=%-8s byte_at p50=%-7s p99.9=%-9s p99.99=%s ns\n' \
    "x86_64-scalar" "$scalar_status" "$scalar_fp" "$scalar_p50" "$scalar_p999" "$scalar_p9999"
printf '  %-14s %-8s footprint=%-8s byte_at p50=%-7s p99.9=%-9s p99.99=%s ns  (%s)\n' \
    "aarch64" "$arm_status" "$arm_fp" "$arm_p50" "$arm_p999" "$arm_p9999" "$arm_note"
echo "  verdict: $verdict"
echo "  evidence/bench/ci-matrix.json"

[ "$verdict" = "PASS" ]
