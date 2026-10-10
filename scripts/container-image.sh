#!/bin/sh
# Build the pinned project container image from ./Dockerfile.
set -eu
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="ekbloom-rust:1.99.0"
cd "$PROJECT_DIR"
docker build -t "$IMAGE" -f Dockerfile .
echo -n "image id: "
docker inspect --format '{{.Id}}' "$IMAGE"
