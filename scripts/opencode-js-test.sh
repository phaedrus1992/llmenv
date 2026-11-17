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
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
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
# The advisory for picomatch has no fixed version (#3792). It is the only advisory that may pass.
# Any other advisory in the tree fails the run.
known_advisory="https://github.com/advisories/GHSA-fc5q-39xc-vjpf"
others=$({ npm audit --prefix "$dir" --json || true; } |
    jq -r --arg known "$known_advisory" \
        '[.vulnerabilities[].via[] | select(type == "object") | .url | select(. != $known)] | unique | .[]')
if [ -n "$others" ]; then
    echo "opencode-js-test.sh: npm audit found advisories other than the known one:" >&2
    echo "$others" >&2
    echo "Fix: update the toolchain in $dir, or review the advisory and add it here." >&2
    exit 1
fi
"$dir/node_modules/.bin/vitest" run ${coverage[@]+"${coverage[@]}"} --config "$dir/vitest.config.js"
