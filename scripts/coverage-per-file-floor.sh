#!/usr/bin/env bash
set -euo pipefail

# Enforces a uniform per-file line-coverage floor over cargo-llvm-cov's JSON
# export, since cargo-llvm-cov has no per-file equivalent to --fail-under-lines
# (unlike vitest's coverage.thresholds.perFile). Mirrors that: every file must
# individually clear the same threshold, distinct from coverage.yml's
# aggregate --fail-under-lines gate, which one well-covered file can satisfy
# on behalf of a poorly-covered one.

usage() {
  echo "Usage: $0 <coverage.json> <floor-percent>" >&2
  exit 1
}

[[ $# -eq 2 ]] || usage
JSON_PATH="$1"
FLOOR="$2"

REPO_ROOT="$(git rev-parse --show-toplevel)"
EXCEPTIONS_FILE="$REPO_ROOT/.github/coverage-per-file-exceptions.txt"

is_excepted() {
  local rel="$1"
  [[ -f "$EXCEPTIONS_FILE" ]] || return 1
  grep -vE '^[[:space:]]*(#|$)' "$EXCEPTIONS_FILE" | grep -qxF "$rel"
}

FAILED=0
while IFS=$'\t' read -r pct filename; do
  rel="${filename#"$REPO_ROOT"/}"
  if is_excepted "$rel"; then
    continue
  fi
  if awk -v p="$pct" -v f="$FLOOR" 'BEGIN { exit !(p < f) }'; then
    printf '::error::%s is %.2f%% line coverage, below the %s%% per-file floor\n' "$rel" "$pct" "$FLOOR"
    FAILED=1
  fi
done < <(jq -r '
  .data[].files[]
  | select(.summary.lines.count > 0)
  | "\(.summary.lines.percent)\t\(.filename)"
' "$JSON_PATH")

if [[ "$FAILED" -eq 1 ]]; then
  echo "::error::one or more files are below the per-file coverage floor (${FLOOR}%)" >&2
  echo "New gaps: add tests. Pre-existing gaps: add the file to ${EXCEPTIONS_FILE#"$REPO_ROOT"/} with a follow-up issue link." >&2
  exit 1
fi

echo "All measured files meet the ${FLOOR}% per-file coverage floor."
