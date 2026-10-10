#!/bin/sh
# Miri (undefined-behaviour) run for EKBLOOM — runs ONLY inside a pinned nightly
# container, never on the host.
#
# Pinned image (see scripts/container-fuzz.sh for how the digest was resolved):
#   rustlang/rust:nightly-bookworm-slim@sha256:2fe113...
#
# Miri is added with `rustup component add miri`, which writes into the image's
# system rustup home; the container therefore runs as root and keeps every build
# artifact in `/tmp` so the mounted source tree is never written to.
#
# A bounded subset of the fast, pure library unit tests is selected to keep the
# runtime and memory footprint small.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

NIGHTLY_IMAGE="rustlang/rust:nightly-bookworm-slim@sha256:2fe113dd3b9f9f36f3993f3720e9dbd16ddf9bd1365e8ef6a4b7b2dc316d13d6"

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-8g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-8g}"

# Bounded subset: the machine module's fast unit tests.
MIRI_FILTER="${EKB_MIRI_FILTER:-machine::}"

echo "ekbloom-miri: pulling pinned nightly image ($NIGHTLY_IMAGE)"
docker pull "$NIGHTLY_IMAGE" >/dev/null

echo "ekbloom-miri: running 'cargo miri test --lib -- $MIRI_FILTER' (memory-capped)"
docker run --rm \
    --memory="$EKB_MEM_LIMIT" \
    --memory-swap="$EKB_MEMSWAP_LIMIT" \
    --ulimit core=0 \
    --cap-drop=ALL \
    --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -w /work \
    -e HOME=/tmp \
    -e CARGO_HOME=/tmp/cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e RUSTUP_TOOLCHAIN=nightly \
    -e CARGO_TARGET_DIR=/tmp/miri-target \
    -e RUST_BACKTRACE=1 \
    -e CARGO_TERM_COLOR=never \
    "$NIGHTLY_IMAGE" sh -c '
        set -eu
        rustup component add miri
        cargo miri test --locked --lib -- '"$MIRI_FILTER"'
    '
