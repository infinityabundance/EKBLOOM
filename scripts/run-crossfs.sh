#!/bin/sh
# Cross-filesystem determinism: write the SAME `.ekb` and stream container on every
# filesystem available in-container, then open each and assert `physical_bytes()`,
# sampled `byte_at` values and the file BLAKE3 are IDENTICAL (see tests/crossfs.rs).
#
# The available filesystems are discovered inside the container (their `fstype` is
# recorded): the container overlay (/tmp), the container tmpfs (/dev/shm) and the
# bind-mounted workspace (host ext4/ext-family). A best-effort loopback xfs/btrfs
# attempt is made; the exact blocker is recorded when the kernel/privileges do not
# allow it (the default container drops every capability, so `mount` is denied).
# Set EKB_CROSSFS_PRIVILEGED=1 to add --privileged and allow a real loopback mount.
#
# Writes evidence/bench/crossfs.json. Runs ONLY inside the pinned container.
# The x86_64-vs-aarch64 result is covered separately by scripts/container-aarch64.sh.
set -eu

. "$(dirname "$0")/lib.sh"

PRIV_FLAGS=""
if [ "${EKB_CROSSFS_PRIVILEGED:-0}" = "1" ]; then
    PRIV_FLAGS="--privileged"
    echo "crossfs: EKB_CROSSFS_PRIVILEGED=1 — allowing loopback mounts"
fi

echo "== cross-filesystem determinism (tmpfs / overlay / ext4 [ / loopback xfs,btrfs ]) =="
# shellcheck disable=SC2086
docker run --rm \
    --memory="$EKB_MEM_LIMIT" \
    --memory-swap="$EKB_MEMSWAP_LIMIT" \
    --cpus="$EKB_CPUS" \
    --pids-limit="$EKB_PIDS" \
    --ulimit core=0 \
    --cap-drop=ALL \
    --security-opt=no-new-privileges \
    $PRIV_FLAGS \
    -v "$PROJECT_DIR":/work \
    -w /work \
    -u "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME="$EKB_CARGO_HOME_CONTAINER" \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR="$EKB_TARGET_DIR_CONTAINER" \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    -e EKB_CROSSFS_LOOPBACK_INSTALL="${EKB_CROSSFS_LOOPBACK_INSTALL:-0}" \
    "$IMAGE" sh scripts/crossfs-inner.sh

echo "== wrote evidence/bench/crossfs.json =="
cat "$PROJECT_DIR/evidence/bench/crossfs.json"
