#!/usr/bin/env bash
# Tests for scripts/mutants-shard.sh (#3569). A stub `timeout` stands in for the budget, so the
# test does not wait: the stub exits with $FAKE_STATUS, as `timeout cargo mutants` would.
# Run: bash .github/workflows/__tests__/mutants-shard.sh
set -uo pipefail

WORKFLOWS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHARD="$(cd "$WORKFLOWS_DIR/../.." && pwd)/scripts/mutants-shard.sh"

SCRATCH="$(mktemp -d)"
if [[ -z "$SCRATCH" || ! -d "$SCRATCH" ]]; then
  echo "harness: mktemp -d failed" >&2
  exit 1
fi
trap 'trash "$SCRATCH" 2>/dev/null || true' EXIT

mkdir -p "$SCRATCH/bin" || exit 1
cat >"$SCRATCH/bin/timeout" <<'STUB'
#!/usr/bin/env bash
exit "${FAKE_STATUS:-0}"
STUB
chmod +x "$SCRATCH/bin/timeout"
export PATH="$SCRATCH/bin:$PATH"
echo "diff" >"$SCRATCH/pr.diff"

PASS=0
FAIL=0
check() {
  local name="$1" want="$2" got="$3"
  if [[ "$want" == "$got" ]]; then
    PASS=$((PASS + 1))
    echo "PASS: $name"
  else
    FAIL=$((FAIL + 1))
    echo "FAIL: $name"
    echo "  want: $want"
    echo "  got:  $got"
  fi
}

summary="$SCRATCH/summary.md"

FAKE_STATUS=0 bash "$SHARD" 0 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a finished shard exits 0" "0" "$?"

FAKE_STATUS=2 bash "$SHARD" 0 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a surviving mutant still fails the shard" "2" "$?"

out="$(FAKE_STATUS=124 GITHUB_STEP_SUMMARY="$summary" bash "$SHARD" 1 3 "$SCRATCH/pr.diff" 2>/dev/null)"
check "a shard out of budget exits 0" "0" "$?"
check "a shard out of budget annotates a warning" "1" \
  "$(grep -c '^::warning title=mutants shard incomplete::' <<<"$out")"
check "the annotation names the shard as one-based" "1" \
  "$(grep -c 'shard 2/3' <<<"$out")"
check "the step summary says INCOMPLETE" "1" "$(grep -c 'INCOMPLETE' "$summary")"

bash "$SHARD" 3 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a shard index at the shard count is refused" "2" "$?"

bash "$SHARD" 0 3 "$SCRATCH/missing.diff" >/dev/null 2>&1
check "a missing diff file is refused" "2" "$?"

bash "$SHARD" 0 x "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a non-number shard count is refused" "2" "$?"

bash "$SHARD" 0 3 >/dev/null 2>&1
check "a missing argument is refused" "2" "$?"

bash "$SHARD" --help >/dev/null 2>&1
check "--help exits 0" "0" "$?"

echo "passed: $PASS, failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
