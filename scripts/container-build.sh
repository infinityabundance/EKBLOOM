#!/bin/sh
# Build the crate inside the pinned container.
set -eu
. "$(dirname "$0")/lib.sh"
docker_run cargo build "$@"
