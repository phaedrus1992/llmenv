#!/usr/bin/env bash
# Tests for scripts/coverage-per-file-floor.sh.
# Run: bash .github/workflows/__tests__/coverage-per-file-floor.sh
set -uo pipefail

PASS=0
FAIL=0

run_test() {
  local name="$1" fn="$2"
  if "$fn"; then
    echo "PASS: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name"
    FAIL=$((FAIL + 1))
  fi
}

SCRIPT="$(cd "$(dirname "$0")/../../.." && pwd)/scripts/coverage-per-file-floor.sh"

# The script resolves REPO_ROOT via `git rev-parse --show-toplevel`, so each
# test builds a scratch git repo and writes coverage.json paths inside it.

test_all_files_above_floor_passes() {
  local repo json
  repo=$(mktemp -d)
  (cd "$repo" && git init -q .)
  json=$(cat <<JSON
{"data":[{"files":[
  {"filename":"$repo/src/a.rs","summary":{"lines":{"count":100,"covered":90,"percent":90.00}}},
  {"filename":"$repo/src/b.rs","summary":{"lines":{"count":100,"covered":91,"percent":91.00}}}
]}]}
JSON
)
  printf '%s' "$json" > "$repo/coverage.json"

  local out rc
  out=$(cd "$repo" && bash "$SCRIPT" coverage.json 40 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 0 ]] && echo "$out" | grep -q "meet the 40% per-file coverage floor"
}

test_file_below_floor_fails() {
  local repo json
  repo=$(mktemp -d)
  (cd "$repo" && git init -q .)
  json=$(cat <<JSON
{"data":[{"files":[
  {"filename":"$repo/src/a.rs","summary":{"lines":{"count":100,"covered":90,"percent":90.00}}},
  {"filename":"$repo/src/low.rs","summary":{"lines":{"count":100,"covered":10,"percent":10.00}}}
]}]}
JSON
)
  printf '%s' "$json" > "$repo/coverage.json"

  local out rc
  out=$(cd "$repo" && bash "$SCRIPT" coverage.json 40 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 1 ]] && echo "$out" | grep -q "src/low.rs is 10.00% line coverage"
}

test_excepted_file_below_floor_passes() {
  local repo json
  repo=$(mktemp -d)
  (cd "$repo" && git init -q . && mkdir -p .github && printf '# comment\n\nsrc/low.rs\n' > .github/coverage-per-file-exceptions.txt)
  json=$(cat <<JSON
{"data":[{"files":[
  {"filename":"$repo/src/low.rs","summary":{"lines":{"count":100,"covered":10,"percent":10.00}}}
]}]}
JSON
)
  printf '%s' "$json" > "$repo/coverage.json"

  local out rc
  out=$(cd "$repo" && bash "$SCRIPT" coverage.json 40 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 0 ]] && ! echo "$out" | grep -q "low.rs"
}

test_zero_line_files_are_ignored() {
  local repo json
  repo=$(mktemp -d)
  (cd "$repo" && git init -q .)
  json=$(cat <<JSON
{"data":[{"files":[
  {"filename":"$repo/src/generated.rs","summary":{"lines":{"count":0,"covered":0,"percent":0.00}}},
  {"filename":"$repo/src/a.rs","summary":{"lines":{"count":100,"covered":90,"percent":90.00}}}
]}]}
JSON
)
  printf '%s' "$json" > "$repo/coverage.json"

  local out rc
  out=$(cd "$repo" && bash "$SCRIPT" coverage.json 40 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 0 ]] && ! echo "$out" | grep -q "generated.rs"
}

test_empty_report_hard_fails() {
  local repo json
  repo=$(mktemp -d)
  (cd "$repo" && git init -q .)
  json='{"data":[{"files":[]}]}'
  printf '%s' "$json" > "$repo/coverage.json"

  local out rc
  out=$(cd "$repo" && bash "$SCRIPT" coverage.json 40 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 1 ]] && echo "$out" | grep -q "yielded no measured files"
}

test_malformed_json_hard_fails() {
  local repo
  repo=$(mktemp -d)
  (cd "$repo" && git init -q .)
  printf 'not valid json {{{' > "$repo/coverage.json"

  local out rc
  out=$(cd "$repo" && bash "$SCRIPT" coverage.json 40 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 1 ]] && echo "$out" | grep -q "failed to parse"
}

run_test "all files above floor passes" test_all_files_above_floor_passes
run_test "file below floor fails with the offending path and percent" test_file_below_floor_fails
run_test "excepted file below floor still passes" test_excepted_file_below_floor_passes
run_test "zero-line (unmeasured) files are ignored" test_zero_line_files_are_ignored
run_test "empty report hard-fails instead of passing vacuously" test_empty_report_hard_fails
run_test "malformed JSON hard-fails instead of passing vacuously" test_malformed_json_hard_fails

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
