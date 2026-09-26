#!/usr/bin/env bash
# Tests for forward-merge-release.yml protected-branch fallback guards.
# Exercises the shell logic extracted from the workflow; stubs git and gh.
# Run: bash .github/workflows/__tests__/forward-merge-release-guards.sh
# Expected: FAIL until Issue #476 and #475 fixes are applied.
# cascade_block/fallback_block run under `set -euo pipefail`, matching the
# real workflow (see Issue #1250) — without it, a failing push/fetch/ls-remote
# captured via `VAR=$(cmd); RC=$?` kills the script before RC is ever read,
# and these tests wouldn't catch it.
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
set -euo pipefail

if FETCH_STDERR=$(git fetch origin "$CURRENT" "$TARGET" 2>&1 >/dev/null); then
  FETCH_RC=0
else
  FETCH_RC=$?
fi
if [[ $FETCH_RC -ne 0 ]]; then
  if [[ -n "$FETCH_STDERR" ]]; then
    echo "::warning::fetch of $CURRENT $TARGET failed: $FETCH_STDERR"
  else
    echo "::warning::fetch of $CURRENT $TARGET failed (exit $FETCH_RC; no stderr)"
  fi
  echo "::endgroup::"
  exit 1
fi
SHELL
}

# Build the fallback script — this mirrors the block at lines 155-182 of
# forward-merge-release.yml (the protected-branch fallback path).
# Callers export: CURRENT TARGET MERGE_BRANCH and provide git/gh stubs on PATH.
fallback_block() {
  cat <<'SHELL'
set -euo pipefail
HALTED=""

if PUSH_STDERR=$(git push origin HEAD:"$TARGET" 2>&1 >/dev/null); then
  PUSH_RC=0
else
  PUSH_RC=$?
fi
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
# Test 7 (Issue #1380): the cascade is a chain, not a fan-out
#
# Scenario: push to release/3.x with release/4.x in between, so TARGETS is
# (release/4.x main). Each target must be merged from the PREVIOUS link —
# release/3.x into release/4.x, then release/4.x into main — so main receives
# release/4.x's own commits along with the 3.x fix.
#
# Old behaviour: `git merge origin/$CURRENT` for every target, so main got
# release/3.x merged directly and 4.x's commits never arrived → FAIL.
# Expected after fix: the second merge names release/4.x → PASS.
# ---------------------------------------------------------------------------
chain_block() {
  cat <<'SHELL'
set -euo pipefail
SOURCE_REF="origin/$CURRENT"
SOURCE_DESC="$CURRENT"
for TARGET in $TARGETS; do
  git merge --no-edit "$SOURCE_REF"
  git push origin HEAD:"$TARGET"
  git update-ref "refs/remotes/origin/$TARGET" HEAD
  SOURCE_REF="origin/$TARGET"
  SOURCE_DESC="$TARGET"
done
SHELL
}

test_1380_cascade_chains_through_each_target() {
  local tmpdir
  tmpdir=$(mktemp -d)
  # Stub git: record what each merge was handed, no-op everything else.
  cat > "$tmpdir/git" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == "merge" ]]; then
  echo "MERGED_FROM:${*: -1}"
fi
exit 0
EOF
  chmod +x "$tmpdir/git"

  local script out
  script=$(chain_block)
  export CURRENT="release/3.x" TARGETS="release/4.x main"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  # First merge takes the pushed branch; second takes the branch before it.
  local expected
  expected=$'MERGED_FROM:origin/release/3.x\nMERGED_FROM:origin/release/4.x'
  if [[ "$out" == "$expected" ]]; then
    return 0
  fi
  printf '  expected: %s\n' "${expected//$'\n'/ | }" >&2
  printf '  got:      %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Build the auto-resolution script. The functions are cut out of
# forward-merge-release.yml itself, so this tests the shipped code and cannot
# drift from it. Unlike the blocks above this runs against a real git repo,
# because the whole point of the guard is what the source branch did to the file
# in history.
# Callers export: SOURCE_REF TARGET SOURCE_DESC MANIFEST_CHECK and run it inside a
# conflicted merge.
WORKFLOW="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/forward-merge-release.yml"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

resolve_block() {
  echo 'set -euo pipefail'
  sed -n '/^ *manifest_keeps_target() {/,/^ *for TARGET in/p' "$WORKFLOW" | sed '$d'
  cat <<'SHELL'
if auto_resolve_conflicts; then
  echo "RESOLVED"
else
  echo "BAILED"
fi
SHELL
}

# Build a repo where `source` and `target` both moved their own version, plus
# whatever extra change `$1` adds to source's Cargo.toml. Leaves the caller
# inside a conflicted `git merge source` on the target branch. Echoes the path.
make_version_conflict_repo() {
  local extra="${1:-}" repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    printf '[package]\nversion = "1.0.0"\n\n[dependencies]\nanyhow = { version = "1" }\n' > Cargo.toml
    git add Cargo.toml
    git commit -q -m base

    git switch -q -c source
    printf '[package]\nversion = "4.0.0-alpha.1"\n\n[dependencies]\nanyhow = { version = "1" }\n%s' "$extra" \
      > Cargo.toml
    git commit -q -am "source bump"

    git switch -q target
    printf '[package]\nversion = "5.0.0-alpha.1"\n\n[dependencies]\nanyhow = { version = "1" }\n' > Cargo.toml
    git commit -q -am "target bump"

    git merge --no-commit --no-ff source >/dev/null 2>&1 || true
  )
  printf '%s\n' "$repo"
}

