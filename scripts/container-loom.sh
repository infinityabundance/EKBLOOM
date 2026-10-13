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
#
# Two model files compile the SAME production code against loom's instrumented
# `Mutex` (src/filesystem.rs, src/handle.rs) and are run here:
#   * tests/loom.rs      — the OrFs index check-then-publish critical section.
#   * tests/loom_rcu.rs  — the RCU handle cache: generation publication, the
#                          single-flight builder lock, and index-publish vs the
#                          lock-free read of an already-open object.
set -eu

# Loom is lightweight (single OS thread, bounded schedules); keep the OOM cap
# tight at 4 GiB as the directive requires. Export before sourcing lib.sh so the
# memory cap every docker_run applies picks these up.
EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-4g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-4g}"
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

LOOM_MAX_PREEMPTIONS="${EKB_LOOM_MAX_PREEMPTIONS:-2}"

echo "ekbloom-loom: RUSTFLAGS='--cfg loom' cargo test --test <loom|loom_rcu> --release"
echo "ekbloom-loom: LOOM_MAX_PREEMPTIONS=$LOOM_MAX_PREEMPTIONS (memory-capped at $EKB_MEM_LIMIT)"
echo "ekbloom-loom: models = tests/loom.rs (OrFs index) + tests/loom_rcu.rs (handle/generation RCU)"

# `env` overlays the two vars for cargo only; everything else (memory cap,
# pid cap, dropped capabilities, non-privileged user) comes from lib.sh.
# `--test-threads=1` runs the models sequentially (loom's scheduler is a scoped
# per-process runtime) and `--nocapture` surfaces each model's interleaving count.
run_loom() {
    target="$1"
    shift
    docker_run env \
        "LOOM_MAX_PREEMPTIONS=$LOOM_MAX_PREEMPTIONS" \
        "RUSTFLAGS=--cfg loom" \
        cargo test --test "$target" --release -- --nocapture --test-threads=1 "$@"
}

run_loom loom "$@"
run_loom loom_rcu "$@"
