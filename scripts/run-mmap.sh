#!/bin/sh
# Mapped read surfaces and huge pages, measured inside the pinned, memory-capped
# container. Writes evidence/bench/mmap.json.
#
# Four read modes at equal residency, per size (1/16/64/256 MiB):
#   1. pread     — cached OpenObject (FileBody) with bounded pread
#   2. resident  — in-memory materialised hot surface (the handle's bytes)
#   3. mmap      — read-only MappedSurface over a logically-aligned output file
#   4. mmap_huge — the same mapping with MADV_HUGEPAGE
# plus MAP_POPULATE / MADV_POPULATE_READ prefault variants, sequential throughput,
# random-1b p50/p99, 4-KiB p50, page faults, and /proc/self/smaps huge-page
# evidence.
#
# `mmap`/`madvise`/`munmap` need no Linux capability and are permitted by Docker's
# default seccomp profile, so `--cap-drop=ALL` is retained and no policy is
# relaxed. Stage 0 runs under the default profile and records the exact flags; if
# (and only if) a syscall were denied, stage 1 retries with
# `--security-opt seccomp=unconfined`. Nothing here runs project tooling on the host.
set -eu

. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
EVIDENCE_REL="evidence/bench/mmap.json"
EVIDENCE_ABS="$PROJECT_DIR/$EVIDENCE_REL"

# run_stage <label> <extra docker flags> <flags recorded in evidence>
run_stage() {
    label="$1"
    extra="$2"
    recorded="$3"
    echo "== $label =="
    # shellcheck disable=SC2086
    docker run --rm \
        --memory="$EKB_MEM_LIMIT" \
        --memory-swap="$EKB_MEMSWAP_LIMIT" \
        --cpus="$EKB_CPUS" \
        --pids-limit="$EKB_PIDS" \
        --ulimit core=0 \
        --cap-drop=ALL \
        --security-opt=no-new-privileges \
        $extra \
        -v "$PROJECT_DIR":/work \
        -w /work \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME="$EKB_CARGO_HOME_CONTAINER" \
        -e RUSTUP_HOME=/usr/local/rustup \
        -e CARGO_TARGET_DIR="$EKB_TARGET_DIR_CONTAINER" \
        -e CARGO_TERM_COLOR=never \
        -e RUSTUP_TOOLCHAIN=1.99.0 \
        -e EKB_MMAP_EVIDENCE="/work/$EVIDENCE_REL" \
        -e EKB_MMAP_CONTAINER_FLAGS="$recorded" \
        "$IMAGE" \
        cargo run --release --example mmap_modes
}

flags_used="--cap-drop=ALL --security-opt=no-new-privileges (Docker default seccomp)"
if run_stage \
    "stage 0: mmap/madvise/munmap under Docker default seccomp" \
    "" \
    "$flags_used"; then
    echo "mmap/madvise/munmap: permitted under the default profile (no relaxation needed)"
else
    flags_used="--cap-drop=ALL --security-opt=no-new-privileges --security-opt seccomp=unconfined"
    echo "a syscall was denied under the default profile; retrying with --security-opt seccomp=unconfined"
    run_stage "stage 1: --security-opt seccomp=unconfined" \
        "--security-opt seccomp=unconfined" \
        "$flags_used"
fi

echo "--- $EVIDENCE_REL ---"
sed -n '1,24p' "$PROJECT_DIR/$EVIDENCE_REL" 2>/dev/null || echo "(no evidence file)"
