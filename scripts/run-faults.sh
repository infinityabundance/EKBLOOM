#!/bin/sh
# Transactional-publication fault injection (TASK 1).
#
# Runs tests/faults.rs, which injects a failing `Write` (WriteZero / ENOSPC / EIO)
# after every byte offset into `StreamBuilder::write`/`checkpoint`/`finish`, and the
# byte-exact crash image into `writer::append_generation*`, and the filesystem
# boundary cases into `writer::write_body_authenticated`. Asserted invariants:
# (a) no partially-visible generation is ever readable, (b) the previous committed
# generation stays intact and byte-identical, (c) recovery restores a clean state.
#
# Writes evidence/bench/faults.json (must be a 100 % pass rate). Runs ONLY inside
# the pinned, memory-capped container.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# OOM / fork-bomb protection (mirrors scripts/lib.sh conventions).
EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-8g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-8g}"
EKB_CPUS="${EKB_CPUS:-8}"
EKB_PIDS="${EKB_PIDS:-2048}"

IMAGE="ekbloom-rust:1.99.0"
EKB_CARGO_HOME_CONTAINER="/work/.cargo-home"
EKB_TARGET_DIR_CONTAINER="/work/target"
EVIDENCE_REL="evidence/bench/faults.json"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ekbloom: project image '$IMAGE' not found." >&2
    echo "Build it with: docker build -t $IMAGE -f Dockerfile ." >&2
    exit 1
fi

mkdir -p "$PROJECT_DIR/evidence/bench"
echo "== fault injection: stream write/checkpoint/finish, append, publish =="
docker run --rm \
    --memory="$EKB_MEM_LIMIT" \
    --memory-swap="$EKB_MEMSWAP_LIMIT" \
    --cpus="$EKB_CPUS" \
    --pids-limit="$EKB_PIDS" \
    --ulimit core=0 \
    --cap-drop=ALL \
    --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -w /work \
    -u "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME="$EKB_CARGO_HOME_CONTAINER" \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR="$EKB_TARGET_DIR_CONTAINER" \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    -e EKB_FAULTS_EVIDENCE="/work/$EVIDENCE_REL" \
    "$IMAGE" \
    cargo test --release --test faults -- --nocapture --test-threads=1

echo "== wrote $EVIDENCE_REL =="
cat "$PROJECT_DIR/$EVIDENCE_REL"
