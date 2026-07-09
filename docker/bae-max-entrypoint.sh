#!/bin/bash
#
# Entrypoint for the bae-max image: starts baesrv and max/server as background
# children of this process (PID 1 in the container). Forwards SIGTERM/SIGINT
# to both, and — critically — the instant EITHER child exits, kills the
# other and exits non-zero, so a crashed baesrv never leaves a
# "healthy-looking" container with only max still running (or vice versa).
#
# Deliberately does NOT `set -e`: `wait -n`'s exit status is the exited
# child's own status, which is expected to be non-zero on a crash — `set -e`
# would abort the script at that exact line and skip the kill/wait/exit below
# it, which is the one thing this script exists to do.
#
# `wait -n` is bash-specific (not POSIX sh/dash), hence the #!/bin/bash
# shebang rather than #!/bin/sh — debian:bookworm-slim's default /bin/sh is
# dash, which does not support it.
set -uo pipefail

baesrv &
BAESRV_PID=$!

node /usr/local/lib/max/server/index.js &
MAX_PID=$!

trap 'kill -TERM "$BAESRV_PID" "$MAX_PID" 2>/dev/null' TERM INT

wait -n "$BAESRV_PID" "$MAX_PID"
EXIT_CODE=$?

kill -TERM "$BAESRV_PID" "$MAX_PID" 2>/dev/null
wait "$BAESRV_PID" "$MAX_PID" 2>/dev/null

exit "$EXIT_CODE"
