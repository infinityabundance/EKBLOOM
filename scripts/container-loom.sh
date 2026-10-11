#!/bin/sh
# Loom model-checking run for EKBLOOM — runs ONLY inside the pinned container,
# never on the host.
#
# Loom needs the `--cfg loom` build (it works on stable; no nightly required) and
# the target-gated `loom` dependency declared in Cargo.toml. The pre-emption bound
# keeps exploration finite: loom deterministically explores every interleaving
# with at most N thread pre-emptions (equivalently, every schedule up to N
# forcible thread switches). 2 is enough to catch the great majority of bugs
# while bounding the run; raise it with EKB_LOOM_MAX_PREEMPTIONS for a deeper
# sweep. See https://docs.rs/loom for the model and its caveats.
set -eu
. "$(dirname "$0")/lib.sh"

LOOM_MAX_PREEMPTIONS="${EKB_LOOM_MAX_PREEMPTIONS:-2}"

echo "ekbloom-loom: RUSTFLAGS='--cfg loom' cargo test --test loom --release"
echo "ekbloom-loom: LOOM_MAX_PREEMPTIONS=$LOOM_MAX_PREEMPTIONS (memory-capped at $EKB_MEM_LIMIT)"

# `env` overlays the two vars for cargo only; everything else (memory cap,
# pid cap, dropped capabilities, non-privileged user) comes from lib.sh.
docker_run env \
    "LOOM_MAX_PREEMPTIONS=$LOOM_MAX_PREEMPTIONS" \
    "RUSTFLAGS=--cfg loom" \
    cargo test --test loom --release "$@"
