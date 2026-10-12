#!/bin/sh
# End-to-end EKBLOOM/OrFS proof on REAL data (the assessment's target milestone).
#
# One EKBLOOM/OrFS object is measured from creation through persistence, reopening
# and sustained use — including the actual on-disk bytes — and compared at an equal
# total on-disk budget against a raw file store and pinned-hot indexed LZ4/Zstandard
# file stores. Corpora are real files inside the container (an ELF executable, a
# text/licence corpus, and a mixed/binary aggregate).
#
# Produces:
#   evidence/bench/e2e.json — per corpus: source blake3/size, the chosen plan, the
#   real OrFS on-disk bytes, creation/reopen costs, sustained seq/hot/random/
#   scan-then-read latencies for EKBLOOM vs each comparison store, and the FUSE
#   end-to-end read latency (when a mount is available).
#
# When /dev/fuse is present the run uses the FUSE-capable image (Dockerfile.fuse) as
# the invoking user, so the FUSE section is measured and no root-owned files are
# left behind; otherwise the pinned verification container runs the same example and
# records that FUSE was skipped. Set EKB_E2E_MAX_BYTES to bound the per-category
# corpus size (default: the full caps in tests/common/e2e.rs). Nothing here runs
# project tooling on the host.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE_FUSE="ekbloom-rust-fuse:1.99.0"
EVIDENCE_REL="evidence/bench/e2e.json"
RUN_UID="$(id -u)"
RUN_GID="$(id -g)"

mkdir -p "$PROJECT_DIR/evidence/bench"

if [ -e /dev/fuse ]; then
    if ! docker image inspect "$IMAGE_FUSE" >/dev/null 2>&1; then
        echo "ekbloom: building FUSE image '$IMAGE_FUSE' from Dockerfile.fuse" >&2
        docker build -t "$IMAGE_FUSE" -f "$PROJECT_DIR/Dockerfile.fuse" "$PROJECT_DIR"
    fi
    docker run --rm \
        --device /dev/fuse \
        --cap-add SYS_ADMIN \
        --security-opt apparmor=unconfined \
        --memory=8g \
        --memory-swap=8g \
        --cpus=8 \
        --pids-limit=2048 \
        --ulimit core=0 \
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
        -e EKB_E2E_MAX_BYTES="${EKB_E2E_MAX_BYTES:-}" \
        -e EKB_E2E_EVIDENCE="/work/$EVIDENCE_REL" \
        "$IMAGE_FUSE" \
        sh -c 'printf "ekb:x:%s:%s:EKB:/tmp:/bin/sh\n" "$EKB_RUN_UID" "$EKB_RUN_GID" >> /etc/passwd; \
               exec setpriv --reuid="$EKB_RUN_UID" --regid="$EKB_RUN_GID" --clear-groups \
                   cargo run --release --features fuse --example e2e'
else
    echo "ekbloom: /dev/fuse is not present; the FUSE section will be skipped" >&2
    . "$PROJECT_DIR/scripts/lib.sh"
    docker_run env EKB_E2E_MAX_BYTES="${EKB_E2E_MAX_BYTES:-}" \
        cargo run --release --features fuse --example e2e
fi

echo "--- $EVIDENCE_REL ---"
sed -n '1,24p' "$PROJECT_DIR/$EVIDENCE_REL" 2>/dev/null || echo "(no evidence file)"
