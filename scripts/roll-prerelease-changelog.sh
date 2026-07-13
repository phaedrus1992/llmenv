#!/usr/bin/env bash
set -euo pipefail

# Delegates to roll-changelog.sh — kept for backward compatibility.
exec "$(dirname "$0")/roll-changelog.sh" "$@"
