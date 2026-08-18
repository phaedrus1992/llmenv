#!/bin/sh
# Test double for a real engine binary (claude/crush/opencode). Dumps its
# environment to $FAKE_ENGINE_ENV_DUMP and its argv (one arg per line) to
# $FAKE_ENGINE_ARGV_DUMP (each if set), sleeps for $FAKE_ENGINE_SLEEP_SECS (if
# set, for signal-propagation tests), then exits with $FAKE_ENGINE_EXIT_CODE
# (default 0).
#
# POSIX sh with an absolute shebang, not `#!/usr/bin/env bash`, so it runs with
# nothing but this script's own directory on PATH — `env` would have to look
# `bash` up on PATH to start at all. That lets a test narrow PATH to exclude
# `which` and still launch the engine (#1382). Every command below is a shell
# builtin except `env` and `sleep`, which only run when the test opts into them.
set -eu

if [ -n "${FAKE_ENGINE_ENV_DUMP:-}" ]; then
    env >"$FAKE_ENGINE_ENV_DUMP"
fi

if [ -n "${FAKE_ENGINE_ARGV_DUMP:-}" ]; then
    : >"$FAKE_ENGINE_ARGV_DUMP"
    for arg in "$@"; do
        printf '%s\n' "$arg" >>"$FAKE_ENGINE_ARGV_DUMP"
    done
fi

if [ -n "${FAKE_ENGINE_SLEEP_SECS:-}" ]; then
    sleep "$FAKE_ENGINE_SLEEP_SECS"
fi

exit "${FAKE_ENGINE_EXIT_CODE:-0}"
