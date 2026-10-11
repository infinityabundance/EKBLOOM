#!/bin/sh
# Exercise the completion-driven io_uring publication backend in a container
# whose seccomp policy permits io_uring_setup(2).
#
# The ordinary verification container (scripts/lib.sh `docker_run`) uses Docker's
# default seccomp profile, which denies io_uring_setup(2) with EPERM on most
# modern kernels. This script uses the same pinned image and the same memory/pid
# limits and then relaxes only that one policy:
#
#   --security-opt seccomp=unconfined     allow io_uring_setup(2)
#
# io_uring_setup(2) needs no Linux capability, so --cap-drop=ALL is retained. The
# script first records what the default profile does (the exact errno), then runs
# with the relaxed profile. If even that cannot use io_uring it retries once with
# --privileged as a last resort, and records which flags were needed (or the exact
# errno that blocks it) in evidence/bench/io-uring.json. Nothing here runs project
# tooling on the host.
set -eu

. "$(dirname "$0")/lib.sh"

EVIDENCE_REL="evidence/bench/io-uring.json"
EVIDENCE_ABS="$PROJECT_DIR/$EVIDENCE_REL"

# run_backend <label> <extra docker flags> <flags recorded in evidence>
run_backend() {
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
        -e EKB_IOURING_EVIDENCE="/work/$EVIDENCE_REL" \
        -e EKB_IOURING_CONTAINER_FLAGS="$recorded" \
        "$IMAGE" \
        cargo test --features io-uring --test async_io -- --nocapture --test-threads=1
}

show_evidence() {
    echo "--- $EVIDENCE_REL ---"
    cat "$EVIDENCE_ABS" 2>/dev/null || echo "(no evidence file)"
    echo
}

# Stage 0: baseline under Docker's default seccomp profile. The async path must
# fall back cleanly here; the recorded probe_error is the exact errno.
if run_backend \
    "stage 0: io_uring probe under Docker default seccomp (expect denial)" \
    "" \
    "--cap-drop=ALL --security-opt=no-new-privileges (Docker default seccomp)"; then
    show_evidence
fi

usable=false

# Stage 1: relax seccomp only. This is the intended fix.
if run_backend \
    "stage 1: --security-opt seccomp=unconfined" \
    "--security-opt seccomp=unconfined" \
    "--cap-drop=ALL --security-opt=no-new-privileges --security-opt seccomp=unconfined"; then
    if grep -q '"io_uring_usable": true' "$EVIDENCE_ABS" 2>/dev/null; then
        usable=true
        echo "io_uring: USABLE with --security-opt seccomp=unconfined (no capability, no --privileged needed)"
    fi
fi

# Stage 2: last resort only if stage 1 did not work.
if [ "$usable" != "true" ]; then
    echo "io_uring: still unavailable under seccomp=unconfined; retrying with --privileged (last resort)"
    if run_backend \
        "stage 2: --privileged" \
        "--privileged" \
        "--privileged"; then
        if grep -q '"io_uring_usable": true' "$EVIDENCE_ABS" 2>/dev/null; then
            usable=true
            echo "io_uring: USABLE only with --privileged"
        fi
    fi
fi

echo "== final evidence: $EVIDENCE_REL =="
cat "$EVIDENCE_ABS" 2>/dev/null || echo "(no evidence file was written)"

if [ "$usable" != "true" ]; then
    echo
    echo "RESULT: io_uring is compile-verified but NOT runtime-verified in this environment."
    echo "        The async path is real, but the kernel/seccomp policy denied"
    echo "        io_uring_setup(2); see probe_error above for the exact errno."
    echo "        The synchronous fallback remains the measured path."
fi
