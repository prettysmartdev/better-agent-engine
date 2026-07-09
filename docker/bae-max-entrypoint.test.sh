#!/bin/bash
# Regression test for docker/bae-max-entrypoint.sh's signal-forwarding and
# failure-propagation behavior. No container image build required: `baesrv`
# and `node` are faked via a PATH-prepended temp bin directory, so the
# entrypoint script itself runs completely unmodified.
#
# Each fake child writes its own PID to a file on start, traps SIGTERM to
# record that it was received (writing a marker file) and exits 0. A
# test-only SIGKILL simulates one child crashing unexpectedly.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENTRYPOINT="$SCRIPT_DIR/bae-max-entrypoint.sh"

FAILURES=0
fail() { echo "FAIL: $1" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS: $1"; }

# wait_for_file <path> <timeout_seconds> — poll until a file is non-empty.
wait_for_file() {
  local path="$1" timeout="$2" waited=0
  while [ ! -s "$path" ]; do
    sleep 0.1
    waited=$((waited + 1))
    if [ "$waited" -ge $((timeout * 10)) ]; then
      return 1
    fi
  done
  return 0
}

# wait_with_timeout <pid> <timeout_seconds> — `wait` for a job with a
# watchdog, so a bug in the entrypoint (hang instead of exit) fails the test
# instead of hanging the suite forever. Sets $WAIT_EXIT_CODE rather than
# returning via command substitution, since `wait` only works on direct
# children of the *current* shell — a subshell (which command substitution
# creates) cannot reap a job started in the caller.
WAIT_EXIT_CODE=
wait_with_timeout() {
  local pid="$1" timeout="$2"
  ( sleep "$timeout"; kill -KILL "$pid" 2>/dev/null ) &
  local watchdog=$!
  wait "$pid"
  WAIT_EXIT_CODE=$?
  kill "$watchdog" 2>/dev/null
  wait "$watchdog" 2>/dev/null
}

make_fixture() {
  local dir
  dir="$(mktemp -d)"
  mkdir -p "$dir/bin"

  cat > "$dir/bin/baesrv" <<EOF
#!/bin/bash
echo "\$\$" > "$dir/baesrv.pid"
trap 'echo TERM > "$dir/baesrv.term"; exit 0' TERM
while true; do sleep 0.1; done
EOF

  cat > "$dir/bin/node" <<EOF
#!/bin/bash
echo "\$\$" > "$dir/max.pid"
trap 'echo TERM > "$dir/max.term"; exit 0' TERM
while true; do sleep 0.1; done
EOF

  chmod +x "$dir/bin/baesrv" "$dir/bin/node"
  echo "$dir"
}

# reap_fixture <dir> — best-effort cleanup of any surviving fake children.
reap_fixture() {
  local dir="$1"
  for f in "$dir/baesrv.pid" "$dir/max.pid"; do
    [ -s "$f" ] && kill -KILL "$(cat "$f")" 2>/dev/null
  done
  rm -rf "$dir"
}

# --- Test 1: killing one child forwards TERM to the other, exits non-zero ---
test_kill_one_child_forwards_term() {
  local dir
  dir="$(make_fixture)"

  PATH="$dir/bin:$PATH" "$ENTRYPOINT" &
  local entrypoint_pid=$!

  if ! wait_for_file "$dir/baesrv.pid" 5 || ! wait_for_file "$dir/max.pid" 5; then
    fail "kill-one-child: children never started"
    kill -KILL "$entrypoint_pid" 2>/dev/null
    reap_fixture "$dir"
    return
  fi

  local baesrv_pid
  baesrv_pid="$(cat "$dir/baesrv.pid")"

  # Simulate baesrv crashing unexpectedly.
  kill -KILL "$baesrv_pid"

  wait_with_timeout "$entrypoint_pid" 10
  local exit_code="$WAIT_EXIT_CODE"

  if [ "$exit_code" -eq 0 ]; then
    fail "kill-one-child: entrypoint exited 0, expected non-zero"
  else
    pass "kill-one-child: entrypoint exited non-zero ($exit_code)"
  fi

  if wait_for_file "$dir/max.term" 5; then
    pass "kill-one-child: surviving child (max) received forwarded TERM"
  else
    fail "kill-one-child: surviving child (max) never received TERM"
  fi

  reap_fixture "$dir"
}

# --- Test 2: SIGTERM to the script forwards to both children, exits cleanly ---
test_sigterm_script_forwards_to_both() {
  local dir
  dir="$(make_fixture)"

  PATH="$dir/bin:$PATH" "$ENTRYPOINT" &
  local entrypoint_pid=$!

  if ! wait_for_file "$dir/baesrv.pid" 5 || ! wait_for_file "$dir/max.pid" 5; then
    fail "sigterm-script: children never started"
    kill -KILL "$entrypoint_pid" 2>/dev/null
    reap_fixture "$dir"
    return
  fi

  kill -TERM "$entrypoint_pid"

  wait_with_timeout "$entrypoint_pid" 10
  local exit_code="$WAIT_EXIT_CODE"

  # Per POSIX, `wait` interrupted by a trapped signal returns immediately
  # with an exit status > 128 indicating the caught signal (128 + 15 = 143
  # for SIGTERM), rather than the exit status either child returned — bash
  # runs the trap (which forwards TERM to both children) but does not
  # resume waiting on the original PIDs afterward. This is the standard,
  # deterministic "clean shutdown via signal" outcome for this wait-in-a-
  # trap pattern, not a hang or a crash, so 143 (not 0) is the correct
  # expectation here.
  if [ "$exit_code" -eq 143 ]; then
    pass "sigterm-script: entrypoint exited cleanly on SIGTERM (143)"
  else
    fail "sigterm-script: entrypoint exit code was $exit_code, expected 143"
  fi

  if wait_for_file "$dir/baesrv.term" 5; then
    pass "sigterm-script: baesrv received TERM"
  else
    fail "sigterm-script: baesrv never received TERM"
  fi

  if wait_for_file "$dir/max.term" 5; then
    pass "sigterm-script: max received TERM"
  else
    fail "sigterm-script: max never received TERM"
  fi

  reap_fixture "$dir"
}

test_kill_one_child_forwards_term
test_sigterm_script_forwards_to_both

if [ "$FAILURES" -gt 0 ]; then
  echo "$FAILURES failure(s)" >&2
  exit 1
fi

echo "all entrypoint regression tests passed"
