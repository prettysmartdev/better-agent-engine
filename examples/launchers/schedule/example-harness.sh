#!/bin/sh
# example-harness.sh — the trivial "agent harness" this example packages.
#
# A real harness would call an LLM/agent SDK here (see docs/guides/
# 01-building-a-client.md). This one only proves that baesched launched it with
# the arguments and environment bae-schedules.toml configured, so it just
# echoes them and exits.

echo "[example-harness] args: $*"
echo "[example-harness] GREETING=${GREETING:-<unset>}"