# ---------------------------------------------------------------------------
# Test 8 (Issue #1381): a version-only manifest conflict resolves to the
# target's version.
#
# Two release lines always carry different versions, so release/4.x
# (4.0.0-alpha.1) into main (5.0.0-alpha.1) conflicts on every manifest on
# every single forward-merge. A forward-merge must never change the target's
# own version, so the target's side wins and the cascade continues.
# ---------------------------------------------------------------------------
test_1381_version_only_conflict_keeps_target_version() {
  local repo out version
  repo=$(make_version_conflict_repo "")

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    MANIFEST_CHECK="$REPO_ROOT/scripts/forward_merge_manifest.py" \
    bash -c "$(resolve_block)" 2>&1 || true)
  version=$(cd "$repo" && sed -n 2p Cargo.toml)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *RESOLVED* ]] && [[ "$version" == 'version = "5.0.0-alpha.1"' ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  version kept: %s\n' "$version" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test 9 (Issue #1381): a manifest carrying more than a version bump bails.
#
# The dangerous case. `git checkout --ours` throws away the source's whole
# file, so if the source also added a dependency, auto-resolving would drop it
# silently and main would build without it. Note the added line contains
# `version = "1"` — a regex looking for version-shaped changed lines would call
# this safe, which is why the guard compares whole files with versions blanked.
# ---------------------------------------------------------------------------
test_1381_non_version_change_bails() {
  local repo out
  repo=$(make_version_conflict_repo 'serde = { version = "1" }\n')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    MANIFEST_CHECK="$REPO_ROOT/scripts/forward_merge_manifest.py" \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *BAILED* ]] && [[ "$out" == *"more than version numbers"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Build a repo where source and target both bumped the same pin (Renovate on
# two release lines), each moved its own version, and each rewrote both
# lockfiles. `$1` is the clap pin the source moves to; the target is on 1.0.1.
# Leaves the repo inside a conflicted `git merge source`. Echoes the path.
make_pin_drift_repo() {
  local source_pin="$1" repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir website
    write_drift_files 1.0.0 1.0.0 base base
    git add -A
    git commit -q -m base

    git switch -q -c source
    write_drift_files "$source_pin" 3.11.2 source-lock source-npm
    git commit -q -am "source"

    git switch -q target
    write_drift_files 1.0.1 4.0.0-alpha.1 target-lock target-npm
    git commit -q -am "target"

    git merge --no-commit --no-ff source >/dev/null 2>&1 || true
  )
  printf '%s\n' "$repo"
}

# Args: clap pin, package version, Cargo.lock body, package-lock.json body.
write_drift_files() {
  printf '[package]\nversion = "%s"\n\n[dependencies]\nclap = { version = "=%s" }\n' "$2" "$1" > Cargo.toml
  printf '%s\n' "$3" > Cargo.lock
  printf '%s\n' "$4" > website/package-lock.json
}

# Put `cargo` and `npm` stubs on PATH. Each rewrites its lockfile to a marker,
# or exits non-zero when FAIL_TOOL names it. Echoes the stub directory.
make_tool_stubs() {
  local dir
  dir=$(mktemp -d)
  cat > "$dir/cargo" <<'STUB'
#!/usr/bin/env bash
[[ "${FAIL_TOOL:-}" == cargo ]] && exit 1
echo regenerated-by-cargo > Cargo.lock
STUB
  cat > "$dir/npm" <<'STUB'
#!/usr/bin/env bash
[[ "${FAIL_TOOL:-}" == npm ]] && exit 1
echo regenerated-by-npm > package-lock.json
STUB
  chmod +x "$dir/cargo" "$dir/npm"
  printf '%s\n' "$dir"
}

# Run the resolution block in a pin-drift repo. Sets DRIFT_OUT and DRIFT_REPO;
# the caller removes the repo.
run_drift() {
  local source_pin="$1" stubs
  DRIFT_REPO=$(make_pin_drift_repo "$source_pin")
  stubs=$(make_tool_stubs)
  DRIFT_OUT=$(cd "$DRIFT_REPO" && PATH="$stubs:$PATH" FAIL_TOOL="${FAIL_TOOL:-}" \
    SOURCE_REF=source TARGET=release/4.x SOURCE_DESC=release/3.x \
    MANIFEST_CHECK="$REPO_ROOT/scripts/forward_merge_manifest.py" \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Test 9a (Issue #2166): the same pin bumped on both release lines, plus both
# lockfiles rewritten on both lines, resolves with no human step.
# ---------------------------------------------------------------------------
test_2166_same_pin_bump_and_lockfiles_resolve() {
  local manifest cargo_lock npm_lock
  run_drift 1.0.1
  manifest=$(cd "$DRIFT_REPO" && cat Cargo.toml)
  cargo_lock=$(cd "$DRIFT_REPO" && cat Cargo.lock)
  npm_lock=$(cd "$DRIFT_REPO" && cat website/package-lock.json)
  trash "$DRIFT_REPO" 2>/dev/null || true

  if [[ "$DRIFT_OUT" == *RESOLVED* ]] && [[ "$manifest" == *'version = "4.0.0-alpha.1"'* ]] \
      && [[ "$cargo_lock" == regenerated-by-cargo ]] && [[ "$npm_lock" == regenerated-by-npm ]]; then
    return 0
  fi
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

# A source pin newer than the target's is a real change and must still bail.
test_2166_newer_source_pin_bails() {
  run_drift 1.0.2
  trash "$DRIFT_REPO" 2>/dev/null || true
  [[ "$DRIFT_OUT" == *BAILED* ]] && [[ "$DRIFT_OUT" == *"newer than target"* ]] && return 0
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

# A failing lockfile tool bails and names the command.
test_2166_lockfile_tool_failure_bails_naming_command() {
  FAIL_TOOL=npm run_drift 1.0.1
  trash "$DRIFT_REPO" 2>/dev/null || true
  [[ "$DRIFT_OUT" == *BAILED* ]] && [[ "$DRIFT_OUT" == *"npm install --package-lock-only"* ]] \
    && return 0
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test 10 (Issue #1912): retry trigger determines the correct source branch
#
# The cascade halts when forward-merge/<source>-to-<target> is already open,
# and nothing re-fires once that PR merges (push only fires once per commit).
# A pull_request(closed) trigger re-derives the source branch from the merged
# branch's own name and re-checks it out, so the rest of the job runs exactly
# as if the source had just been pushed again.
#
# This mirrors the "Determine source branch" step in forward-merge-release.yml.
# Callers export: EVENT_NAME PUSHED_REF PR_HEAD_REF.
# ---------------------------------------------------------------------------
determine_source_block() {
  cat <<'SHELL'
set -euo pipefail
if [[ "$EVENT_NAME" == "push" ]]; then
  echo "ref=$PUSHED_REF"
else
  rest="${PR_HEAD_REF#forward-merge/}"
  source_branch="${rest%-to-*}"
  if [[ ! "$source_branch" =~ ^(release/[0-9]+\.x|main)$ ]]; then
    echo "::error::could not parse a valid source branch out of forward-merge PR head ref '$PR_HEAD_REF' (expected forward-merge/<source>-to-<target>, source one of release/X.x or main)"
    exit 1
  fi
  echo "ref=$source_branch"
fi
SHELL
}

test_1912_push_event_uses_pushed_ref() {
  local out
  out=$(EVENT_NAME=push PUSHED_REF=release/3.x PR_HEAD_REF='' \
    bash -c "$(determine_source_block)" 2>&1 || true)
  [[ "$out" == "ref=release/3.x" ]]
}

test_1912_merged_pr_parses_source_from_branch_name() {
  local out
  out=$(EVENT_NAME=pull_request PUSHED_REF='' \
    PR_HEAD_REF="forward-merge/release/3.x-to-main" \
    bash -c "$(determine_source_block)" 2>&1 || true)
  [[ "$out" == "ref=release/3.x" ]]
}

test_1912_malformed_head_ref_fails_loudly_instead_of_silent_fallback() {
  local out rc
  out=$(EVENT_NAME=pull_request PUSHED_REF='' \
    PR_HEAD_REF="forward-merge/not-a-real-branch" \
    bash -c "$(determine_source_block)" 2>&1)
  rc=$?
  [[ "$rc" -ne 0 ]] && echo "$out" | grep -q "::error::could not parse a valid source branch"
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

run_test "Issue #1380: cascade chains 3.x -> 4.x -> main instead of fanning out" \
  test_1380_cascade_chains_through_each_target

run_test "Issue #1381: version-only manifest conflict keeps the target's version" \
  test_1381_version_only_conflict_keeps_target_version

run_test "Issue #1381: a manifest change beyond the version bails instead of dropping it" \
  test_1381_non_version_change_bails

run_test "Issue #2166: same pin bumped on both lines and lockfiles resolve with no human step" \
  test_2166_same_pin_bump_and_lockfiles_resolve

run_test "Issue #2166: a source pin newer than the target still bails" \
  test_2166_newer_source_pin_bails

run_test "Issue #2166: a failing lockfile tool bails and names the command" \
  test_2166_lockfile_tool_failure_bails_naming_command

run_test "Issue #1912: push event uses the pushed ref as the source branch" \
  test_1912_push_event_uses_pushed_ref

run_test "Issue #1912: merged forward-merge PR parses its source branch from its own head ref" \
  test_1912_merged_pr_parses_source_from_branch_name

run_test "Issue #1912: malformed head ref fails loudly instead of falling back silently" \
  test_1912_malformed_head_ref_fails_loudly_instead_of_silent_fallback

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
