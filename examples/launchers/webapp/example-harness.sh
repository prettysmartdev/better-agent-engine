#!/bin/sh
# example-harness.sh — the trivial "agent harness" this example packages.
#
# A real harness would call an LLM/agent SDK here (see docs/guides/
# 01-building-a-client.md). This one only proves that baeapi launched it with the
# arguments and environment bae-app.toml's templating produced, so it just
# echoes them and exits. The webapp's chat view streams this output live.

echo "[example-harness] args: $*"
echo "[example-harness] AGENT_PROMPT=${AGENT_PROMPT:-<unset>}"
