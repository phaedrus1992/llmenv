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

# The run lasts 3 s, and the KILL is due at 2 + 1 s. The whole-second clock reads 3 or 4.
MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=2 MUTANTS_KILL_AFTER=1 FAKE_SLEEP=3 FAKE_STATUS=137 \
  bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a KILL after the budget and the grace time is a budget stop, as GNU timeout -k ends it" \
  "10" "$?"

# The run lasts 2 s, and the KILL is due at 2 + 5 s. A 137 inside the grace time is a failure.
MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=2 MUTANTS_KILL_AFTER=5 FAKE_SLEEP=2 FAKE_STATUS=137 \
  bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a 137 inside the grace time is a failure, as an out-of-memory kill is" "137" "$?"

mkdir -p "$SCRATCH/datebin"
cat >"$SCRATCH/datebin/date" <<'STUB'
#!/usr/bin/env bash
n=$(($(cat "$DATE_COUNT" 2>/dev/null || echo 0) + 1))
echo "$n" >"$DATE_COUNT"
if ((n > ${DATE_OK:-99})); then exit 1; fi
exec /bin/date "$@"
STUB
chmod +x "$SCRATCH/datebin/date"
job_start="$(date +%s)"
err="$(PATH="$SCRATCH/datebin:$PATH" DATE_COUNT="$SCRATCH/date.count" DATE_OK=1 \
  JOB_START="$job_start" MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=2 MUTANTS_KILL_AFTER=1 \
  FAKE_SLEEP=3 FAKE_STATUS=137 bash "$SHARD" 1 3 "$SCRATCH/pr.diff" 2>&1 >/dev/null)"
check "a failed clock after the run keeps a 137 red" "137" "$?"
check "a failed clock after the run is named" "1" "$(grep -c 'cannot read the clock after' <<<"$err")"

MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=100 FAKE_SLEEP=0 FAKE_STATUS=137 \
  bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a 137 before the budget ran out is a failure, as an out-of-memory kill is" "137" "$?"

echo "src/lib.rs:1: replace foo with ()" >mutants.out/missed.txt
FAKE_STATUS=124 bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a survivor before the stop still fails the shard" "2" "$?"
check "a survivor before the stop also writes the incomplete marker" "1" \
  "$(grep -c '^INCOMPLETE shard 2/3' mutants.out/INCOMPLETE)"

MUTANTS_MIN_BUDGET=1 MUTANTS_BUDGET=2 MUTANTS_KILL_AFTER=1 FAKE_SLEEP=3 FAKE_STATUS=137 \
  bash "$SHARD" 1 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a survivor before a KILL stop still fails the shard" "2" "$?"
rm -f mutants.out/missed.txt

clock="$(date +%s)"
old_start=$((clock - 2350))
out="$(JOB_START="$old_start" FAKE_STATUS=0 bash "$SHARD" 0 3 "$SCRATCH/pr.diff" 2>/dev/null)"
check "a shard with no time left after the setup does not start, and fails" "10" "$?"
check "the late shard says why" "1" "$(grep -c 'did not start' <<<"$out")"

future_start=$((clock + 3600))
JOB_START="$future_start" FAKE_STATUS=0 bash "$SHARD" 0 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a job start in the future is refused, with the setup code" "11" "$?"

JOB_START="$clock" MUTANTS_BUDGET=0 MUTANTS_MIN_BUDGET=0 FAKE_STATUS=0 \
  bash "$SHARD" 0 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a budget of zero does not start the shard, because timeout 0 has no limit" "10" "$?"

PATH="$SCRATCH/nobin" /bin/bash "$SHARD" 0 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a runner without timeout is refused, with the setup code" "11" "$?"

bash "$SHARD" 3 3 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a shard index at the shard count is refused, with the setup code" "11" "$?"

bash "$SHARD" 0 3 "$SCRATCH/missing.diff" >/dev/null 2>&1
check "a missing diff file is refused, with the setup code" "11" "$?"

bash "$SHARD" 0 x "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a non-number shard count is refused, with the setup code" "11" "$?"

bash "$SHARD" 08 9 "$SCRATCH/pr.diff" >/dev/null 2>&1
check "a shard index with a leading zero is refused, not read as octal" "11" "$?"

bash "$SHARD" 0 3 >/dev/null 2>&1
check "a missing argument is refused, with the setup code" "11" "$?"

bash "$SHARD" --help >/dev/null 2>&1
check "--help exits 0" "0" "$?"

check "--help lists every environment variable" "4" \
  "$(bash "$SHARD" --help 2>&1 | grep -c -E '^  (JOB_START|MUTANTS_BUDGET|MUTANTS_MIN_BUDGET|MUTANTS_KILL_AFTER) ')"

echo "passed: $PASS, failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
