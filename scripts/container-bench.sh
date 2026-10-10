#!/bin/sh
# Run benchmarks inside the pinned container.
set -eu
. "$(dirname "$0")/lib.sh"
docker_run cargo bench "$@"
