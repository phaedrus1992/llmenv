#!/usr/bin/env bash
# Tests for scripts/mutants-plan.sh (#2413). Build a scratch repo with an
# origin branch, stub `cargo mutants --list`, and check the printed plan.
# Run: bash .github/workflows/__tests__/mutants-plan.sh
set -uo pipefail

WORKFLOWS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLAN="$(cd "$WORKFLOWS_DIR/../.." && pwd)/scripts/mutants-plan.sh"

SCRATCH="$(mktemp -d)"
if [[ -z "$SCRATCH" || ! -d "$SCRATCH" ]]; then
  echo "harness: mktemp -d failed" >&2
  exit 1
fi
trap 'trash "$SCRATCH" 2>/dev/null || true' EXIT

mkdir -p "$SCRATCH/bin" "$SCRATCH/work" || exit 1
# The stub prints $FAKE_MUTANTS lines, one per mutant.
cat >"$SCRATCH/bin/cargo" <<'STUB'
#!/usr/bin/env bash
for ((i = 0; i < ${FAKE_MUTANTS:-0}; i++)); do echo "src/lib.rs:$i: mutant"; done
STUB
chmod +x "$SCRATCH/bin/cargo"
export PATH="$SCRATCH/bin:$PATH"

cd "$SCRATCH/work" || exit 1
git init -q -b main . || exit 1
git config user.email t@example.com
git config user.name t
git config commit.gpgsign false
echo base >README.md
git add README.md
git commit -qm base
git update-ref refs/remotes/origin/main HEAD
echo 'fn main() {}' >lib.rs
git add lib.rs
git commit -qm change

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

out="$(FAKE_MUTANTS=13 bash "$PLAN" main --per-shard 6 2>/dev/null)"
check "13 mutants, 6 per shard: three shards" "shards=3" "$(sed -n 1p <<<"$out")"
check "labels are one-based, shards zero-based" \
  'matrix=[{"shard":0,"label":"1/3"},{"shard":1,"label":"2/3"},{"shard":2,"label":"3/3"}]' \
  "$(sed -n 2p <<<"$out")"

out="$(FAKE_MUTANTS=0 bash "$PLAN" main 2>/dev/null)"
check "zero mutants: no shards" "shards=0" "$(sed -n 1p <<<"$out")"
check "zero mutants: empty matrix" "matrix=[]" "$(sed -n 2p <<<"$out")"

out="$(FAKE_MUTANTS=100 bash "$PLAN" main --per-shard 6 --max-shards 2 2>/dev/null)"
check "max-shards caps the label total" \
  'matrix=[{"shard":0,"label":"1/2"},{"shard":1,"label":"2/2"}]' \
  "$(sed -n 2p <<<"$out")"

echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
