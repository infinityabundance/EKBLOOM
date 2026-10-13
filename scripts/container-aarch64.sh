#!/bin/sh
# Cross-architecture determinism: run the SAME corpus through encode →
# `physical_bytes()` → content hash on native x86_64 and on `--platform
# linux/arm64` (QEMU), and assert the canonical `ARCHDET` output is IDENTICAL.
#
# tests/arch_determinism.rs prints one canonical line per (corpus, machine) with
# the root body's physical bytes, the BLAKE3 core hash, and an FNV-1a content hash
# of the decoded bytes, plus the `auto`/`Full` plan's chosen machine and physical
# bytes. The corpus is the committed real Silesia/Calgary prefixes in
# `fuzz/seeds/` plus deterministic generated sources.
#
# Every docker run is memory-capped. If arm64 emulation (qemu-aarch64 via
# binfmt_misc) is unavailable, this script reports the EXACT docker error and what
# can be verified instead — it never fakes the comparison.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
AMD64_IMAGE="ekbloom-rust:1.99.0"
ARM64_IMAGE="rust:1.99.0-slim-bookworm"
NIGHTLY_IMAGE="rustlang/rust:nightly-bookworm-slim@sha256:8bc2b7245b41fb54c50812dc57d9f425de4685a004f6d37af8bcb29df4efeb82"

MEM="${EKB_MEM_LIMIT:-4g}"
CPUS="${EKB_CPUS:-8}"
PIDS="${EKB_PIDS:-2048}"

# The 61-test bounded subset that Miri covers in scripts/container-miri.sh
# (explicit module prefixes; a bare `machine` substring would also pull in
# clock-dependent filesystem/fuse tests). On arm64 these run as ordinary arm64
# tests — Miri itself is an interpreter, so executing it under QEMU emulation is
# prohibitively slow (set EKB_ARM64_MIRI=1 to attempt it anyway).
MIRI_FILTER="machine:: machine_pack:: machine_seg:: machine_store:: machine_rans:: machine_rans_i:: machine_rans_il:: machine_rans_w:: blocks:: stream:: checkpoint:: bloom resource::"
MIRI_SKIP="--skip stream::tests::roundtrip_various_sizes --skip stream::tests::truncated_and_crafted_are_typed_errors --skip checkpoint::tests::restore_matches_state_log_all_intervals"

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM

if ! docker image inspect "$AMD64_IMAGE" >/dev/null 2>&1; then
    echo "container-aarch64: project image '$AMD64_IMAGE' missing; build with scripts/container-image.sh" >&2
    exit 1
fi

# --- probe arm64 emulation -----------------------------------------------------
echo "== probing linux/arm64 emulation (qemu-aarch64 via binfmt_misc) =="
if ! docker run --rm --memory="$MEM" --memory-swap="$MEM" \
        --platform linux/arm64 "$ARM64_IMAGE" uname -m \
        >"$TMP/probe.out" 2>"$TMP/probe.err"; then
    echo "container-aarch64: linux/arm64 emulation is UNAVAILABLE on this host." >&2
    echo "--- exact docker error ---" >&2
    cat "$TMP/probe.err" >&2
    echo "--- what can be verified instead ---" >&2
    echo "  * x86_64-only determinism: 'sh scripts/run-gates.sh' (G1-G5) and the" >&2
    echo "    full suite 'sh scripts/container-verify.sh';" >&2
    echo "  * to enable QEMU arm64 (needs host privileges), register binfmt:" >&2
    echo "    docker run --privileged --rm tonistiigi/binfmt --install arm64" >&2
    # Exit 3 = emulation unavailable (distinct from a real failure) so
    # scripts/run-matrix.sh can record this job as SKIPPED rather than FAIL.
    exit 3
fi
echo "  arm64 probe: $(cat "$TMP/probe.out")"

# Every linux/arm64 command runs through this memory-capped, separate-target-dir
# helper (QEMU via binfmt_misc).
arm64_run() {
    docker run --rm \
        --memory="$MEM" --memory-swap="$MEM" --cpus="$CPUS" \
        --pids-limit="$PIDS" --ulimit core=0 \
        --cap-drop=ALL --security-opt=no-new-privileges \
        --platform linux/arm64 \
        -v "$PROJECT_DIR":/work -w /work \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/work/.cargo-home \
        -e RUSTUP_HOME=/usr/local/rustup \
        -e CARGO_TARGET_DIR=/work/target/aarch64 \
        -e CARGO_TERM_COLOR=never \
        "$ARM64_IMAGE" "$@"
}

# --- x86_64 run ----------------------------------------------------------------
echo "== tests/arch_determinism on x86_64 =="
. "$(dirname "$0")/lib.sh"   # provides docker_run (amd64 project image)
set +e
docker_run cargo test --test arch_determinism -- --nocapture >"$TMP/amd64.raw" 2>&1
rc=$?
set -e
grep '^ARCHDET ' "$TMP/amd64.raw" | LC_ALL=C sort >"$TMP/amd64.txt" || true
if [ "$rc" -ne 0 ]; then
    echo "container-aarch64: x86_64 run FAILED (rc=$rc)" >&2
    tail -n 30 "$TMP/amd64.raw" >&2
    exit 1
