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

# Build the auto-resolution script — mirrors version_only_change and
# auto_resolve_conflicts in forward-merge-release.yml. Unlike the blocks above
# this runs against a real git repo, because the whole point of the guard is
# what the source branch did to the file in history.
# Callers export: SOURCE_REF TARGET SOURCE_DESC and run it inside a conflicted merge.
resolve_block() {
  cat <<'SHELL'
set -euo pipefail

version_only_change() {
  local file="$1" base before after
  base=$(git merge-base HEAD "$SOURCE_REF") || return 1
  before=$(git show "$base:$file" 2>/dev/null \
    | sed -E 's/version = "[^"]*"/version = "*"/g') || return 1
  after=$(git show "$SOURCE_REF:$file" 2>/dev/null \
    | sed -E 's/version = "[^"]*"/version = "*"/g') || return 1
  [[ "$before" == "$after" ]]
}

auto_resolve_conflicts() {
  local file conflicted_files remaining
  conflicted_files="$(git diff --name-only --diff-filter=U)" || {
    echo "::error::failed to list conflicted files" >&2
    return 1
  }
  while IFS= read -r file; do
    [[ -n "$file" ]] || continue
    case "$file" in
      website/docs/changelog.md)
        echo "  $file: regenerating from the merged CHANGELOG-*.md sources"
        git checkout --ours -- "$file" || {
          echo "::error::$file: git checkout --ours failed" >&2
          return 1
        }
        local script_path="scripts/sync-changelog-doc.sh" target_script source_script tmp_script
        target_script=$(git show "HEAD:$script_path" 2>/dev/null) || {
          echo "::error::$file: failed to read $TARGET's copy of $script_path" >&2
          return 1
        }
        source_script=$(git show "$SOURCE_REF:$script_path" 2>/dev/null) || {
          echo "::error::$file: failed to read $SOURCE_DESC's copy of $script_path" >&2
          return 1
        }
        if [[ "$target_script" != "$source_script" ]]; then
          echo "::error::$file: $script_path differs between $SOURCE_DESC and $TARGET; a script-logic change must be forward-merged and reviewed by hand, not auto-run" >&2
          return 1
        fi
        # Materialized inside scripts/ (not /tmp) so the script's own
        # `cd "$(dirname "$0")/.."` still lands on the repo root. Written
        # from the already-verified $target_script, not a second `git show`,
        # so there's no gap between what was compared and what runs.
        tmp_script=$(mktemp "$PWD/scripts/.sync-changelog-doc.XXXXXX") || {
          echo "::error::$file: failed to create temp file for $script_path" >&2
          return 1
        }
        printf '%s\n' "$target_script" > "$tmp_script" || {
          echo "::error::$file: failed to write pinned copy of $script_path" >&2
          rm -f "$tmp_script"
          return 1
        }
        if ! bash "$tmp_script"; then
          echo "::error::$file: scripts/sync-changelog-doc.sh failed to regenerate the changelog" >&2
          rm -f "$tmp_script"
          return 1
        fi
        rm -f "$tmp_script"
        git add -- "$file"
        ;;
      Cargo.toml | Cargo.lock | crates/*/Cargo.toml)
        if ! version_only_change "$file"; then
          echo "::error::$file: $SOURCE_DESC changed more than version numbers here, so keeping $TARGET's copy could drop real changes"
          return 1
        fi
        echo "  $file: keeping $TARGET's own version"
        git checkout --ours -- "$file"
        git add -- "$file"
        ;;
      *)
        echo "::error::$file: conflict has no auto-resolution rule"
        return 1
        ;;
    esac
  done <<< "$conflicted_files"
  remaining="$(git diff --name-only --diff-filter=U)" || {
    echo "::error::failed to verify remaining conflicts" >&2
    return 1
  }
  [[ -z "$remaining" ]]
}

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
    printf 'version = "1.0.0"\n\n[dependencies]\nanyhow = { version = "1" }\n' > Cargo.toml
    git add Cargo.toml
    git commit -q -m base

    git switch -q -c source
    printf 'version = "4.0.0-alpha.1"\n\n[dependencies]\nanyhow = { version = "1" }\n%s' "$extra" \
      > Cargo.toml
    git commit -q -am "source bump"

    git switch -q target
    printf 'version = "5.0.0-alpha.1"\n\n[dependencies]\nanyhow = { version = "1" }\n' > Cargo.toml
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
    bash -c "$(resolve_block)" 2>&1 || true)
  version=$(cd "$repo" && head -1 Cargo.toml)
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
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *BAILED* ]] && [[ "$out" == *"more than version numbers"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1525): a `git diff` failure inside auto_resolve_conflicts
# fails loudly instead of being read as "no conflicted files left".
#
# The old code streamed `git diff --name-only --diff-filter=U` straight into
# a `while read ... done < <(...)` and re-ran the same command bare in the
# closing `[[ -z "$(...)" ]]` check. Either form hides a `git diff` failure
# under `set -e`: a process substitution's exit status is invisible to the
# consuming loop, and a bare `$(...)` failure still yields an empty string
# that reads as "nothing left" — so a transient failure would report
# RESOLVED with a real conflict left unprocessed in the tree.
# ---------------------------------------------------------------------------
test_1525_auto_resolve_conflicts_fails_on_git_diff_failure() {
  local tmpdir
  tmpdir=$(mktemp -d)

  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "diff" && "$2" == "--name-only" && "$3" == "--diff-filter=U" ]]; then
  exit 1
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(resolve_block)
  export SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  unset SOURCE_REF TARGET SOURCE_DESC
  rm -rf "$tmpdir"

  if [[ "$out" == *BAILED* ]] && [[ "$out" == *"failed to list conflicted files"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Build a repo with a website/docs/changelog.md conflict, plus a stub
# scripts/sync-changelog-doc.sh that always fails. Leaves the caller inside a
# conflicted `git merge source` on the target branch. Echoes the path.
make_changelog_conflict_repo() {
  local repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir -p website/docs scripts
    printf 'base content\n' > website/docs/changelog.md
    printf 'exit 1\n' > scripts/sync-changelog-doc.sh
    git add website/docs/changelog.md scripts/sync-changelog-doc.sh
    git commit -q -m base

    git switch -q -c source
    printf 'source content\n' > website/docs/changelog.md
    git commit -q -am "source change"

    git switch -q target
    printf 'target content\n' > website/docs/changelog.md
    git commit -q -am "target change"

    git merge --no-commit --no-ff source >/dev/null 2>&1 || true
  )
  printf '%s\n' "$repo"
}

# ---------------------------------------------------------------------------
# Test (Issue #1525): a scripts/sync-changelog-doc.sh failure inside the
# changelog case-branch bails instead of reporting resolved.
#
# auto_resolve_conflicts is invoked as `if auto_resolve_conflicts; then` —
# bash suppresses `set -e` for the whole body of a function called as part of
# an if/while condition, so an unchecked failure wouldn't abort; it would
# just fall through to `git add`, staging whatever the failed regeneration
# left behind, and the function's return status would reflect only that
# `git add`'s (successful) exit code, reporting RESOLVED. Both git checkout
# --ours and bash scripts/sync-changelog-doc.sh in that branch must be
# checked explicitly.
# ---------------------------------------------------------------------------
test_1525_changelog_regeneration_failure_bails() {
  local repo out
  repo=$(make_changelog_conflict_repo)

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *BAILED* ]] && [[ "$out" == *"failed to regenerate the changelog"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Build a repo where target and source both carry scripts/sync-changelog-doc.sh
# (with the given, possibly differing, content) plus a changelog.md conflict.
# Only target commits touch changelog.md and only source commits touch the
# script, so the script itself never conflicts — it merges silently, which is
# exactly the Issue #1532 scenario. Leaves the caller inside a conflicted
# `git merge source` on the target branch. Echoes the path.
# ---------------------------------------------------------------------------
make_changelog_script_repo() {
  local target_script="$1" source_script="$2" repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir -p website/docs scripts
    printf 'base content\n' > website/docs/changelog.md
    printf '%s\n' "$target_script" > scripts/sync-changelog-doc.sh
    git add website/docs/changelog.md scripts/sync-changelog-doc.sh
    git commit -q -m base

    git switch -q -c source
    printf 'source content\n' > website/docs/changelog.md
    printf '%s\n' "$source_script" > scripts/sync-changelog-doc.sh
    git commit -q -am "source change"

    git switch -q target
    printf 'target content\n' > website/docs/changelog.md
    git commit -q -am "target change"

    git merge --no-commit --no-ff source >/dev/null 2>&1 || true
  )
  printf '%s\n' "$repo"
}

# ---------------------------------------------------------------------------
# Test (Issue #1532): identical script on source and target runs the target's
# pinned copy of scripts/sync-changelog-doc.sh.
# ---------------------------------------------------------------------------
test_1532_identical_script_runs_target_copy() {
  local repo out marker
  repo=$(make_changelog_script_repo \
    'echo "TARGET_SCRIPT_RAN" > ran-marker.txt' \
    'echo "TARGET_SCRIPT_RAN" > ran-marker.txt')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  marker=$(cat "$repo/ran-marker.txt" 2>/dev/null || echo "MISSING")
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *RESOLVED* ]] && [[ "$marker" == "TARGET_SCRIPT_RAN" ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  marker: %s\n' "$marker" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1532): a scripts/sync-changelog-doc.sh that differs between
# source and target bails instead of running either copy.
#
# Before fix: the script file isn't itself conflicted (only changelog.md is),
# so the merge silently resolves it to the source's copy, and that untrusted
# copy runs with FORWARD_MERGE_PAT/GH_TOKEN in env → SOURCE_SCRIPT_RAN, FAIL.
# After fix: source and target copies are diffed explicitly; a mismatch bails
# before either runs → BAILED, no marker file, PASS.
# ---------------------------------------------------------------------------
test_1532_differing_script_bails() {
  local repo out marker_exists
  repo=$(make_changelog_script_repo \
    'echo "TARGET_SCRIPT_RAN" > ran-marker.txt' \
    'echo "SOURCE_SCRIPT_RAN" > ran-marker.txt')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  [[ -f "$repo/ran-marker.txt" ]] && marker_exists=1 || marker_exists=0
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *BAILED* ]] && [[ "$out" == *"differs between"* ]] && [[ "$marker_exists" -eq 0 ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  marker_exists: %s\n' "$marker_exists" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1532): FORWARD_MERGE_PAT must not be inherited by any
# subprocess the cascade step spawns (e.g. sync-changelog-doc.sh); only
# push_with_pat, via a captured local variable, still has access.
#
# Before fix: FORWARD_MERGE_PAT stays in the step's own env for its whole
# duration, so a spawned subprocess inherits it → LEAKED, FAIL.
# After fix: it's captured into a local variable and unset immediately →
# NOT_LEAKED, and push_with_pat still authenticates via the local var, PASS.
# ---------------------------------------------------------------------------
pat_scope_block() {
  cat <<'SHELL'
set -euo pipefail
_forward_merge_pat="${FORWARD_MERGE_PAT:-}"
unset FORWARD_MERGE_PAT

push_with_pat() {
  if [[ -n "$_forward_merge_pat" ]]; then
    local auth
    auth="$(printf 'x-access-token:%s' "$_forward_merge_pat" | base64 -w0)"
    echo "::add-mask::$auth"
    git -c http.https://github.com/.extraheader="AUTHORIZATION: basic ${auth}" push "$@"
  else
    git push "$@"
  fi
}

bash -c 'if [[ -n "${FORWARD_MERGE_PAT:-}" ]]; then echo "LEAKED"; else echo "NOT_LEAKED"; fi'

push_with_pat origin HEAD:main
SHELL
}

test_1532_forward_merge_pat_not_inherited_by_subprocess() {
  local tmpdir
  tmpdir=$(mktemp -d)

  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "-c" && "$2" == http.https://github.com/.extraheader=* && "$3" == "push" ]]; then
  echo "PUSHED_WITH_EXTRAHEADER"
  exit 0
fi
if [[ "$1" == "push" ]]; then
  echo "PUSHED_WITHOUT_EXTRAHEADER"
  exit 0
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(pat_scope_block)
  export FORWARD_MERGE_PAT="fake-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  unset FORWARD_MERGE_PAT
  rm -rf "$tmpdir"

  [[ "$out" == *"NOT_LEAKED"* ]] && [[ "$out" == *"::add-mask::"* ]] && [[ "$out" == *"PUSHED_WITH_EXTRAHEADER"* ]]
}

# ---------------------------------------------------------------------------
# Build a repo carrying the REAL, currently-committed scripts/sync-changelog-doc.sh
# (read from this checkout, not a stub) plus a minimal CHANGELOG-1.md, so the
# regeneration actually exercises the script's own `cd "$(dirname "$0")/.."`
# logic. Leaves the caller inside a conflicted `git merge source` on the
# target branch. Echoes the path.
# ---------------------------------------------------------------------------
make_real_changelog_script_repo() {
  local real_script repo
  real_script="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)/scripts/sync-changelog-doc.sh"
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir -p website/docs scripts
    cp "$real_script" scripts/sync-changelog-doc.sh
    printf '## [1.0.0] - 2026-01-01\n\n### Added\n\n- Base entry.\n' > CHANGELOG-1.md
    printf 'base content\n' > website/docs/changelog.md
    git add website/docs/changelog.md scripts/sync-changelog-doc.sh CHANGELOG-1.md
    git commit -q -m base

    git switch -q -c source
    printf 'source content\n' > website/docs/changelog.md
    git commit -q -am "source change"

    git switch -q target
    printf 'target content\n' > website/docs/changelog.md
    git commit -q -am "target change"

    git merge --no-commit --no-ff source >/dev/null 2>&1 || true
  )
  printf '%s\n' "$repo"
}

# ---------------------------------------------------------------------------
# Test (Issue #1532): the real scripts/sync-changelog-doc.sh actually
# regenerates website/docs/changelog.md when run through the pinned-copy
# path — not just a stub that happens to exit 0.
#
# The real script does `cd "$(dirname "$0")/.."`, so a pinned copy materialized
# outside the repo (e.g. a plain `mktemp` under /tmp) resolves that cd to the
# wrong directory and the script dies before it ever touches changelog.md —
# exactly the regression the stub-based tests above can't see.
# ---------------------------------------------------------------------------
test_1532_real_script_regenerates_changelog() {
  local repo out regenerated
  repo=$(make_real_changelog_script_repo)

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  regenerated=$(cat "$repo/website/docs/changelog.md" 2>/dev/null || echo "MISSING")
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *RESOLVED* ]] && [[ "$regenerated" == *"1.0.0"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  changelog.md: %s\n' "${regenerated//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test 10 (Issue #1504): push_with_pat scopes the PAT to just the push
#
# Mirrors the push_with_pat() helper in forward-merge-release.yml. Verifies
# it authenticates with FORWARD_MERGE_PAT (via a per-invocation -c override,
# not the job-wide persisted credential) when the secret is set, and falls
# back to a plain `git push` (the checkout's own persisted credential) when
# it isn't.
# ---------------------------------------------------------------------------
push_with_pat_block() {
  cat <<'SHELL'
set -euo pipefail
_forward_merge_pat="${FORWARD_MERGE_PAT:-}"
unset FORWARD_MERGE_PAT

push_with_pat() {
  if [[ -n "$_forward_merge_pat" ]]; then
    local auth
    auth="$(printf 'x-access-token:%s' "$_forward_merge_pat" | base64 -w0)"
    echo "::add-mask::$auth"
    git -c http.https://github.com/.extraheader="AUTHORIZATION: basic ${auth}" push "$@"
  else
    git push "$@"
  fi
}

push_with_pat origin HEAD:main
SHELL
}

test_1504_push_with_pat_uses_pat_when_set() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # git stub: record whether -c http...extraheader was passed before "push".
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "-c" && "$2" == http.https://github.com/.extraheader=* && "$3" == "push" ]]; then
  echo "PUSHED_WITH_EXTRAHEADER"
  exit 0
fi
if [[ "$1" == "push" ]]; then
  echo "PUSHED_WITHOUT_EXTRAHEADER"
  exit 0
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(push_with_pat_block)
  export FORWARD_MERGE_PAT="fake-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  unset FORWARD_MERGE_PAT
  rm -rf "$tmpdir"

  [[ "$out" == *"::add-mask::"* ]] && [[ "$out" == *"PUSHED_WITH_EXTRAHEADER"* ]]
}

# ---------------------------------------------------------------------------
# Test 11 (Issue #1504): direct push to $TARGET is never attempted once
# FORWARD_MERGE_PAT is set.
#
# This repo's branch-protection ruleset has an admin bypass_actor with
# bypass_mode "always" (verified via `gh api repos/.../rulesets`), so an
# admin-owned PAT could push straight past required checks instead of being
# rejected the way GITHUB_TOKEN would be. Mirrors the direct-push decision at
# forward-merge-release.yml's cascade step: once the secret is set, the
# direct `git push origin HEAD:"$TARGET"` must never run at all — the
# cascade must go straight to the PR path so required checks still run.
# ---------------------------------------------------------------------------
direct_push_decision_block() {
  cat <<'SHELL'
set -euo pipefail
_forward_merge_pat="${FORWARD_MERGE_PAT:-}"
unset FORWARD_MERGE_PAT
TARGET="main"
PUSH_SKIPPED=0
if [[ -n "$_forward_merge_pat" ]]; then
  PUSH_SKIPPED=1
  PUSH_RC=1
  PUSH_STDERR=""
elif PUSH_STDERR=$(git push origin HEAD:"$TARGET" 2>&1 >/dev/null); then
  PUSH_RC=0
else
  PUSH_RC=$?
fi
if [[ $PUSH_RC -eq 0 ]]; then
  echo "DIRECT_PUSH_SUCCEEDED"
elif [[ "$PUSH_SKIPPED" -eq 1 ]]; then
  echo "SKIPPED_DIRECT_PUSH_OPENING_PR"
else
  echo "DIRECT_PUSH_FAILED"
fi
SHELL
}

test_1504_direct_push_never_attempted_with_pat_set() {
  local tmpdir
  tmpdir=$(mktemp -d)

  # Sentinel: a direct `push origin HEAD:main` call means the skip logic
  # didn't fire — must never be reached in this test.
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "push" && "$2" == "origin" && "$3" == "HEAD:main" ]]; then
  echo "::error::direct push attempted despite FORWARD_MERGE_PAT being set" >&2
  exit 99
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(direct_push_decision_block)
  export FORWARD_MERGE_PAT="fake-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  unset FORWARD_MERGE_PAT
  rm -rf "$tmpdir"

  [[ "$out" == "SKIPPED_DIRECT_PUSH_OPENING_PR" ]]
}

test_1504_direct_push_still_attempted_without_pat() {
  local tmpdir
  tmpdir=$(mktemp -d)

  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "push" && "$2" == "origin" && "$3" == "HEAD:main" ]]; then
  exit 0
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(direct_push_decision_block)
  unset FORWARD_MERGE_PAT

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  [[ "$out" == "DIRECT_PUSH_SUCCEEDED" ]]
}

test_1504_push_with_pat_falls_back_without_secret() {
  local tmpdir
  tmpdir=$(mktemp -d)

  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "-c" ]]; then
  echo "PUSHED_WITH_EXTRAHEADER"
  exit 0
fi
if [[ "$1" == "push" ]]; then
  echo "PUSHED_WITHOUT_EXTRAHEADER"
  exit 0
fi
exit 0
STUB
  chmod +x "$tmpdir/git"

  local script out
  script=$(push_with_pat_block)
  unset FORWARD_MERGE_PAT

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  rm -rf "$tmpdir"

  [[ "$out" == "PUSHED_WITHOUT_EXTRAHEADER" ]]
}

# ---------------------------------------------------------------------------
# Test (Issue #1540): push_with_pat's PAT header must not collide with the
# persisted GITHUB_TOKEN extraheader that actions/checkout's
# persist-credentials: true step already wrote into .git/config.
#
# http.extraheader is a multi-valued git config key: a `-c` override on the
# command line adds a value alongside a persisted one, it does not replace
# it. Verified directly against real git (not a stub) below, since this is a
# property of git's own config layering, not of this workflow's logic — an
# empty `-c key=` override does NOT unset a persisted value either, it adds
# a third, empty entry. Before the fix, invoking push_with_pat while a
# persisted extraheader value exists leaves BOTH values in effect, and
# GitHub's server rejects the resulting double `Authorization` header with a
# 400 ("Duplicate header"). The fix must remove the persisted local-config
# entry before setting the PAT's own header via -c, so exactly one
# Authorization header is in effect.
#
# This mirrors push_with_pat with its trailing git subcommand parameterized
# (production hardcodes `push`) so the test can inspect the resulting git
# config state via `config --get-all` instead of needing a real remote push.
# ---------------------------------------------------------------------------
push_with_pat_header_block() {
  cat <<'SHELL'
set -euo pipefail
_forward_merge_pat="${FORWARD_MERGE_PAT:-}"
unset FORWARD_MERGE_PAT

push_with_pat() {
  if [[ -n "$_forward_merge_pat" ]]; then
    local auth
    auth="$(printf 'x-access-token:%s' "$_forward_merge_pat" | base64 -w0)"
    echo "::add-mask::$auth"
    if UNSET_OUT=$(git config --local --unset-all http.https://github.com/.extraheader 2>&1); then
      UNSET_RC=0
    else
      UNSET_RC=$?
    fi
    if [[ $UNSET_RC -ne 0 && $UNSET_RC -ne 5 ]]; then
      echo "::error::failed to clear persisted extraheader before PAT push (git config exit $UNSET_RC): $UNSET_OUT"
      exit 1
    fi
    while IFS= read -r include_key; do
      [[ -n "$include_key" ]] || continue
      if INCLUDE_UNSET_OUT=$(git config --local --unset-all "$include_key" 2>&1); then
        INCLUDE_UNSET_RC=0
      else
        INCLUDE_UNSET_RC=$?
      fi
      if [[ $INCLUDE_UNSET_RC -ne 0 ]]; then
        echo "::error::failed to remove $include_key before PAT push (git config exit $INCLUDE_UNSET_RC): $INCLUDE_UNSET_OUT"
        exit 1
      fi
    done < <(git config --local --name-only --get-regexp '^includeIf\.gitdir:' 2>/dev/null || true)
    git -c http.https://github.com/.extraheader="AUTHORIZATION: basic ${auth}" "$@"
  else
    git "$@"
  fi
}

push_with_pat config --get-all http.https://github.com/.extraheader
SHELL
}

test_1540_push_with_pat_clears_persisted_header_before_setting_new() {
  local repo out header_count
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    # Simulate actions/checkout's persist-credentials: true, which writes a
    # persistent extraheader entry into .git/config for the whole job.
    git config --local --add http.https://github.com/.extraheader \
      "AUTHORIZATION: basic PERSISTED_TOKEN"
  )

  out=$(cd "$repo" && FORWARD_MERGE_PAT="fake-pat-token" \
    bash -c "$(push_with_pat_header_block)" 2>&1)
  header_count=$(echo "$out" | grep -c '^AUTHORIZATION: basic')
  trash "$repo" 2>/dev/null || true

  if [[ "$header_count" -eq 1 ]] && ! echo "$out" | grep -q "PERSISTED_TOKEN"; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  header_count: %s\n' "$header_count" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540 follow-up): actions/checkout's persist-credentials: true
# does NOT write the extraheader directly into .git/config — confirmed from
# a live forward-merge run's own Checkout step log, after PR #1544's fix
# (which only handled a directly-set extraheader) still hit the identical
# "Duplicate header" 400 in production. The real header lives in an
# external file, pulled in only via `includeIf.gitdir:*.path` entries in
# .git/config. A plain `--unset-all` on the extraheader key finds nothing
# there (exit 5) and silently no-ops while the header stays effectively
# active via the include — this reproduces that exact shape, not just a
# directly-set extraheader.
# ---------------------------------------------------------------------------
test_1540_push_with_pat_clears_includeif_persisted_credential() {
  local repo credfile out header_count
  repo=$(mktemp -d)
  credfile=$(mktemp)
  printf '[http "https://github.com/"]\n\textraheader = AUTHORIZATION: basic PERSISTED_TOKEN\n' \
    > "$credfile"
  (
    cd "$repo" || exit 1
    git init -q .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    # Mirrors actions/checkout: the real header lives in an external file,
    # referenced only via includeIf — never written directly into
    # .git/config's own [http] section.
    git config --local "includeIf.gitdir:$repo/.git.path" "$credfile"
  )

  out=$(cd "$repo" && FORWARD_MERGE_PAT="fake-pat-token" \
    bash -c "$(push_with_pat_header_block)" 2>&1)
  header_count=$(echo "$out" | grep -c '^AUTHORIZATION: basic')
  trash "$repo" 2>/dev/null || true
  trash "$credfile" 2>/dev/null || true

  if [[ "$header_count" -eq 1 ]] && ! echo "$out" | grep -q "PERSISTED_TOKEN"; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  header_count: %s\n' "$header_count" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540 code review): a real config-write failure while clearing
# the persisted header must halt the cascade with a diagnostic, not be
# swallowed the same way the benign "no such key" case (git config exit 5)
# is. A bare `|| true` on the unset would hide this and silently reproduce
# the same "Duplicate header" 400, with no trace of the real cause.
# ---------------------------------------------------------------------------
test_1540_unset_all_real_failure_is_surfaced_and_halts() {
  local tmpdir

  tmpdir=$(mktemp -d)

  # git stub: unset-all fails with a non-5 exit code (e.g. a config-file
  # lock failure). Any git call reached afterward is a sentinel failure —
  # the branching must halt before it.
  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "config" && "$2" == "--local" && "$3" == "--unset-all" ]]; then
  echo "error: could not lock config file .git/config: Permission denied" >&2
  exit 255
fi
echo "::error::git call reached past the unset-all failure: $*" >&2
exit 99
STUB
  chmod +x "$tmpdir/git"

  local script out rc
  script=$(push_with_pat_header_block)
  export FORWARD_MERGE_PAT="fake-pat-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1)
  rc=$?
  unset FORWARD_MERGE_PAT
  rm -rf "$tmpdir"

  if [[ $rc -ne 0 ]] \
      && echo "$out" | grep -q "::error::failed to clear persisted extraheader before PAT push (git config exit 255)" \
      && ! echo "$out" | grep -q "git call reached past"; then
    return 0
  fi
  printf '  rc: %s\n' "$rc" >&2
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540 code review): the benign "no such key" case (git config
# exit 5 — no persisted header at all) must NOT be treated as an error; the
# PAT push proceeds normally.
# ---------------------------------------------------------------------------
test_1540_unset_all_missing_key_is_silently_tolerated() {
  local repo out
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    # Deliberately no persisted extraheader entry.
  )

  out=$(cd "$repo" && FORWARD_MERGE_PAT="fake-pat-token" \
    bash -c "$(push_with_pat_header_block)" 2>&1)
  trash "$repo" 2>/dev/null || true

  if echo "$out" | grep -q "^AUTHORIZATION: basic" && ! echo "$out" | grep -q "::error::"; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Drift guard (Issue #1540 code review): push_with_pat_header_block above is
# a hand-maintained mirror of push_with_pat() in forward-merge-release.yml,
# not an extraction — if production's fix-relevant lines change without this
# mirror being updated, the tests above would keep passing against stale
# logic and silently stop covering the real workflow. Assert the exact fixed
# lines are still present in production so a future edit there fails loudly
# here instead of drifting unnoticed.
# ---------------------------------------------------------------------------
test_1540_test_mirror_matches_production_push_with_pat() {
  local workflow_file
  workflow_file="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/forward-merge-release.yml"

  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  if grep -qF 'git config --local --unset-all http.https://github.com/.extraheader 2>&1' "$workflow_file" \
      && grep -qF "git config --local --name-only --get-regexp '^includeIf\\.gitdir:'" "$workflow_file" \
      && grep -qF 'git -c http.https://github.com/.extraheader="AUTHORIZATION: basic ${auth}" push "$@"' "$workflow_file"; then
    return 0
  fi
  echo "  production's push_with_pat() no longer matches the lines this file mirrors — update push_with_pat_header_block and push_with_pat_block above" >&2
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

run_test "Issue #1380: cascade chains 3.x -> 4.x -> main instead of fanning out" \
  test_1380_cascade_chains_through_each_target

run_test "Issue #1381: version-only manifest conflict keeps the target's version" \
  test_1381_version_only_conflict_keeps_target_version

run_test "Issue #1381: a manifest change beyond the version bails instead of dropping it" \
  test_1381_non_version_change_bails

run_test "Issue #1525: a git diff failure inside auto_resolve_conflicts fails loudly" \
  test_1525_auto_resolve_conflicts_fails_on_git_diff_failure

run_test "Issue #1525: a sync-changelog-doc.sh failure in the changelog branch bails" \
  test_1525_changelog_regeneration_failure_bails

run_test "Issue #1504: push_with_pat authenticates with the PAT when the secret is set" \
  test_1504_push_with_pat_uses_pat_when_set

run_test "Issue #1504: push_with_pat falls back to a plain push without the secret" \
  test_1504_push_with_pat_falls_back_without_secret

run_test "Issue #1504: direct push never attempted once FORWARD_MERGE_PAT is set (bypass risk)" \
  test_1504_direct_push_never_attempted_with_pat_set

run_test "Issue #1504: direct push still attempted normally without the secret" \
  test_1504_direct_push_still_attempted_without_pat

run_test "Issue #1532: identical scripts/sync-changelog-doc.sh runs the target's pinned copy" \
  test_1532_identical_script_runs_target_copy

run_test "Issue #1532: a differing scripts/sync-changelog-doc.sh bails instead of running either copy" \
  test_1532_differing_script_bails

run_test "Issue #1532: FORWARD_MERGE_PAT is not inherited by a spawned subprocess" \
  test_1532_forward_merge_pat_not_inherited_by_subprocess

run_test "Issue #1532: the real sync-changelog-doc.sh regenerates changelog.md through the pinned-copy path" \
  test_1532_real_script_regenerates_changelog

run_test "Issue #1540: push_with_pat clears the persisted extraheader before setting its own" \
  test_1540_push_with_pat_clears_persisted_header_before_setting_new

run_test "Issue #1540 follow-up: push_with_pat clears an includeIf-referenced persisted credential" \
  test_1540_push_with_pat_clears_includeif_persisted_credential

run_test "Issue #1540: a real config-write failure while clearing the header is surfaced and halts" \
  test_1540_unset_all_real_failure_is_surfaced_and_halts

run_test "Issue #1540: the benign no-such-key case is silently tolerated" \
  test_1540_unset_all_missing_key_is_silently_tolerated

run_test "Issue #1540: the test mirror still matches production's push_with_pat()" \
  test_1540_test_mirror_matches_production_push_with_pat

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
