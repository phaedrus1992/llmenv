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

mkdir -p "$SCRATCH/bin" "$SCRATCH/nobin" "$SCRATCH/work" || exit 1
cat >"$SCRATCH/bin/timeout" <<'STUB'
#!/usr/bin/env bash
sleep "${FAKE_SLEEP:-0}"
exit "${FAKE_STATUS:-0}"
STUB
chmod +x "$SCRATCH/bin/timeout"
ln -s "$(command -v date)" "$SCRATCH/nobin/date"
export PATH="$SCRATCH/bin:$PATH"
cd "$SCRATCH/work" || exit 1
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
check "a shard out of budget exits 10, so the check fails" "10" "$?"
check "a shard out of budget annotates an error" "1" \
  "$(grep -c '^::error title=mutants shard incomplete::' <<<"$out")"
check "the annotation names the shard as one-based" "1" \
  "$(grep -c 'shard 2/3' <<<"$out")"
check "the step summary says INCOMPLETE" "1" "$(grep -c 'INCOMPLETE' "$summary")"
check "the marker file is written" "1" "$(grep -c 'INCOMPLETE shard 2/3' mutants.out/INCOMPLETE)"

MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=2 FAKE_SLEEP=2 FAKE_STATUS=137 \
  bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a KILL after the budget ran out fails the shard, and is never green" "137" "$?"

MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=100 FAKE_SLEEP=0 FAKE_STATUS=137 \
  bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a 137 before the budget ran out is a failure, as an out-of-memory kill is" "137" "$?"

echo "src/lib.rs:1: replace foo with ()" >mutants.out/missed.txt
FAKE_STATUS=124 bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a survivor before the stop still fails the shard" "2" "$?"
check "a survivor before the stop also writes the incomplete marker" "1" \
  "$(grep -c '^INCOMPLETE shard 2/3' mutants.out/INCOMPLETE)"
rm -f mutants.out/missed.txt

old_start=$(($(date +%s) - 2350))
out="$(JOB_START="$old_start" FAKE_STATUS=0 bash "$SHARD" 0 3 "$SCRATCH/pr.diff" 2>/dev/null)"
check "a shard with no time left after the setup does not start, and fails" "10" "$?"
check "the late shard says why" "1" "$(grep -c 'did not start' <<<"$out")"

PATH="$SCRATCH/nobin" /bin/bash "$SHARD" 0 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a runner without timeout is refused, with the setup code" "11" "$?"

bash "$SHARD" 3 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a shard index at the shard count is refused, with the setup code" "11" "$?"

bash "$SHARD" 0 3 "$SCRATCH/missing.diff" >/dev/null 2>&1
check "a missing diff file is refused, with the setup code" "11" "$?"

bash "$SHARD" 0 x "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a non-number shard count is refused, with the setup code" "11" "$?"

bash "$SHARD" 0 3 >/dev/null 2>&1
check "a missing argument is refused, with the setup code" "11" "$?"

bash "$SHARD" --help >/dev/null 2>&1
check "--help exits 0" "0" "$?"

echo "passed: $PASS, failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
