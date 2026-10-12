#!/bin/sh
# OrFS on-disk footprint: the real bytes the concealed namespace occupies, next to
# a single `.ekb`, a raw file, and pinned-hot LZ4 / Zstandard stores at one equal
# budget, with the in-memory engine number stated alongside.
#
# Produces:
#   evidence/bench/orfs.json — per corpus (skewed, mixed; 4 MiB each):
#   the in-memory manifested footprint (`EkbloomBody::physical_bytes()`), the
#   actual OrFS object bytes (everything under `.ekbloom/` plus the logical
#   pointer artifact), a single `.ekb`, a raw file, and pinned-hot LZ4/Zstandard
#   footprints — plus the exact excess of the OrFS disk number over the in-memory
#   engine number.
#
# Runs only inside the pinned, memory-capped container (see scripts/lib.sh). The
# two 4 MiB corpora stay well inside the 8 GiB cap.
set -eu
. "$(dirname "$0")/lib.sh"

mkdir -p "$PROJECT_DIR/evidence/bench"
docker_run cargo run --release --example orfs_disk
