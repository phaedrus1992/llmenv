#!/usr/bin/env bash
# Run the vitest tests of src/adapter/opencode/plugin.js from the repo root.
#
# Usage: scripts/opencode-js-test.sh [--coverage] [--help]
#
# Installs the pinned toolchain with `npm ci` (no install scripts), audits it, and runs vitest.
# With --coverage it writes coverage/lcov.info at the repo root, where crap-score reads it.
# It must run from the repo root: vitest writes the paths of the LCOV file relative to the
# working directory, and crap-score resolves them from the repo root (#3092).
set -euo pipefail

dir=src/adapter/opencode/js-test

usage() {
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
}

coverage=()
for arg in "$@"; do
    case "$arg" in
    --help | -h)
        usage
        exit 0
        ;;
    --coverage) coverage=(--coverage) ;;
    *)
        echo "opencode-js-test.sh: unknown argument '$arg'. Fix: run with --help." >&2
        exit 2
        ;;
    esac
done

cd "$(git rev-parse --show-toplevel)"

npm ci --prefix "$dir"
# The advisory for picomatch has no fixed version (#3792), so only a critical advisory fails.
npm audit --prefix "$dir" --audit-level=critical
"$dir/node_modules/.bin/vitest" run ${coverage[@]+"${coverage[@]}"} --config "$dir/vitest.config.js"
