# examples/launchers/webapp/Dockerfile — minimal bae-launcher-webapp extension
#
#   docker build -t my-webapp-launcher examples/launchers/webapp/
#   docker run --rm -p 9090:9090 my-webapp-launcher
#
# Then open http://localhost:9090/ in a browser: a one-card grid ("Echo
# Agent") links to a chat view where a free-form prompt or the "Say hello" /
# "Tell a joke" buttons trigger POST /agents/echo/trigger and stream the
# harness's output into the chat transcript live.
#
# `bae-launcher-webapp` is a BASE image bundling the SAME `baeapi` binary as
# `bae-launcher-api` (no second process) plus the built static webapp; `baeapi`
# is already its ENTRYPOINT. Your own Dockerfile only ever COPYs in your
# harness binary/script and your bae-app.toml — never redeclare ENTRYPOINT or
# CMD, or `baeapi` never runs and the container serves nothing (not even the
# static assets).
#
# BAE_LAUNCHER_API_TOKEN is left UNSET here on purpose (this is a local, throw-
# away example) — every /agents/* route, including the ones the webapp itself
# calls, is wide open with no auth. Never run an image built this way on a
# network-reachable host without either setting that env var or putting it
# behind a reverse proxy on an internal network; see
# docs/guides/harness-launchers.md and aspec/architecture/security.md.

FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp

COPY --chmod=755 example-harness.sh /usr/local/bin/example-harness

# BAE_LAUNCHER_API_CONFIG already defaults to /etc/bae/bae-app.toml in this
# base image (the plain API image's default is /etc/bae/bae-api.toml instead —
# same binary, different baked-in default), so dropping the file at that exact
# path is all you need — no env var to set in your own Dockerfile.
COPY bae-app.toml /etc/bae/bae-app.toml

# No ENTRYPOINT/CMD here — inherited from the base image (`baeapi`, serving
# both the JSON routes and the pre-built static SPA via BAE_LAUNCHER_WEBAPP_
# STATIC_DIR, which the base image's own Dockerfile already sets).
