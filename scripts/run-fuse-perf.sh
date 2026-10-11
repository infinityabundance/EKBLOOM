#!/bin/sh
# FUSE read-path study: measure the real cost of reading a materialised object
# through the read-only OrFs mount vs a direct read and a raw file read, and record
# the FUSE passthrough / FUSE-over-io_uring feasibility findings.
#
# Writes evidence/bench/fuse-perf.json. Reuses the dedicated FUSE image
# (Dockerfile.fuse) with:
#
#   --device /dev/fuse                    the FUSE character device
#   --cap-add SYS_ADMIN                   direct mount() when running as root
#   --security-opt apparmor=unconfined    in case an AppArmor profile blocks FUSE
#   -u "$(id -u):$(id -g)"                run as the invoking user; the setuid
#                                          fusermount3 helper performs the mount, so
#                                          the run leaves no root-owned files
#
# Runs as root only to install a passwd entry and drop to the invoking uid via
# setpriv; the setuid fusermount3 helper then performs the mount as that user, so
# the run leaves no root-owned files in the repository.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE_FUSE="ekbloom-rust-fuse:1.99.0"
EVIDENCE_REL="evidence/bench/fuse-perf.json"
RUN_UID="$(id -u)"
RUN_GID="$(id -g)"

if [ ! -e /dev/fuse ]; then
    echo "ekbloom: /dev/fuse is not present on the host; FUSE cannot run here." >&2
    exit 2
fi

if ! docker image inspect "$IMAGE_FUSE" >/dev/null 2>&1; then
    echo "ekbloom: building FUSE image '$IMAGE_FUSE' from Dockerfile.fuse" >&2
    docker build -t "$IMAGE_FUSE" -f "$PROJECT_DIR/Dockerfile.fuse" "$PROJECT_DIR"
fi

mkdir -p "$PROJECT_DIR/evidence/bench"

# Reuse the project's cargo cache and target dir so the run needs no network and
# leaves no root-owned files (the container drops to the invoking uid before
# running cargo).
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
    -e CARGO_HOME=/work/.cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR=/work/target \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    -e EKB_RUN_UID="$RUN_UID" \
    -e EKB_RUN_GID="$RUN_GID" \
    -e EKB_FUSE_PERF_EVIDENCE="/work/$EVIDENCE_REL" \
    "$IMAGE_FUSE" \
    sh -c 'printf "ekb:x:%s:%s:EKB:/tmp:/bin/sh\n" "$EKB_RUN_UID" "$EKB_RUN_GID" >> /etc/passwd; \
           exec setpriv --reuid="$EKB_RUN_UID" --regid="$EKB_RUN_GID" --clear-groups \
               cargo run --release --features fuse --example fuse_read'

echo "--- $EVIDENCE_REL ---"
sed -n '1,12p' "$PROJECT_DIR/$EVIDENCE_REL" 2>/dev/null || echo "(no evidence file)"
