# Better Agent Engine (BAE) — production server image.
#
# Multi-stage build: compile the Rust server, then ship only the binary on a
# slim runtime base. SQLite data lives in /var/lib/bae — mount a volume
# there to persist state across container restarts.
#
#   docker build -t better-agent-engine .
#   docker run -p 8080:8080 -v bae-data:/var/lib/bae better-agent-engine

FROM rust:1-bookworm AS build
WORKDIR /build
COPY server/ ./
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --home-dir /var/lib/bae bae \
    && mkdir -p /var/lib/bae \
    && chown bae:bae /var/lib/bae

COPY --from=build /build/target/release/baesrv /usr/local/bin/baesrv

USER bae
ENV BAE_ADDR=0.0.0.0:8080 \
    BAE_DB_PATH=/var/lib/bae/bae.db \
    BAE_LOG=info
# Only the client port is exposed. The admin port (BAE_ADMIN_ADDR, default
# 127.0.0.1:8081) binds to loopback inside the container and is intentionally
# NOT exposed — reach it via `docker exec` / an SSH tunnel, never the network.
# TLS terminates upstream; the container speaks plain HTTP.
EXPOSE 8080
VOLUME ["/var/lib/bae"]
ENTRYPOINT ["baesrv"]
