#!/bin/sh
# Standard-corpora court — EKBLOOM on real standard data, at an equal budget.
#
# Downloads and extracts the standard corpora (Silesia, enwik8, Calgary) INSIDE a
# memory-capped container, outside the repository, then compares EKBLOOM (the best
# body the joint optimiser admits under a budget equal to the raw size) against
# block-indexed LZ4, chunked zstd-19/zstd-3, a raw Vec, and pinned-hot LZ4/zstd-19.
#
# The corpora themselves are NEVER committed: only their hashes and the measured
# results (evidence/bench/corpora.json). The download directory is a host temp
# dir, bind-mounted read-write for the download container and read-only for the
# run, and removed on exit.
#
# Downloads are best-effort: a corpus that cannot be fetched is skipped and the
# remaining corpora (plus the container's own text + ELF corpora, which are always
# present) are still measured. Every docker run is memory-capped at 6 GiB.
#
# Run: sh scripts/run-corpora.sh
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="ekbloom-rust:1.99.0"
TOOLS_IMAGE="ekbloom-corpus-tools:1.99.0"
MEM="6g"
CPUS="8"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ekbloom: project image '$IMAGE' not found; build it with scripts/container-image.sh" >&2
    exit 1
fi

# --- 1. build the download/extract helper image if missing ---------------------
if ! docker image inspect "$TOOLS_IMAGE" >/dev/null 2>&1; then
    echo "== building helper image $TOOLS_IMAGE =="
    BUILD_DIR="$(mktemp -d)"
    trap 'rm -rf "$BUILD_DIR"' EXIT INT TERM
    {
        echo "FROM $IMAGE"
        echo "USER root"
        echo 'RUN set -eux; apt-get update; \'
        echo '    apt-get install -y --no-install-recommends curl tar gzip unzip ca-certificates; \'
        echo '    rm -rf /var/lib/apt/lists/*; command -v curl unzip'
    } > "$BUILD_DIR/Dockerfile"
    docker build -t "$TOOLS_IMAGE" "$BUILD_DIR"
    rm -rf "$BUILD_DIR"
    trap - EXIT INT TERM
fi

# --- 2. download + extract the standard corpora outside the repo ---------------
CORPUS_DIR="$(mktemp -d)"
cleanup() { rm -rf "$CORPUS_DIR"; }
trap cleanup EXIT INT TERM

cat > "$CORPUS_DIR/download.sh" <<'EOS'
set -u
cd /tmp
mkdir -p /corpora/silesia /corpora/enwik8 /corpora/calgary
: > /corpora/archives.sha256
fetch() {
    url="$1"; name="$2"; sub="$3"
    if curl -fsSL --retry 3 --max-time 900 -o "/tmp/$name" "$url"; then
        sha256sum "$name" >> /corpora/archives.sha256
        if unzip -o -q "/tmp/$name" -d "/corpora/$sub"; then
            echo "ok: $name -> $sub"
        else
            echo "WARN: could not extract $name" >&2
        fi
    else
        echo "WARN: download failed: $url" >&2
    fi
}
fetch "https://mattmahoney.net/dc/silesia.zip" silesia.zip silesia
fetch "https://mattmahoney.net/dc/enwik8.zip"  enwik8.zip  enwik8
fetch "https://mattmahoney.net/dc/calgary.zip" calgary.zip calgary
echo "--- archive hashes ---"
cat /corpora/archives.sha256
EOS

echo "== downloading + extracting standard corpora (bounded, inside the container) =="
docker run --rm \
    --memory="$MEM" --memory-swap="$MEM" --cpus="$CPUS" \
    --pids-limit=512 --ulimit core=0 \
    --cap-drop=ALL --security-opt=no-new-privileges \
    -v "$CORPUS_DIR":/corpora \
    -w /tmp -u "$(id -u):$(id -g)" -e HOME=/tmp \
    "$TOOLS_IMAGE" sh /corpora/download.sh || true

# --- 3. run the corpora court (pinned project image, 6 GiB cap) -----------------
echo "== running examples/corpora =="
mkdir -p "$PROJECT_DIR/evidence/bench"
docker run --rm \
    --memory="$MEM" --memory-swap="$MEM" --cpus="$CPUS" \
    --pids-limit=2048 --ulimit core=0 \
    --cap-drop=ALL --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -v "$CORPUS_DIR":/corpora:ro \
    -w /work -u "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME=/work/.cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR=/work/target \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    -e EKB_CORPORA_DIR=/corpora \
    -e EKB_CORPORA_MANIFEST=/corpora/archives.sha256 \
    -e EKB_CORPORA_CAP="${EKB_CORPORA_CAP:-}" \
    "$IMAGE" cargo run --release --example corpora

echo "--- evidence/bench/corpora.json (head) ---"
sed -n '1,12p' "$PROJECT_DIR/evidence/bench/corpora.json" 2>/dev/null || echo "(no evidence file)"

# --- 4. concurrency p99: the lock-free read path under publication pressure ---
echo "== running examples/concurrency_p99 =="
docker run --rm \
    --memory="$MEM" --memory-swap="$MEM" --cpus="$CPUS" \
    --pids-limit=4096 --ulimit core=0 \
    --cap-drop=ALL --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -w /work -u "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME=/work/.cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_TARGET_DIR=/work/target \
    -e CARGO_TERM_COLOR=never \
    -e RUSTUP_TOOLCHAIN=1.99.0 \
    "$IMAGE" cargo run --release --example concurrency_p99

echo "--- evidence/bench/concurrency.json ---"
cat "$PROJECT_DIR/evidence/bench/concurrency.json" 2>/dev/null || echo "(no evidence file)"
