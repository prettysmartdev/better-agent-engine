# examples/launchers/api/Dockerfile — minimal bae-launcher-api extension
#
#   docker build -t my-api-launcher examples/launchers/api/
#   docker run --rm -p 9090:9090 my-api-launcher
#
# Then, in another shell:
#   curl --no-buffer -X POST http://localhost:9090/agents/echo/trigger \
#     -H 'Content-Type: application/json' \
#     -d '{"prompt": "hello from curl"}'
#
# `bae-launcher-api` is a BASE image: `baeapi` is already its ENTRYPOINT. Your
# own Dockerfile only ever COPYs in your harness binary/script and your
# bae-api.toml — never redeclare ENTRYPOINT or CMD, or `baeapi` never runs and
# the container serves nothing.
#
# BAE_LAUNCHER_API_TOKEN is left UNSET here on purpose (this is a local, throw-
# away example) — every /agents/* route is wide open with no auth. Never run
# an image built this way on a network-reachable host without either setting
# that env var or putting it behind a reverse proxy on an internal network; see
# docs/guides/harness-launchers.md and aspec/architecture/security.md.

FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-api

COPY --chmod=755 example-harness.sh /usr/local/bin/example-harness

# BAE_LAUNCHER_API_CONFIG already defaults to /etc/bae/bae-api.toml in this
# base image (the webapp image's default is /etc/bae/bae-app.toml instead —
# same binary, different baked-in default), so dropping the file at that exact
# path is all you need — no env var to set in your own Dockerfile.
COPY bae-api.toml /etc/bae/bae-api.toml

# No ENTRYPOINT/CMD here — inherited from the base image (`baeapi`).
