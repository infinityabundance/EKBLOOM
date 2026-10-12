#!/bin/sh
# Run the read-only FUSE smoke test (and the read-path performance study) in a
# device-capable container.
#
# FUSE needs /dev/fuse and privilege to mount, which the ordinary verification
# container deliberately does not have. This script builds and runs a separate
# image (Dockerfile.fuse) with:
#
#   --device /dev/fuse      pass the FUSE character device through
#   --cap-add SYS_ADMIN     allow the mount()/umount() syscalls
#   --security-opt apparmor=unconfined   in case an AppArmor profile blocks FUSE
#
# With no arguments it runs, in order:
#   1. the `tests/fuse.rs` smoke test (real mount + unmount);
#   2. `examples/fuse_read.rs`, which performs a real mount as the invoking user
#      and writes evidence/bench/fuse-perf.json (backing-surface vs decode vs
#      direct read, plus the passthrough probe).
#
# Any arguments replace the default command and are run verbatim in the image.
# Nothing here runs project tooling on the host.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE_FUSE="ekbloom-rust-fuse:1.99.0"
EVIDENCE_REL="evidence/bench/fuse-perf.json"

if [ ! -e /dev/fuse ]; then
    echo "ekbloom: /dev/fuse is not present on the host; FUSE cannot run here." >&2
    exit 2
fi

if ! docker image inspect "$IMAGE_FUSE" >/dev/null 2>&1; then
    echo "ekbloom: building FUSE image '$IMAGE_FUSE' from Dockerfile.fuse" >&2
    docker build -t "$IMAGE_FUSE" -f "$PROJECT_DIR/Dockerfile.fuse" "$PROJECT_DIR"
fi

# Run an explicit command in the FUSE image (root; used for the smoke test, whose
# cargo target lives off the bind mount so no root-owned files enter the repo).
run_fuse() {
    docker run --rm \
        --device /dev/fuse \
        --cap-add SYS_ADMIN \
        --security-opt apparmor=unconfined \
        --memory=8g \
        --memory-swap=8g \
        --cpus=8 \
        --pids-limit=2048 \
        -v "$PROJECT_DIR":/work \
        -w /work \
        -e HOME=/tmp \
        -e CARGO_HOME=/usr/local/cargo \
        -e RUSTUP_HOME=/usr/local/rustup \
        -e CARGO_TARGET_DIR=/tmp/ekbloom-fuse-target \
        -e CARGO_TERM_COLOR=never \
        "$IMAGE_FUSE" \
        "$@"
}

# Explicit arguments override the default.
if [ "$#" -gt 0 ]; then
    run_fuse "$@"
    exit 0
fi

echo "ekbloom: FUSE smoke test (tests/fuse.rs)"
run_fuse cargo test --features fuse --test fuse -- --nocapture --test-threads=1

# The perf study needs the evidence file to be owned by the invoking user (it is
# written into the bind-mounted repo), so it drops to that uid via setpriv; see
# scripts/run-fuse-perf.sh, which owns that logic.
echo "ekbloom: FUSE read-path study (examples/fuse_read.rs)"
sh "$PROJECT_DIR/scripts/run-fuse-perf.sh"

echo "--- $EVIDENCE_REL ---"
sed -n '1,12p' "$PROJECT_DIR/$EVIDENCE_REL" 2>/dev/null || echo "(no evidence file)"
