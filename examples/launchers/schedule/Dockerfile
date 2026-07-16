# examples/launchers/schedule/Dockerfile — minimal bae-launcher-schedule extension
#
#   docker build -t my-schedule-launcher examples/launchers/schedule/
#   docker run --rm my-schedule-launcher
#
# `bae-launcher-schedule` is a BASE image: `baesched` is already its
# ENTRYPOINT. Your own Dockerfile only ever COPYs in your harness binary/script
# and your bae-schedules.toml — never redeclare ENTRYPOINT or CMD, or
# `baesched` never runs and the container does nothing.
#
# This example's "harness" is a one-file shell script (example-harness.sh)
# that just echoes its args/env and exits — swap it for your own compiled
# binary or script, built by any language/toolchain, using none of bae's SDKs
# if you don't want to. See docs/guides/harness-launchers.md for a full
# walkthrough and docs/reference/launchers.md for the bae-schedules.toml
# schema.

FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-schedule

# --chmod=755 makes the script executable without a separate RUN chmod layer.
COPY --chmod=755 example-harness.sh /usr/local/bin/example-harness

# BAE_SCHEDULES_CONFIG already defaults to /etc/bae/bae-schedules.toml in the
# base image, so dropping the file at that exact path is all you need — no env
# var to set in your own Dockerfile.
COPY bae-schedules.toml /etc/bae/bae-schedules.toml

# No ENTRYPOINT/CMD here — inherited from the base image (`baesched`).
