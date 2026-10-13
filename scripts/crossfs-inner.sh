#!/bin/sh
# In-container half of the cross-filesystem determinism run.
#
# Invoked by scripts/run-crossfs.sh inside the pinned, memory-capped container.
# It discovers every writable filesystem target (recording its `fstype`), makes a
# best-effort loopback attempt at xfs/btrfs (recording the exact blocker when the
# kernel/privileges do not allow it), then runs the cross-FS test which writes
# evidence/bench/crossfs.json.
set -eu

TAB=$(printf '\t')
TARGETS=""
LOOP_NOTE="loopback xfs/btrfs"

# Record a directory as a target, labelled by its filesystem type.
add_target() {
    dir="$1"
    if ! mkdir -p "$dir" 2>/dev/null; then
        return 0
    fi
    if [ ! -w "$dir" ]; then
        return 0
    fi
    fs=$(stat -f -c %T "$dir" 2>/dev/null || echo unknown)
    TARGETS="${TARGETS}${fs}${TAB}${dir}
"
    echo "crossfs: target ${fs} at ${dir}"
}

# A best-effort loopback filesystem (needs mkfs tool + CAP_SYS_ADMIN to mount).
try_loop() {
    fs="$1"
    mkfs="$2"
    if ! command -v "$mkfs" >/dev/null 2>&1; then
        LOOP_NOTE="${LOOP_NOTE}; ${fs}: ${mkfs} not installed"
        return 0
    fi
    img="/tmp/loop-${fs}.img"
    if ! dd if=/dev/zero of="$img" bs=1M count=96 status=none 2>/dev/null; then
        LOOP_NOTE="${LOOP_NOTE}; ${fs}: could not create loop image"
        return 0
    fi
    if ! dev=$(losetup --find --show "$img" 2>/tmp/loop-err); then
        LOOP_NOTE="${LOOP_NOTE}; ${fs}: losetup denied ($(head -n1 /tmp/loop-err 2>/dev/null))"
        return 0
    fi
    if ! "$mkfs" -f "$dev" >/dev/null 2>&1; then
        LOOP_NOTE="${LOOP_NOTE}; ${fs}: ${mkfs} failed"
        losetup -d "$dev" 2>/dev/null || true
        return 0
    fi
    mnt="/mnt/${fs}"
    mkdir -p "$mnt"
    if ! mount "$dev" "$mnt" 2>/tmp/mount-err; then
        LOOP_NOTE="${LOOP_NOTE}; ${fs}: mount denied ($(head -n1 /tmp/mount-err 2>/dev/null))"
        losetup -d "$dev" 2>/dev/null || true
        return 0
    fi
    add_target "$mnt"
    LOOP_NOTE="${LOOP_NOTE}; ${fs}: mounted on loopback"
}

# Always-available targets: the container overlay (/tmp), the container tmpfs
# (/dev/shm) and the bind-mounted workspace (host ext4/ext-family).
add_target /tmp/ekb-crossfs
add_target /dev/shm/ekb-crossfs
add_target /work/target/ekb-crossfs

# Optional: install the loopback filesystem tools (opt-in; needs network).
if [ "${EKB_CROSSFS_LOOPBACK_INSTALL:-0}" = "1" ]; then
    if ! command -v mkfs.xfs >/dev/null 2>&1 || ! command -v mkfs.btrfs >/dev/null 2>&1; then
        echo "crossfs: installing xfsprogs/btrfs-progs (opt-in)"
        if apt-get update -qq >/dev/null 2>&1 && \
           apt-get install -y -qq --no-install-recommends xfsprogs btrfs-progs >/dev/null 2>&1; then
            echo "crossfs: loopback tools installed"
        else
            echo "crossfs: apt install failed (offline or no permission)"
        fi
    fi
fi

# Optional: a privileged container can mount loopback filesystems. The run script
# adds --privileged only when EKB_CROSSFS_PRIVILEGED=1; otherwise these attempts
# fail at losetup/mount and the exact blocker is recorded.
try_loop xfs mkfs.xfs
try_loop btrfs mkfs.btrfs

echo "crossfs: loopback note: ${LOOP_NOTE}"

export EKB_CROSSFS_TARGETS="$TARGETS"
export EKB_CROSSFS_LOOPBACK_NOTE="$LOOP_NOTE"
export EKB_CROSSFS_EVIDENCE="/work/evidence/bench/crossfs.json"

mkdir -p /work/evidence/bench
cargo test --release --test crossfs -- --nocapture --test-threads=1
