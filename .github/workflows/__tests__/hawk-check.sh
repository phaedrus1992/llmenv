#!/usr/bin/env bash
# Tests for scripts/hawk-check.sh (#2312). Stub `cargo` records the hawk
# arguments and answers `cargo metadata`, so no real hawk build runs.
# Run: bash .github/workflows/__tests__/hawk-check.sh
set -uo pipefail

WORKFLOWS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$(cd "$WORKFLOWS_DIR/../.." && pwd)/scripts/hawk-check.sh"

SCRATCH="$(mktemp -d)"
if [[ -z "$SCRATCH" || ! -d "$SCRATCH" ]]; then
  echo "harness: mktemp -d failed" >&2
  exit 1
fi
trap 'trash "$SCRATCH" 2>/dev/null || true' EXIT

mkdir -p "$SCRATCH/bin" "$SCRATCH/nojq" "$SCRATCH/repo" || exit 1
RECORD="$SCRATCH/hawk-args"
cat >"$SCRATCH/bin/cargo" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == metadata ]]; then
  echo '{"target_directory":"/workspace/target"}'
  exit 0
fi
printf '%s\n' "\$@" >"$RECORD"
STUB
chmod +x "$SCRATCH/bin/cargo"

# A PATH without jq: the system tools the script needs, plus the stub.
for tool in bash dirname git env; do
  src="$(command -v "$tool")" && ln -sf "$src" "$SCRATCH/nojq/$tool"
done
ln -sf "$SCRATCH/bin/cargo" "$SCRATCH/nojq/cargo"

git -C "$SCRATCH/repo" init -q . || exit 1

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

# The hawk arguments after the exclusion list, one line.
tail_args() { sed -n '/^--exclude-crate$/,$p' "$RECORD" | grep -v -e '^--exclude-crate$' -e '^llmenv_' | tr '\n' ' '; }

PATH="$SCRATCH/bin:$PATH" bash "$SCRIPT" -D warnings
check "default target dir is <target>/hawk, -D warnings last" \
  "--target-dir /workspace/target/hawk -D warnings " "$(tail_args)"

PATH="$SCRATCH/bin:$PATH" bash "$SCRIPT" --target-dir /y -D warnings
check "caller --target-dir wins" "--target-dir /y -D warnings " "$(tail_args)"

PATH="$SCRATCH/bin:$PATH" bash "$SCRIPT" --target-dir=/z -D warnings
check "caller --target-dir=VALUE wins" "--target-dir=/z -D warnings " "$(tail_args)"

(cd "$SCRATCH/repo" && PATH="$SCRATCH/nojq" CARGO_TARGET_DIR=/x bash "$SCRIPT" -D warnings)
check "without jq, CARGO_TARGET_DIR is honored" \
  "--target-dir /x/hawk -D warnings " "$(tail_args)"

(cd "$SCRATCH/repo" && PATH="$SCRATCH/nojq" bash "$SCRIPT" -D warnings)
root="$(cd "$SCRATCH/repo" && pwd -P)"
check "without jq, the default is the git root target dir" \
  "--target-dir $root/target/hawk -D warnings " "$(tail_args)"

echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
