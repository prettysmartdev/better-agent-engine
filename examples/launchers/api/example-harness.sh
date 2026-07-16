#!/bin/sh
# example-harness.sh — the trivial "agent harness" this example packages.
#
# A real harness would call an LLM/agent SDK here (see docs/guides/
# building-a-client.md). This one only proves that baeapi launched it with the
# arguments and environment bae-api.toml's templating produced, so it just
# echoes them and exits.

echo "[example-harness] args: $*"
echo "[example-harness] AGENT_PROMPT=${AGENT_PROMPT:-<unset>}"
