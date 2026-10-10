#!/bin/sh
# Run the full test suite inside the pinned container.
set -eu
. "$(dirname "$0")/lib.sh"
docker_run cargo test "$@"