fi

# --- arm64 run (QEMU); a separate target dir keeps the amd64 build intact ------
echo "== tests/arch_determinism on linux/arm64 (QEMU) =="
set +e
arm64_run cargo test --test arch_determinism -- --nocapture >"$TMP/arm64.raw" 2>&1
rc=$?
set -e
grep '^ARCHDET ' "$TMP/arm64.raw" | LC_ALL=C sort >"$TMP/arm64.txt" || true
if [ "$rc" -ne 0 ]; then
    echo "container-aarch64: arm64 run FAILED (rc=$rc)" >&2
    tail -n 30 "$TMP/arm64.raw" >&2
    exit 1
fi

# --- compare -------------------------------------------------------------------
n_amd=$(wc -l <"$TMP/amd64.txt" | tr -d ' ')
n_arm=$(wc -l <"$TMP/arm64.txt" | tr -d ' ')
if [ "$n_amd" -eq 0 ]; then
    echo "container-aarch64: x86_64 produced no ARCHDET lines" >&2
    exit 1
fi

amd_hash=$(sha256sum "$TMP/amd64.txt" | cut -d' ' -f1)
arm_hash=$(sha256sum "$TMP/arm64.txt" | cut -d' ' -f1)

if diff -u "$TMP/amd64.txt" "$TMP/arm64.txt" >"$TMP/diff.txt" 2>&1; then
    identical=true
    verdict="identical physical_bytes() and content hashes across x86_64 and arm64"
else
    identical=false
    verdict="MISMATCH between x86_64 and arm64"
    echo "--- diff (x86_64 vs arm64) ---" >&2
    cat "$TMP/diff.txt" >&2
fi

echo "  amd64 lines=$n_amd sha256=$amd_hash"
echo "  arm64 lines=$n_arm sha256=$arm_hash"

mkdir -p "$PROJECT_DIR/evidence/bench"
{
    printf '{\n'
    printf '  "generated_by": "scripts/container-aarch64.sh",\n'
    printf '  "platforms": ["linux/amd64", "linux/arm64"],\n'
    printf '  "arm64_emulation": "qemu-aarch64 (binfmt_misc)",\n'
    printf '  "corpus": "fuzz/seeds/*.bin (Silesia/Calgary 4 KiB) + generated",\n'
    printf '  "amd64_lines": %s,\n' "$n_amd"
    printf '  "arm64_lines": %s,\n' "$n_arm"
    printf '  "amd64_output_sha256": "%s",\n' "$amd_hash"
    printf '  "arm64_output_sha256": "%s",\n' "$arm_hash"
    printf '  "identical": %s,\n' "$identical"
    printf '  "verdict": "%s"\n' "$verdict"
    printf '}\n'
} >"$PROJECT_DIR/evidence/bench/aarch64-determinism.json"

echo "== cross-arch determinism: identical=$identical =="
echo "   evidence/bench/aarch64-determinism.json"

if [ "$identical" != "true" ]; then
    exit 1
fi

# --- arm64 CI job: Miri subset + gates + bounded court -------------------------
# The arm64 build has no AVX2, so every SIMD dispatch in src/simd.rs takes its
# scalar fallback: this job is the cross-architecture half of the "the scalar
# fallback cannot rot" contract (the x86_64 half is scripts/run-scalar.sh).
# Results are recorded in evidence/bench/ci-aarch64.json for scripts/run-matrix.sh.
echo "== arm64 CI job: bounded Miri subset (the tests scripts/container-miri.sh covers) =="
miri_subset=1
set +e
arm64_run cargo test --release --lib -- $MIRI_FILTER >"$TMP/arm64-lib.raw" 2>&1
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
    echo "container-aarch64: arm64 Miri-subset run FAILED (rc=$rc)" >&2
    tail -n 30 "$TMP/arm64-lib.raw" >&2
    miri_subset=0
fi
subset_passed="$(sed -n 's/^test result: ok\. \([0-9][0-9]*\) passed.*/\1/p' "$TMP/arm64-lib.raw" | tail -n1 || true)"
subset_passed="${subset_passed:-0}"
grep -E '^test result:' "$TMP/arm64-lib.raw" | tail -n1 || true

