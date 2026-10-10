#!/bin/sh
# Reproducible clean build + test + environment capture from a pristine checkout.
#
# Removes build artifacts, rebuilds from Cargo.lock, runs the correctness suite,
# and writes an environment record under evidence/.
set -eu
. "$(dirname "$0")/lib.sh"

echo "== capturing environment =="
mkdir -p "$PROJECT_DIR/evidence"
{
    echo "image: $IMAGE"
    docker_run rustc -Vv
    docker_run cargo -V
    docker_run sh -c 'uname -a'
    docker_run sh -c 'nproc; grep -m1 "model name" /proc/cpuinfo || true'
    if [ -f "$PROJECT_DIR/Cargo.lock" ]; then
        echo -n "cargo.lock sha256: "
        sha256sum "$PROJECT_DIR/Cargo.lock" | cut -d' ' -f1
    fi
    if command -v git >/dev/null 2>&1 && [ -d "$PROJECT_DIR/.git" ]; then
        git -C "$PROJECT_DIR" rev-parse HEAD 2>/dev/null || echo "commit: (none)"
    fi
} | tee "$PROJECT_DIR/evidence/environment.txt"

echo "== clean build =="
rm -rf "$PROJECT_DIR/target"
docker_run cargo build --release --locked

echo "== tests =="
docker_run cargo test --release --locked

echo "== reproduce complete =="
