#!/bin/sh
# Format the crate inside the pinned container (rustfmt), rewriting in place.
set -eu
. "$(dirname "$0")/lib.sh"
docker_run cargo fmt --all