# If requested, also run the *real* Miri (nightly interpreter) for aarch64 under
# QEMU. Miri is an interpreter, so this is far slower than the native arm64 run
# above; it is opt-in (EKB_ARM64_MIRI=1).
true_miri=false
if [ "${EKB_ARM64_MIRI:-0}" = "1" ]; then
    echo "== arm64 CI job: true Miri under QEMU (opt-in) =="
    set +e
    docker run --rm \
        --memory="$MEM" --memory-swap="$MEM" --cpus="$CPUS" \
        --pids-limit="$PIDS" --ulimit core=0 \
        --cap-drop=ALL --security-opt=no-new-privileges \
        --platform linux/arm64 \
        -v "$PROJECT_DIR":/work -w /work \
        -e HOME=/tmp -e CARGO_HOME=/tmp/cargo-home \
        -e RUSTUP_HOME=/usr/local/rustup -e RUSTUP_TOOLCHAIN=nightly \
        -e CARGO_TARGET_DIR=/tmp/miri-target -e RUST_BACKTRACE=1 \
        -e CARGO_TERM_COLOR=never \
        "$NIGHTLY_IMAGE" sh -c "rustup component add miri && cargo miri test --locked --lib -- --test-threads=1 $MIRI_SKIP $MIRI_FILTER" \
        >"$TMP/arm64-miri.raw" 2>&1
    rc=$?
    set -e
    if [ "$rc" -eq 0 ]; then
        true_miri=true
    else
        echo "container-aarch64: arm64 true-Miri FAILED (rc=$rc)" >&2
        tail -n 30 "$TMP/arm64-miri.raw" >&2
    fi
fi

echo "== arm64 CI job: acceptance gates G1-G5 =="
gates=1
for gate in g1_ g2_ g3_ g4_ g5_; do
    set +e
    arm64_run cargo test --release --test gates "$gate" -- --nocapture --test-threads=1 \
        >"$TMP/arm64-gate-$gate.raw" 2>&1
    rc=$?
    set -e
    if [ "$rc" -eq 0 ]; then
        echo "  $gate: PASS"
    else
        echo "  $gate: FAIL" >&2
        tail -n 20 "$TMP/arm64-gate-$gate.raw" >&2
        gates=0
    fi
done

echo "== arm64 CI job: bounded Silesia-derived court (tests/ci_bench.rs) =="
court=1
set +e
arm64_run cargo test --release --test ci_bench -- --nocapture >"$TMP/arm64-court.raw" 2>&1
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
    echo "container-aarch64: arm64 court FAILED (rc=$rc)" >&2
    tail -n 30 "$TMP/arm64-court.raw" >&2
    court=0
fi
grep '^CIBENCH' "$TMP/arm64-court.raw" || true
AGG="$(grep '^CIBENCH-AGG ' "$TMP/arm64-court.raw" | tail -n1 || true)"
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

if [ "$miri_subset" = "1" ] && [ "$gates" = "1" ] && [ "$court" = "1" ]; then
    pass=true
else
    pass=false
fi

{
    printf '{\n'
    printf '  "generated_by": "scripts/container-aarch64.sh",\n'
    printf '  "job": "aarch64",\n'
    printf '  "platform": "linux/arm64",\n'
    printf '  "arm64_emulation": "qemu-aarch64 (binfmt_misc)",\n'
    printf '  "simd_forced_scalar": false,\n'
    printf '  "pass": %s,\n' "$pass"
    printf '  "miri_subset_pass": %s,\n' "$miri_subset"
    printf '  "true_miri_pass": %s,\n' "$true_miri"
    printf '  "miri_subset_tests_passed": %s,\n' "$subset_passed"
    printf '  "gates_pass": %s,\n' "$gates"
    printf '  "court_pass": %s,\n' "$court"
    printf '  "footprint_bytes": %s,\n' "$physical"
    printf '  "byte_at_p50_ns": %s,\n' "$p50"
    printf '  "byte_at_p99_ns": %s,\n' "$p99"
    printf '  "byte_at_p99_9_ns": %s,\n' "$p999"
    printf '  "byte_at_p99_99_ns": %s,\n' "$p9999"
    printf '  "court": {"corpora": %s, "n": %s, "physical_bytes": %s, "e_v": %s, "samples": %s, "byte_at_p50_ns": %s, "byte_at_p99_ns": %s, "byte_at_p99_9_ns": %s, "byte_at_p99_99_ns": %s},\n' \
        "$corpora" "$cn" "$physical" "$e_v" "$samples" "$p50" "$p99" "$p999" "$p9999"
    printf '  "determinism": {"identical": %s, "amd64_output_sha256": "%s", "arm64_output_sha256": "%s"},\n' "$identical" "$amd_hash" "$arm_hash"
    printf '  "note": "arm64 has no AVX2, so every SIMD dispatch takes its scalar fallback; byte_at percentiles over deterministic samples, each read byte-checked"\n'
    printf '}\n'
} >"$PROJECT_DIR/evidence/bench/ci-aarch64.json"

echo "== arm64 CI job: pass=$pass (miri_subset=$miri_subset gates=$gates court=$court)  physical=$physical  byte_at p50=${p50}ns p99.9=${p999}ns p99.99=${p9999}ns =="
echo "   evidence/bench/ci-aarch64.json"

[ "$pass" = "true" ]
