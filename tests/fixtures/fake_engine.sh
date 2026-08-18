#!/usr/bin/env bash
# Test double for a real engine binary (claude/crush/opencode). Dumps its
# environment to $FAKE_ENGINE_ENV_DUMP and its argv (one arg per line) to
# $FAKE_ENGINE_ARGV_DUMP (each if set), sleeps for $FAKE_ENGINE_SLEEP_SECS (if
# set, for signal-propagation tests), then exits with $FAKE_ENGINE_EXIT_CODE
# (default 0).
set -euo pipefail

if [[ -n "${FAKE_ENGINE_ENV_DUMP:-}" ]]; then
    env >"$FAKE_ENGINE_ENV_DUMP"
fi

if [[ -n "${FAKE_ENGINE_ARGV_DUMP:-}" ]]; then
    : >"$FAKE_ENGINE_ARGV_DUMP"
    for arg in "$@"; do
        printf '%s\n' "$arg" >>"$FAKE_ENGINE_ARGV_DUMP"
    done
fi

if [[ -n "${FAKE_ENGINE_SLEEP_SECS:-}" ]]; then
    sleep "$FAKE_ENGINE_SLEEP_SECS"
fi

exit "${FAKE_ENGINE_EXIT_CODE:-0}"
