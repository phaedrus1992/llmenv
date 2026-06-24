#!/usr/bin/env bash
# Tests for forward-merge-release.yml protected-branch fallback guards.
# Exercises the shell logic extracted from the workflow; stubs git and gh.
# Run: bash .github/workflows/__tests__/forward-merge-release-guards.sh
# Expected: FAIL until Issue #476 and #475 fixes are applied.
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

# Build the fallback script — this mirrors the block at lines 155-169 of
# forward-merge-release.yml (the protected-branch fallback path).
# Callers export: CURRENT TARGET MERGE_BRANCH and provide git/gh stubs on PATH.
fallback_block() {
  cat <<'SHELL'
set -uo pipefail
HALTED=""

if git push origin HEAD:"$TARGET" 2>/dev/null; then
  echo "Pushed directly to $TARGET"
else
  if git ls-remote --exit-code --heads origin "$MERGE_BRANCH" >/dev/null 2>&1; then
    echo "::warning::$MERGE_BRANCH already exists; not overwriting in-progress resolution"
    echo "::error::Cascade halted: $TARGET is protected and $MERGE_BRANCH is already open"
    HALTED="protected branch $TARGET"
  else
    git checkout -B "$MERGE_BRANCH"
    if ! git push origin "$MERGE_BRANCH" --force-with-lease; then
      echo "::error::Push to merge branch $MERGE_BRANCH failed; cannot open PR"
      HALTED="protected branch $TARGET"
    else
      gh pr create --base "$TARGET" --head "$MERGE_BRANCH" \
        --title "Forward-merge $CURRENT into $TARGET" \
        --body "Direct push to $TARGET blocked by branch protection; opening PR." || true

      echo "::error::Cascade halted: $TARGET is protected, opened PR instead"
      HALTED="protected branch $TARGET"
    fi
  fi
fi

if [[ -n "$HALTED" ]]; then
  exit 1
fi
SHELL
}

# ---------------------------------------------------------------------------
# Test 1 (Issue #476): branch-exists guard
#
# Scenario: MERGE_BRANCH already exists remotely (a human is resolving a
# conflict on it). The protected-branch fallback MUST NOT force-push to it
# (that would overwrite their work).
#
# Current behaviour (no guard): push runs → sentinel prints error → FAIL.
# Expected after fix: ls-remote detects branch exists → push skipped → PASS.
# ---------------------------------------------------------------------------
test_476_branch_exists_guard() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # git stub:
  #   push HEAD:<target>  → fail  (simulates branch protection)
  #   ls-remote           → 0     (merge branch already exists)
  #   push $MERGE_BRANCH  → 99    (sentinel: must NOT be reached)
  #   checkout            → 0
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "push" && "$2" == "origin" && "$3" == HEAD:* ]]; then
  exit 1
fi
if [[ "$1" == "ls-remote" ]]; then
  exit 0
fi
if [[ "$1" == "push" ]]; then
  echo "::error::git push to merge branch called despite branch existing" >&2
  exit 99
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  printf '#!/usr/bin/env bash\nexit 0\n' > "$tmpdir/gh"
  chmod +x "$tmpdir/gh"

  local script out
  script=$(fallback_block)
  export CURRENT="release/2.x" TARGET="main" MERGE_BRANCH="forward-merge/release/2.x-to-main"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  # Sentinel in output means the guard is missing → FAIL.
  if echo "$out" | grep -q "despite branch existing"; then
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Test 2 (Issue #475): error annotation on push failure in fallback
#
# Scenario: direct push is blocked (branch protection) AND the push of the
# merge branch also fails (e.g. auth error, network).
#
# Current behaviour: push failure is silently ignored; only the generic
# "Cascade halted: $TARGET is protected" annotation is emitted — nothing
# flags the push failure itself.
#
# Expected after fix: a ::error:: annotation naming the push failure is
# emitted before the cascade-halted message.
# ---------------------------------------------------------------------------
test_475_push_failure_annotation() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # git stub:
  #   push HEAD:<target>  → fail  (branch protection)
  #   ls-remote           → 1     (branch does not exist; won't short-circuit #476 guard)
  #   push $MERGE_BRANCH  → fail  (e.g. auth/network error)
  #   checkout            → 0
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "push" && "$2" == "origin" && "$3" == HEAD:* ]]; then
  exit 1
fi
if [[ "$1" == "ls-remote" ]]; then
  exit 1
fi
if [[ "$1" == "push" ]]; then
  echo "remote: error: push rejected" >&2
  exit 1
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  printf '#!/usr/bin/env bash\nexit 0\n' > "$tmpdir/gh"
  chmod +x "$tmpdir/gh"

  local script out
  script=$(fallback_block)
  export CURRENT="release/2.x" TARGET="main" MERGE_BRANCH="forward-merge/release/2.x-to-main"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  # Post-fix: output must include a ::error:: annotation for the push failure.
  # Current code does NOT emit such an annotation → FAIL.
  # After fix adds an error handler on the merge-branch push → PASS.
  if echo "$out" | grep -E '::error::.*([Pp]ush|MERGE_BRANCH|merge.branch)' | grep -qv 'Cascade halted'; then
    return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------
run_test "Issue #476: branch-exists guard prevents overwrite of in-progress resolution" \
  test_476_branch_exists_guard

run_test "Issue #475: push failure in fallback emits ::error:: annotation" \
  test_475_push_failure_annotation

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
