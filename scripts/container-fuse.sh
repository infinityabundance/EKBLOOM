#!/bin/sh
# Run the read-only FUSE smoke test in a device-capable container.
#
# FUSE needs /dev/fuse and privilege to mount, which the ordinary verification
# container deliberately does not have. This script builds and runs a separate
# image (Dockerfile.fuse) with:
#
#   --device /dev/fuse      pass the FUSE character device through
#   --cap-add SYS_ADMIN     allow the mount()/umount() syscalls
#   --security-opt apparmor=unconfined   in case an AppArmor profile blocks FUSE
#
# It runs the `tests/fuse.rs` integration test, which performs a real mount and
# unmounts on drop. Nothing here runs project tooling on the host.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE_FUSE="ekbloom-rust-fuse:1.99.0"

if [ ! -e /dev/fuse ]; then
    echo "ekbloom: /dev/fuse is not present on the host; FUSE cannot run here." >&2
    exit 2
fi

if ! docker image inspect "$IMAGE_FUSE" >/dev/null 2>&1; then
    echo "ekbloom: building FUSE image '$IMAGE_FUSE' from Dockerfile.fuse" >&2
    docker build -t "$IMAGE_FUSE" -f "$PROJECT_DIR/Dockerfile.fuse" "$PROJECT_DIR"
fi

# Default to the real smoke test with output visible.
if [ "$#" -eq 0 ]; then
    set -- cargo test --features fuse --test fuse -- --nocapture --test-threads=1
fi

# Keep cargo output off the host bind mount so the run cannot leave root-owned
# files in the repository. Cargo reads sources from the (lock-up-to-date) tree.
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
