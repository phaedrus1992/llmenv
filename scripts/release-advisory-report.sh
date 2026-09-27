#!/usr/bin/env bash
# Print a markdown advisory report for one release branch.
# Usage: release-advisory-report.sh <branch> <deny-outcome> <npm-audit-json>
#   <branch>          the release branch scanned (e.g. release/3.x)
#   <deny-outcome>    the cargo-deny-action step outcome: success|failure|cancelled|skipped
#   <npm-audit-json>  path to `npm audit --json` output, or "-" if the branch has no
#                     website/package-lock.json
# Exit 0: no findings. Exit 1: findings printed on stdout. Exit 2: bad input.
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "Usage: $(basename "$0") <branch> <deny-outcome> <npm-audit-json>" >&2
    exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$script_dir/release_advisory_report.py" "$@"
