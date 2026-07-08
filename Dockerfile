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
# Two independently-buildable crates share this stage: baesrv (server/) builds
# for the native gnu target, baectl builds as a static musl binary. They are
# COPYed into distinct subdirectories (server/ NOT flattened into /build) so a
# second crate can live alongside the first — hence the per-crate
# --manifest-path below rather than one build at the stage root.
COPY server/ ./server/
COPY baectl/ ./baectl/
RUN cargo build --release --manifest-path server/Cargo.toml
# baectl ships as a fully static musl binary so it drops into the slim runtime
# base with no libc/OpenSSL dependency (reqwest is pinned to rustls, no
# native-tls). musl-tools provides the musl-gcc linker the target needs.
RUN rustup target add x86_64-unknown-linux-musl \
    && apt-get update && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && cargo build --release --target x86_64-unknown-linux-musl --manifest-path baectl/Cargo.toml

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --home-dir /var/lib/bae bae \
    && mkdir -p /var/lib/bae \
    && chown bae:bae /var/lib/bae

COPY --from=build /build/server/target/release/baesrv /usr/local/bin/baesrv
COPY --from=build /build/baectl/target/x86_64-unknown-linux-musl/release/baectl /usr/local/bin/baectl

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
