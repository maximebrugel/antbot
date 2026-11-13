# syntax=docker/dockerfile:1.7
#
# antbot: multi-stage build.
#   builder: Rust toolchain, BuildKit cache mounts for the cargo registry,
#            git checkouts and the target dir (fast incremental rebuilds).
#   runtime: debian slim + CA certificates, non-root user, data in /data.

ARG RUST_VERSION=1.100
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS builder
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src

# reqwest / tungstenite use rustls: no OpenSSL or pkg-config needed.
# The target dir lives in a cache mount, so the binary is copied out of it.
RUN --mount=type=cache,id=antbot-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=antbot-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=antbot-target,target=/src/target \
    cargo build --release --locked --bin antbot \
    && install -D -m 0755 target/release/antbot /out/antbot

FROM debian:${DEBIAN_RELEASE}-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG APP_UID=10001
ARG APP_GID=10001
RUN groupadd --system --gid ${APP_GID} antbot \
    && useradd --system --uid ${APP_UID} --gid ${APP_GID} --home-dir /app \
       --shell /usr/sbin/nologin antbot \
    && mkdir -p /app/config /data \
    && chown antbot:antbot /data

COPY --from=builder /out/antbot /usr/local/bin/antbot
COPY config/default.toml /app/config/default.toml

WORKDIR /app
# Config path and data dir (state, CSV journals, logs/). /data is the volume.
ENV ANTBOT_CONFIG=/app/config/default.toml \
    ANTBOT_DATA_DIR=/data \
    RUST_LOG=info
VOLUME ["/data"]
USER antbot:antbot

# Healthy while the run lock heartbeat (refreshed every 20 s) is fresh. Shell
# form so the path follows ANTBOT_DATA_DIR (e.g. /data/testnet set in .env).
HEALTHCHECK --interval=60s --timeout=5s --start-period=60s --retries=3 \
    CMD test -n "$(find "${ANTBOT_DATA_DIR:-/data}/antbot.lock" -mmin -2 2>/dev/null)" || exit 1

# antbot handles SIGINT / SIGTERM itself (cancels open orders, saves state,
# flushes alerts). Default command: paper mode (the config default).
ENTRYPOINT ["antbot"]
CMD ["run"]
