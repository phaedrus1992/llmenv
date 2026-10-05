#!/usr/bin/env bash
# Tests for scripts/check-binary-version.sh (#2438). A stub binary prints a version, so no build runs.
# Run: bash .github/workflows/__tests__/check-binary-version.sh
set -uo pipefail

WORKFLOWS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$(cd "$WORKFLOWS_DIR/../.." && pwd)/scripts/check-binary-version.sh"

SCRATCH="$(mktemp -d)"
if [[ -z "$SCRATCH" || ! -d "$SCRATCH" ]]; then
  echo "harness: mktemp -d failed" >&2
  exit 1
fi
trap 'trash "$SCRATCH" 2>/dev/null || true' EXIT

printf '#!/usr/bin/env bash\necho "llmenv 3.12.0 (abc1234-dirty)"\n' >"$SCRATCH/llmenv"
chmod +x "$SCRATCH/llmenv"

failures=0
check() {
  local name="$1" want_rc="$2" want_text="$3"
  shift 3
  local out rc
  out="$(bash "$SCRIPT" "$@" 2>&1)"
  rc=$?
  if [[ "$rc" -ne "$want_rc" || "$out" != *"$want_text"* ]]; then
    echo "FAIL: $name (rc=$rc, want $want_rc; output: $out)"
    failures=$((failures + 1))
  else
    echo "ok: $name"
  fi
}

check "a matching version prints it" 0 "3.12.0" --binary "$SCRATCH/llmenv" --expected 3.12.0
check "a stale binary fails" 1 "reports 3.12.0, but the source version is 3.13.0" \
  --binary "$SCRATCH/llmenv" --expected 3.13.0
check "a missing binary is a tool error" 2 "is not an executable" \
  --binary "$SCRATCH/none" --expected 3.12.0
check "an unknown argument is a usage error" 2 "unknown argument" --nope
check "--binary without a value is a usage error" 2 "needs a path" --binary
check "--help prints the usage" 0 "Usage:" --help

exit "$((failures > 0))"
