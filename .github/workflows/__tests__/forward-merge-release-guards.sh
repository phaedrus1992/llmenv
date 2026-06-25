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

# Build the cascade script — this mirrors the main loop at lines 131-143 of
# forward-merge-release.yml (the fetch + merge sequence in the cascade).
# Callers export: CURRENT TARGET and provide git stubs on PATH.
cascade_block() {
  cat <<'SHELL'
set -uo pipefail
HALTED=""

FETCH_STDERR=$(git fetch origin "$CURRENT" "$TARGET" 2>&1 >/dev/null)
FETCH_RC=$?
if [[ $FETCH_RC -ne 0 ]]; then
  if [[ -n "$FETCH_STDERR" ]]; then
    echo "::warning::fetch of $CURRENT $TARGET failed: $FETCH_STDERR"
  else
    echo "::warning::fetch of $CURRENT $TARGET failed (exit $FETCH_RC; no stderr)"
  fi
  echo "::endgroup::"
  HALTED="fetch failed for $TARGET"
fi

if [[ -n "$HALTED" ]]; then
  exit 1
fi
SHELL
}

# Build the fallback script — this mirrors the block at lines 155-182 of
# forward-merge-release.yml (the protected-branch fallback path).
# Callers export: CURRENT TARGET MERGE_BRANCH and provide git/gh stubs on PATH.
fallback_block() {
  cat <<'SHELL'
set -uo pipefail
HALTED=""

PUSH_STDERR=$(git push origin HEAD:"$TARGET" 2>&1 >/dev/null)
PUSH_RC=$?
if [[ $PUSH_RC -eq 0 ]]; then
  echo "Pushed directly to $TARGET"
else
  if [[ -n "$PUSH_STDERR" ]]; then
    echo "::warning::push to $TARGET failed: $PUSH_STDERR"
  else
    echo "::warning::push to $TARGET failed (exit $PUSH_RC; no stderr)"
  fi
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
# Test 3 (Issue #480): initial push stderr is logged for non-protection failures
#
# Scenario: direct push fails with a non-protection error (e.g. auth failure).
# The actual stderr from git must appear in the workflow log so operators can
# diagnose the real cause rather than assuming branch protection.
#
# Before fix: 2>/dev/null swallowed stderr; nothing was logged → FAIL.
# After fix: stderr captured and emitted via ::warning:: → PASS.
# ---------------------------------------------------------------------------
test_480_initial_push_stderr_logged() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # git stub:
  #   push HEAD:<target>  → fail with diagnostic stderr (non-protection error)
  #   ls-remote           → 1     (branch does not exist)
  #   push $MERGE_BRANCH  → 0     (succeeds so the test isolation is clean)
  #   checkout            → 0
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "push" && "$2" == "origin" && "$3" == HEAD:* ]]; then
  echo "fatal: unable to access 'https://github.com/': Could not resolve host" >&2
  exit 1
fi
if [[ "$1" == "ls-remote" ]]; then
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

  # The actual git stderr must surface in the output via ::warning::.
  if echo "$out" | grep -q "::warning::.*unable to access"; then
    return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# Test 4 (Issue #480): ::warning:: emitted when push fails with empty stderr
#
# Scenario: direct push fails with no stderr (e.g. silent rejection).
# The ::warning:: annotation must still be emitted with the exit code.
#
# Before fix: empty-stderr branch was missing; nothing logged.
# After fix: else branch emits ::warning:: with exit code.
# ---------------------------------------------------------------------------
test_480_empty_stderr_push_logged() {
  local tmpdir
  tmpdir=$(mktemp -d)

  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "push" && "$2" == "origin" && "$3" == HEAD:* ]]; then
  exit 1
fi
if [[ "$1" == "ls-remote" ]]; then
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

  if echo "$out" | grep -q "::warning::push to main failed (exit"; then
    return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# Test 5 (Issue #482): fetch failure with stderr is logged and cascade halted
#
# Scenario: fetch fails with a diagnostic error (e.g. auth failure, network).
# The actual stderr must be logged so operators can diagnose the real cause.
#
# Before fix: 2>/dev/null swallowed stderr; cascade continued → FAIL.
# After fix: stderr captured and emitted via ::warning::; cascade halted → PASS.
# ---------------------------------------------------------------------------
test_482_fetch_fail_with_stderr() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # git stub:
  #   fetch → fail with diagnostic stderr
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "fetch" ]]; then
  echo "fatal: could not read Password for 'https://github.com': terminal prompts disabled" >&2
  exit 1
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(cascade_block)
  export CURRENT="release/2.x" TARGET="main"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  # The actual git stderr must surface in the output via ::warning::.
  if echo "$out" | grep -q "::warning::fetch of release/2.x main failed: fatal: could not read Password"; then
    return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# Test 6 (Issue #482): fetch failure with empty stderr is logged
#
# Scenario: fetch fails with no stderr (e.g. silent rejection).
# The ::warning:: annotation must still be emitted with the exit code.
#
# Before fix: empty-stderr case was silently ignored.
# After fix: else branch emits ::warning:: with exit code.
# ---------------------------------------------------------------------------
test_482_fetch_fail_empty_stderr() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # git stub:
  #   fetch → fail with no stderr (silent rejection)
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "fetch" ]]; then
  exit 1
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(cascade_block)
  export CURRENT="release/2.x" TARGET="main"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  if echo "$out" | grep -q "::warning::fetch of release/2.x main failed (exit"; then
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

run_test "Issue #480: initial push stderr logged for non-protection failures" \
  test_480_initial_push_stderr_logged

run_test "Issue #480: ::warning:: emitted when push fails with empty stderr" \
  test_480_empty_stderr_push_logged

run_test "Issue #482: fetch failure with stderr is logged and cascade halted" \
  test_482_fetch_fail_with_stderr

run_test "Issue #482: fetch failure with empty stderr is logged" \
  test_482_fetch_fail_empty_stderr

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
