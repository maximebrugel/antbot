# syntax=docker/dockerfile:1.7
#
# antbot: multi-stage build.
#   builder: Rust toolchain, BuildKit cache mounts for the cargo registry,
#            git checkouts and the target dir (fast incremental rebuilds).
#   runtime: debian slim + CA certificates, non-root user, data in /data.
#
#   docker build --build-arg GIT_SHA=$(git rev-parse --short HEAD) \
#     -t antbot:latest -t antbot:$(git rev-parse --short HEAD) .
#
# The runtime works with a read-only root filesystem: the bot only writes to
# its data dir (/data), and the healthcheck is the binary itself.

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
USER ${APP_UID}:${APP_GID}

# `antbot healthcheck` (exec form: no shell, curl or wget needed; writes
# nothing, so it works with a read-only root filesystem). With
# ANTBOT_HEALTH_LISTEN / [monitor].health_listen it needs a 200 from the
# loopback /healthz (fresh market data, reconciliation ok, no kill, ...);
# without it, it checks the run-lock heartbeat in $ANTBOT_DATA_DIR.
HEALTHCHECK --interval=30s --timeout=5s --start-period=120s --retries=3 \
    CMD ["antbot", "healthcheck", "--timeout-secs", "4"]

# Labels last: a new revision does not invalidate the layers above.
ARG DEBIAN_RELEASE
ARG GIT_SHA=unknown
ARG SOURCE_URL=https://github.com/maximebrugel/antbot
LABEL org.opencontainers.image.title="antbot" \
      org.opencontainers.image.description="Hyperliquid premium mean-reversion bot" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.revision="${GIT_SHA}" \
      org.opencontainers.image.base.name="debian:${DEBIAN_RELEASE}-slim"

# antbot handles SIGINT / SIGTERM itself (cancels open orders, saves state,
# flushes alerts). Default command: run in the config mode (paper by default).
ENTRYPOINT ["antbot"]
CMD ["run"]
