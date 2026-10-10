# EKBLOOM build/test container.
#
# Base image is the official Docker Hub Rust slim image, pinned by digest.
# Pinned in scripts/lib.sh as IMAGE_DIGEST; keep in sync.
FROM rust:1.99.0-slim-bookworm@sha256:2c3a22f0a5533ea2dd5a16627bc841228151faa2d4de2644ac9987e4a2f1f2fa

# Minimal tooling: no build-essential bloat. We only need the Rust toolchain
# that already ships in the base image, plus a few utilities for evidence.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        coreutils \
        file \
    ; \
    rm -rf /var/lib/apt/lists/*

# The slim image ships only cargo/rustc/std; add the components the verify gate needs.
RUN rustup component add rustfmt clippy

# Non-root user.
RUN set -eux; \
    groupadd -g 10001 ekb; \
    useradd -u 10001 -g 10001 -m -s /usr/sbin/nologin ekb

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_TERM_COLOR=never \
    RUSTFLAGS=""

WORKDIR /work
USER 10001:10001

CMD ["cargo", "build", "--release"]
