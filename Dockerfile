# Official kevy server image — pure-Rust, zero-dep, Redis-compatible KV.
#
# Build context = repo root. Two stages:
#   1. build  — rust:1.95-slim-bookworm, cargo build --release --bin kevy
#   2. runtime — debian:bookworm-slim, just the binary
#
# Default: bind 0.0.0.0:6379 (Redis-default port — drop-in for clients), AOF
# on, data dir /data. Override with KEVY_BIND / KEVY_PORT / KEVY_DIR /
# KEVY_AOF / KEVY_THREADS or by passing CLI flags after the entrypoint.
#
# Quick start (host port 6379 → container 6379, persistent volume):
#   docker run -d --name kevy -p 6379:6379 -v kevy-data:/data ghcr.io/goliajp/kevy
#
# kevy auto-selects io_uring on Linux when available (kernel >= 5.19), else
# falls back to epoll — startup never fails. Docker's default seccomp blocks
# io_uring_setup, so the default run uses epoll; allow io_uring for the faster
# reactor (or force it with -e KEVY_IO_URING=1, force epoll with =0):
#   docker run --rm -p 6379:6379 \
#     --security-opt seccomp=unconfined ghcr.io/goliajp/kevy

FROM rust:1.95-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Use the image's bundled toolchain (1.95) — we deliberately don't copy
# rust-toolchain.toml so the build skips a redundant `rustup` download.
# Builds BOTH the server (`kevy`) and the client CLI (`kevy-cli`) —
# the latter ships in the runtime image so Docker / k8s healthchecks
# (and `docker exec mycontainer kevy-cli ping`) work without an
# external image.
RUN cargo build --release --bin kevy --bin kevy-cli --locked

FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.title="kevy" \
      org.opencontainers.image.description="Pure-Rust, zero-dependency, Redis-compatible KV server." \
      org.opencontainers.image.source="https://github.com/goliajp/kevy" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.vendor="GOLIA K.K."

COPY --from=build /src/target/release/kevy /usr/local/bin/kevy
COPY --from=build /src/target/release/kevy-cli /usr/local/bin/kevy-cli

# Default config; every value is overridable at `docker run` time.
ENV KEVY_BIND=0.0.0.0 \
    KEVY_PORT=6379 \
    KEVY_DIR=/data \
    KEVY_AOF=1

VOLUME ["/data"]
EXPOSE 6379

# Container healthcheck: kevy-cli ping returns +PONG when the server is
# accepting RESP. Honoured by `docker compose` (HEALTHCHECK directive),
# kubernetes (exec liveness probe — see docs/), and `docker inspect`.
HEALTHCHECK --interval=5s --timeout=2s --start-period=2s --retries=3 \
    CMD kevy-cli -p ${KEVY_PORT:-6379} ping || exit 1

ENTRYPOINT ["kevy"]
