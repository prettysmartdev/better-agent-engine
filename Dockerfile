# Better Agent Server Engine (BASE) — production server image.
#
# Multi-stage build: compile the Rust server, then ship only the binary on a
# slim runtime base. SQLite data lives in /var/lib/base — mount a volume
# there to persist state across container restarts.
#
#   docker build -t better-agent-server-engine .
#   docker run -p 8080:8080 -v base-data:/var/lib/base better-agent-server-engine

FROM rust:1-bookworm AS build
WORKDIR /build
COPY server/ ./
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --home-dir /var/lib/base base \
    && mkdir -p /var/lib/base \
    && chown base:base /var/lib/base

COPY --from=build /build/target/release/base-server /usr/local/bin/base-server

USER base
ENV BASE_ADDR=0.0.0.0:8080 \
    BASE_DB_PATH=/var/lib/base/base.db
EXPOSE 8080
VOLUME ["/var/lib/base"]
ENTRYPOINT ["base-server"]
