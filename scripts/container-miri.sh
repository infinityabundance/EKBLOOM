#!/bin/sh
# Miri (undefined-behaviour) run for EKBLOOM — runs ONLY inside a pinned nightly
# container, never on the host.
#
# Pinned image (see scripts/container-fuzz.sh for how the digest was resolved):
#   rustlang/rust:nightly-bookworm-slim@sha256:2fe113...
#
# Miri is added with `rustup component add miri`, which writes into the image's
# system rustup home; the container therefore runs as root and keeps every build
# artifact in `/tmp` so the mounted source tree is never written to.
#
# A broadened but bounded subset of the fast, pure library unit tests is selected
# to keep the runtime and memory footprint small while covering every machine and
# codec:
#
#   * machines 1-8 and the decode paths -> the eight explicit machine-module
#     prefixes (machine::, machine_pack::, machine_seg::, machine_store::,
#     machine_rans::, machine_rans_i::, machine_rans_il::, machine_rans_w::) plus
#     blocks:: (machine-1/2 decode);
#   * stream / checkpoint / bloom / resource unit tests.
#
# The prefixes are EXPLICIT module qualifiers, not the bare substring `machine`.
# A bare `machine` filter also matches `filesystem::tests::
# read_machine_reconstructs_ignoring_manifested_structures` and
# `fuse::tests::machine_mode_forces_reconstruction_and_attaches_no_surface`, whose
# `temp_root` helpers call `SystemTime::now()` -> `clock_gettime(CLOCK_REALTIME)`,
# which Miri's default isolation rejects. Those filesystem/fuse tests are covered
# by the normal suite, not by Miri.
#
# The three sanctioned `unsafe` modules are EXPLICITLY EXCLUDED (no filter below
# matches them): `simd::` (Miri cannot execute the x86 AVX2/AVX-512/SSE
# intrinsics), `async_io::` (feature-gated; `io_uring` ring syscalls) and
# `mapped::` (`mmap`/`madvise`/`munmap` libc calls Miri does not model). They
# remain covered by `tests/simd.rs`, `tests/unsafe_review.rs`, `tests/mmap.rs` and
# the review in docs/reviews/security.md.
set -eu

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

NIGHTLY_IMAGE="rustlang/rust:nightly-bookworm-slim@sha256:2fe113dd3b9f9f36f3993f3720e9dbd16ddf9bd1365e8ef6a4b7b2dc316d13d6"

EKB_MEM_LIMIT="${EKB_MEM_LIMIT:-8g}"
EKB_MEMSWAP_LIMIT="${EKB_MEMSWAP_LIMIT:-8g}"

# libtest matches a test iff its name contains ANY of these substrings (OR).
# Explicit module prefixes only (see above): no `async_io::`, `mapped::`, `simd::`,
# and no filesystem/fuse clock-dependent test.
MIRI_FILTER="${EKB_MIRI_FILTER:-machine:: machine_pack:: machine_seg:: machine_store:: machine_rans:: machine_rans_i:: machine_rans_il:: machine_rans_w:: blocks:: stream:: checkpoint:: bloom resource::}"

# Three unit tests have loop/allocation bounds that are Miri-infeasible (Miri is
# an interpreter, ~100-1000x slower): they build/parse 100k-300k-byte streams or
# replay every position at every checkpoint interval. They remain fully covered
# by `sh scripts/container-verify.sh`; here they are skipped by exact name so the
# broadened Miri pass stays bounded.
MIRI_SKIP="${EKB_MIRI_SKIP:---skip stream::tests::roundtrip_various_sizes --skip stream::tests::truncated_and_crafted_are_typed_errors --skip checkpoint::tests::restore_matches_state_log_all_intervals}"

# Deterministic: the `resource::` unit tests mutate the process-wide default
# allocation ceiling (an `AtomicU64`), so serialise the harness to keep every
# machine/stream/bloom test off a temporarily-lowered cap.
MIRI_TEST_THREADS="${EKB_MIRI_TEST_THREADS:-1}"

echo "ekbloom-miri: pulling pinned nightly image ($NIGHTLY_IMAGE)"
docker pull "$NIGHTLY_IMAGE" >/dev/null

echo "ekbloom-miri: running 'cargo miri test --lib -- --test-threads=$MIRI_TEST_THREADS $MIRI_SKIP $MIRI_FILTER' (memory-capped)"
docker run --rm \
    --memory="$EKB_MEM_LIMIT" \
    --memory-swap="$EKB_MEMSWAP_LIMIT" \
    --ulimit core=0 \
    --cap-drop=ALL \
    --security-opt=no-new-privileges \
    -v "$PROJECT_DIR":/work \
    -w /work \
    -e HOME=/tmp \
    -e CARGO_HOME=/tmp/cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e RUSTUP_TOOLCHAIN=nightly \
    -e CARGO_TARGET_DIR=/tmp/miri-target \
    -e RUST_BACKTRACE=1 \
    -e CARGO_TERM_COLOR=never \
    "$NIGHTLY_IMAGE" sh -c '
        set -eu
        rustup component add miri
        cargo miri test --locked --lib -- --test-threads='"$MIRI_TEST_THREADS"' '"$MIRI_SKIP"' '"$MIRI_FILTER"'
    '
