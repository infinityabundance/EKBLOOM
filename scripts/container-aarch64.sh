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

MEM="${EKB_MEM_LIMIT:-4g}"
CPUS="${EKB_CPUS:-8}"
PIDS="${EKB_PIDS:-2048}"

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
    exit 1
fi
echo "  arm64 probe: $(cat "$TMP/probe.out")"

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
    "$ARM64_IMAGE" cargo test --test arch_determinism -- --nocapture >"$TMP/arm64.raw" 2>&1
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
