#!/bin/sh
# REGENERATE the frozen wire-format compatibility fixtures.
#
#   !!  ONLY run this when DELIBERATELY minting a new format version.  !!
#
# `tests/fixtures/*.ekb` are frozen artifacts written by OLDER releases. Current
# code must open them byte-identically (`tests/compat_fixtures.rs`). Rewriting
# them silently would destroy the compatibility guarantee this repository makes:
# the whole point is that the bytes were produced by another build and are never
# touched again. A format change requires NEW fixtures plus a MIGRATION entry —
# see `docs/MIGRATION.md` §6 "Frozen fixtures".
#
# This script checks out the two release commits that authored the current wire
# line into git worktrees, compiles `examples/emit_fixtures.rs` inside each with
# that release's own `create_machine` / `write_body`, and writes the resulting
# `.ekb` files plus one expectation fragment per release. It then merges the
# fragments into `tests/fixtures/expected.json`.
#
# Hard rule: project tooling runs ONLY inside the pinned container. This script
# is a host-side orchestrator that drives `docker_run`; every container is
# memory-capped (4g/4g).
set -eu

# Cap every docker run at 4 GiB before lib.sh reads its defaults.
export EKB_MEM_LIMIT=4g
export EKB_MEMSWAP_LIMIT=4g
. "$(dirname "$0")/lib.sh"

OUT="$PROJECT_DIR/tests/fixtures"
EMITTER="$PROJECT_DIR/examples/emit_fixtures.rs"
WT_ROOT="$PROJECT_DIR/target/fixture-worktrees"

# release commit, worktree name, file prefix, writer label
COMMIT_040=8a9c217
COMMIT_030=b26a165
LABEL_040=0.4.0
LABEL_030=0.3.0
PREFIX_040=v0.4_
PREFIX_030=v0.3_

mkdir -p "$OUT"
# Remove only the artifacts this script owns, so a partial failure is obvious.
rm -f "$OUT"/*.ekb "$OUT"/expected.json "$OUT"/expected.*.jsonl

rm -rf "$WT_ROOT"
# Clear any stale worktree registrations left by an interrupted run.
git -C "$PROJECT_DIR" worktree prune
mkdir -p "$WT_ROOT"

emit_release() {
    commit="$1"
    name="$2"
    prefix="$3"
    label="$4"
    wt="$WT_ROOT/$name"
    echo "== emitting fixtures from release $label (commit $commit) =="
    git -C "$PROJECT_DIR" worktree add -f --detach "$wt" "$commit"
    mkdir -p "$wt/src/bin"
    cp "$EMITTER" "$wt/src/bin/emit_fixtures.rs"
    # Separate target dir per release so the old and current builds never mix.
    docker_run sh -c "cd /work/target/fixture-worktrees/$name && \
CARGO_TARGET_DIR=/work/target/fixture-build/$label \
cargo run --release --locked --offline --bin emit_fixtures -- \
/work/tests/fixtures $prefix $label"
}

emit_release "$COMMIT_040" v0.4.0 "$PREFIX_040" "$LABEL_040"
emit_release "$COMMIT_030" v0.3.0 "$PREFIX_030" "$LABEL_030"

echo "== merging expectation fragments into expected.json =="
expected="$OUT/expected.json"
printf '{\n  "fixtures": [\n' >"$expected"
first=1
for label in "$LABEL_040" "$LABEL_030"; do
    frag="$OUT/expected.$label.jsonl"
    if [ ! -f "$frag" ]; then
        echo "regen-fixtures: missing fragment $frag" >&2
        exit 1
    fi
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        if [ "$first" -eq 0 ]; then
            printf ',\n' >>"$expected"
        fi
        printf '    %s' "$line" >>"$expected"
        first=0
    done <"$frag"
done
printf '\n  ]\n}\n' >>"$expected"
rm -f "$OUT"/expected.*.jsonl

echo "== removing worktrees =="
git -C "$PROJECT_DIR" worktree remove --force "$WT_ROOT/v0.4.0" || true
git -C "$PROJECT_DIR" worktree remove --force "$WT_ROOT/v0.3.0" || true
rm -rf "$WT_ROOT"

echo "== done: $(ls "$OUT"/*.ekb | wc -l) fixtures, expected.json written =="
