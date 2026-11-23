#!/usr/bin/env bash
# Tests for forward-merge-release.yml.
# Exercises shell logic extracted from the workflow; stubs git and gh.
# Run: bash .github/workflows/__tests__/forward-merge-release-guards.sh
# Extracted blocks run under `set -euo pipefail`, matching the real workflow
# (see Issue #1250): without it, a failing push/fetch/ls-remote captured via
# `VAR=$(cmd); RC=$?` kills the script before RC is ever read, and these
# tests wouldn't catch it.
set -uo pipefail

WORKFLOWS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$WORKFLOWS_DIR/../.." && pwd)"

# Several tests run blocks extracted from the workflow. A broken extraction
# can run real cascade code, so run every test outside any git repo and with
# no GitHub credentials: a stray `git push` or `gh` call then fails (#2299).
HARNESS_SCRATCH="$(mktemp -d)"
if [[ -z "$HARNESS_SCRATCH" || ! -d "$HARNESS_SCRATCH" ]]; then
  echo "harness: mktemp -d failed; cannot isolate the tests" >&2
  exit 1
fi
trap 'trash "$HARNESS_SCRATCH" 2>/dev/null || true' EXIT
mkdir -p "$HARNESS_SCRATCH/gh-config" "$HARNESS_SCRATCH/bin" || exit 1
export GH_CONFIG_DIR="$HARNESS_SCRATCH/gh-config"
unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
# A gh wrapper can read a token from the OS keychain, so block every bare gh
# call. A test that needs gh puts its own stub earlier on PATH.
cat > "$HARNESS_SCRATCH/bin/gh" <<'STUB'
#!/usr/bin/env bash
echo "harness: real gh blocked: gh $*" >&2
exit 97
STUB
chmod +x "$HARNESS_SCRATCH/bin/gh" || exit 1
export PATH="$HARNESS_SCRATCH/bin:$PATH"
cd "$HARNESS_SCRATCH" || exit 1

PASS=0
FAIL=0

# Extracts one shell function from the workflow by name, so a test runs the
# shipped code instead of a copy (#2796, #2798). A missing function prints a
# script that exits 97 with a message, so an empty extraction cannot pass a test.
workflow_function() {
  local out
  out=$(awk -v name="$1" '
    $0 ~ ("^ *" name "\\(\\) \\{$") { indent = $0; sub(/[^ ].*$/, "", indent); found = 1 }
    found { print }
    found && $0 == indent "}" { exit }
  ' "$WORKFLOW")
  if [[ -z "$out" ]]; then
    echo "workflow_function: $1() not found in $WORKFLOW" >&2
    printf '%s\n' "echo 'workflow_function: $1() not found' >&2; exit 97"
    return 1
  fi
  printf '%s\n' "$out"
}

# Runs one helper in a clean environment. Sets RUN_OUT, RUN_ERR, and RUN_RC.
run_helper() {
  local helper="$1"
  local err_file
  err_file=$(mktemp)
  RUN_RC=0
  RUN_OUT=$(env -i PATH="$PATH" "${@:2}" bash -c "$(workflow_function "$helper")
$helper" 2>"$err_file") || RUN_RC=$?
  RUN_ERR=$(<"$err_file")
  rm -f "$err_file"
}


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
    FETCH_STDERR_SAFE="${FETCH_STDERR//$'\n'/ }"
    FETCH_STDERR_SAFE="${FETCH_STDERR_SAFE//::/  }"
    echo "::warning::fetch of $CURRENT $TARGET failed: $FETCH_STDERR_SAFE"
  else
    echo "::warning::fetch of $CURRENT $TARGET failed (exit $FETCH_RC; no stderr)"
  fi
  echo "::endgroup::"
  exit 1
fi
SHELL
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

# Build the auto-resolution script. The manifest, lockfile and conflict functions
# are cut out of forward-merge-release.yml itself, so this tests the shipped code
# and cannot drift from it. Unlike the blocks above this runs against a real git
# repo, because the whole point of the guard is what the source branch did to the
# file in history.
# Callers export: SOURCE_REF TARGET SOURCE_DESC and run it inside a conflicted merge.
WORKFLOW="$WORKFLOWS_DIR/forward-merge-release.yml"

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
    mkdir scripts
    cp "$REPO_ROOT/scripts/forward_merge_manifest.py" scripts/
    git add Cargo.toml scripts
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
  repo=$(make_version_conflict_repo $'serde = { version = "1" }\n')

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
# copy runs with APP_TOKEN/GH_TOKEN in env → SOURCE_SCRIPT_RAN, FAIL.
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
# Test (Issue #1532): APP_TOKEN must not be inherited by any
# subprocess the cascade step spawns (e.g. sync-changelog-doc.sh); only
# push_with_app_token, via a captured local variable, still has access.
#
# Before fix: APP_TOKEN stays in the step's own env for its whole
# duration, so a spawned subprocess inherits it → LEAKED, FAIL.
# After fix: it's captured into a local variable and unset immediately →
# NOT_LEAKED, and push_with_app_token still authenticates via the local var, PASS.
# ---------------------------------------------------------------------------
app_token_scope_block() {
  echo 'set -euo pipefail'
  sed -n '/^ *# #1532, #2796: capture the App token/,/^ *require_app_token$/p' "$WORKFLOW"
  workflow_function push_with_app_token
  cat <<'SHELL'
bash -c 'if [[ -n "${APP_TOKEN:-}" ]]; then echo "LEAKED"; else echo "NOT_LEAKED"; fi'
push_with_app_token origin HEAD:main
SHELL
}

test_1532_app_token_not_inherited_by_subprocess() {
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
  script=$(app_token_scope_block)
  export APP_TOKEN="fake-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1 || true)
  unset APP_TOKEN
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
  real_script="$REPO_ROOT/scripts/sync-changelog-doc.sh"
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
# Build a repo with `target` and `source` branches whose
# scripts/sync-changelog-doc.sh content is given by the caller, with no
# changelog.md at all -- so merging source into target never conflicts.
# Exercises the #1534 gap: an unconditional script-integrity check must
# catch a script change on a clean merge, not only inside a changelog.md
# conflict. A `refs/remotes/origin/target` ref is created manually (no real
# remote) so the check's own `origin/$TARGET` naming can be exercised as-is.
# ---------------------------------------------------------------------------
make_script_only_change_repo() {
  local target_script="$1" source_script="$2" repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir -p scripts
    printf '%s\n' "$target_script" > scripts/sync-changelog-doc.sh
    git add scripts/sync-changelog-doc.sh
    git commit -q -m base

    git switch -q -c source
    printf '%s\n' "$source_script" > scripts/sync-changelog-doc.sh
    git commit -q -am "source change" --allow-empty

    git switch -q target
    git update-ref refs/remotes/origin/target refs/heads/target
  )
  printf '%s\n' "$repo"
}

# Build a repo where the script exists on `source` but not `target` (e.g. a
# script being introduced for the first time by this push). This asymmetric
# case must now HALT -- treating it as a skip (the pre-fix behavior) was the
# exact #1534 bypass, just entered via introduction instead of modification.
make_script_added_only_on_source_repo() {
  local repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    printf 'placeholder\n' > README.md
    git add README.md
    git commit -q -m base

    git switch -q -c source
    mkdir -p scripts
    printf 'echo hi\n' > scripts/sync-changelog-doc.sh
    git add scripts/sync-changelog-doc.sh
    git commit -q -am "add script"

    git switch -q target
    git update-ref refs/remotes/origin/target refs/heads/target
  )
  printf '%s\n' "$repo"
}

# Build a repo where NEITHER branch carries scripts/sync-changelog-doc.sh at
# all (e.g. an older release line predating the script's introduction on
# both sides) -- the guard must still skip cleanly in this, the only
# legitimate no-file case.
make_script_missing_on_both_sides_repo() {
  local repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    printf 'placeholder\n' > README.md
    git add README.md
    git commit -q -m base

    git switch -q -c source
    printf 'source change\n' >> README.md
    git commit -q -am "source change"

    git switch -q target
    git update-ref refs/remotes/origin/target refs/heads/target
  )
  printf '%s\n' "$repo"
}

# Mirrors the per-target script-integrity guard added to
# forward-merge-release.yml's per-target loop (#1534, hardened further by a
# #1543 follow-up review): runs before the merge attempt, for every target,
# regardless of whether this push conflicts on changelog.md at all. Only
# skips when NEITHER side carries the script; an asymmetric case (added or
# removed by this push) halts, since that's exactly as dangerous as a
# same-side content change. Reads use 2>/dev/null, not 2>&1, so stderr never
# folds into the value compared for equality. Callers export SOURCE_REF,
# TARGET, SOURCE_DESC.
script_integrity_guard_block() {
  cat <<'SHELL'
set -euo pipefail
SYNC_SCRIPT_PATH="scripts/sync-changelog-doc.sh"
TARGET_HAS_SCRIPT=0
SOURCE_HAS_SCRIPT=0
git cat-file -e "origin/$TARGET:$SYNC_SCRIPT_PATH" 2>/dev/null && TARGET_HAS_SCRIPT=1
git cat-file -e "$SOURCE_REF:$SYNC_SCRIPT_PATH" 2>/dev/null && SOURCE_HAS_SCRIPT=1
if [[ "$TARGET_HAS_SCRIPT" -eq 1 || "$SOURCE_HAS_SCRIPT" -eq 1 ]]; then
  if [[ "$TARGET_HAS_SCRIPT" -ne "$SOURCE_HAS_SCRIPT" ]]; then
    echo "::error::$SYNC_SCRIPT_PATH exists on only one of $SOURCE_DESC/$TARGET; a script addition or removal must be forward-merged and reviewed by hand, not auto-run"
    exit 1
  fi
  SCRIPT_TARGET=$(git show "origin/$TARGET:$SYNC_SCRIPT_PATH" 2>/dev/null) || {
    echo "::error::failed to read $TARGET's copy of $SYNC_SCRIPT_PATH"
    exit 1
  }
  SCRIPT_SOURCE=$(git show "$SOURCE_REF:$SYNC_SCRIPT_PATH" 2>/dev/null) || {
    echo "::error::failed to read $SOURCE_DESC's copy of $SYNC_SCRIPT_PATH"
    exit 1
  }
  if [[ "$SCRIPT_TARGET" != "$SCRIPT_SOURCE" ]]; then
    echo "::error::$SYNC_SCRIPT_PATH differs between $SOURCE_DESC and $TARGET; a script-logic change must be forward-merged and reviewed by hand, not auto-run"
    exit 1
  fi
fi
echo "PASSED_INTEGRITY_CHECK"
SHELL
}

test_1534_script_only_change_without_conflict_is_caught() {
  local repo out
  repo=$(make_script_only_change_repo 'echo target' 'echo source')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=target SOURCE_DESC=release/4.x \
    bash -c "$(script_integrity_guard_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *"differs between"* ]] && [[ "$out" != *PASSED_INTEGRITY_CHECK* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_1534_identical_script_without_conflict_proceeds() {
  local repo out
  repo=$(make_script_only_change_repo 'echo same' 'echo same')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=target SOURCE_DESC=release/4.x \
    bash -c "$(script_integrity_guard_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *PASSED_INTEGRITY_CHECK* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_1534_script_added_only_on_source_halts() {
  local repo out
  repo=$(make_script_added_only_on_source_repo)

  out=$(cd "$repo" && SOURCE_REF=source TARGET=target SOURCE_DESC=release/4.x \
    bash -c "$(script_integrity_guard_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *"exists on only one of"* ]] && [[ "$out" != *PASSED_INTEGRITY_CHECK* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_1534_script_missing_on_both_sides_skips_check() {
  local repo out
  repo=$(make_script_missing_on_both_sides_repo)

  out=$(cd "$repo" && SOURCE_REF=source TARGET=target SOURCE_DESC=release/4.x \
    bash -c "$(script_integrity_guard_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *PASSED_INTEGRITY_CHECK* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Drift guard: script_integrity_guard_block above is a hand-maintained
# mirror of the per-target loop's script-integrity check in
# forward-merge-release.yml, not an extraction -- assert the exact lines
# are still present in production so a future edit there fails loudly here
# instead of drifting unnoticed.
test_1534_test_mirror_matches_production_script_integrity_guard() {
  local workflow_file
  workflow_file="$WORKFLOWS_DIR/forward-merge-release.yml"

  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  if grep -qF 'git cat-file -e "origin/$TARGET:$SYNC_SCRIPT_PATH" 2>/dev/null && TARGET_HAS_SCRIPT=1' "$workflow_file" \
      && grep -qF 'git cat-file -e "$SOURCE_REF:$SYNC_SCRIPT_PATH" 2>/dev/null && SOURCE_HAS_SCRIPT=1' "$workflow_file" \
      && grep -qF '$SYNC_SCRIPT_PATH exists on only one of $SOURCE_DESC/$TARGET; a script addition or removal must be forward-merged and reviewed by hand, not auto-run' "$workflow_file" \
      && grep -qF '$SYNC_SCRIPT_PATH differs between $SOURCE_DESC and $TARGET; a script-logic change must be forward-merged and reviewed by hand, not auto-run' "$workflow_file"; then
    return 0
  fi
  echo "  production's per-target script-integrity guard no longer matches the lines this file mirrors -- update script_integrity_guard_block above" >&2
  return 1
}

# Build a repo where source and target both bumped the same pin (Renovate on two
# release lines), each moved its own version, and each rewrote both lockfiles.
# `$1` is the clap pin the source moves to; the target is on 1.0.1. The base
# commit carries the manifest script unless `$2` is "no-script". Leaves the repo
# inside a conflicted `git merge source`. Echoes the path.
make_pin_drift_repo() {
  local source_pin="$1" script="${2:-script}" repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir website scripts
    if [[ "$script" == script ]]; then
      cp "$REPO_ROOT/scripts/forward_merge_manifest.py" scripts/
    else
      touch scripts/.keep
    fi
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
[[ -n "${TOOL_OUTPUT:-}" ]] && printf '%s\n' "$TOOL_OUTPUT" >&2
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
# the caller removes the repo. Args: source pin, optional "no-script".
run_drift() {
  local stubs
  DRIFT_REPO=$(make_pin_drift_repo "$@")
  stubs=$(make_tool_stubs)
  DRIFT_OUT=$(cd "$DRIFT_REPO" && PATH="$stubs:$PATH" FAIL_TOOL="${FAIL_TOOL:-}" \
    TOOL_OUTPUT="${TOOL_OUTPUT:-}" \
    SOURCE_REF=source TARGET=release/4.x SOURCE_DESC=release/3.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Tests (Issue #2166): the same pin bumped on two release lines, plus both
# lockfiles rewritten on both lines, resolves with no human step. A real change
# still bails. The manifest check is the target's own copy (#1532), never the
# source's.
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

test_2166_newer_source_pin_bails() {
  run_drift 1.0.2
  trash "$DRIFT_REPO" 2>/dev/null || true
  [[ "$DRIFT_OUT" == *BAILED* ]] && [[ "$DRIFT_OUT" == *"newer than target"* ]] && return 0
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

test_2166_lockfile_tool_failure_bails_naming_command() {
  FAIL_TOOL=npm run_drift 1.0.1
  trash "$DRIFT_REPO" 2>/dev/null || true
  [[ "$DRIFT_OUT" == *BAILED* ]] && [[ "$DRIFT_OUT" == *"npm install --package-lock-only"* ]] \
    && return 0
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

# cargo prints paths from the merged manifests, which a source commit
# controls (#2271).
test_2271_lockfile_tool_output_is_sanitized() {
  TOOL_OUTPUT="warning: path nope ::stop-commands::tok" run_drift 1.0.1
  trash "$DRIFT_REPO" 2>/dev/null || true
  [[ "$DRIFT_OUT" == *RESOLVED* ]] && [[ "$DRIFT_OUT" == *"warning: path nope"* ]] \
    && [[ "$DRIFT_OUT" != *"::stop-commands::"* ]] && return 0
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

test_2166_target_without_manifest_script_bails() {
  run_drift 1.0.1 no-script
  trash "$DRIFT_REPO" 2>/dev/null || true
  [[ "$DRIFT_OUT" == *BAILED* ]] && [[ "$DRIFT_OUT" == *"could not read the manifest-check script"* ]] && return 0
  printf '  out: %s\n' "${DRIFT_OUT//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540): push_with_app_token's App token header must not collide with the
# persisted GITHUB_TOKEN extraheader that actions/checkout's
# persist-credentials: true step already wrote into .git/config.
#
# http.extraheader is a multi-valued git config key: a `-c` override on the
# command line adds a value alongside a persisted one, it does not replace
# it. Verified directly against real git (not a stub) below, since this is a
# property of git's own config layering, not of this workflow's logic — an
# empty `-c key=` override does NOT unset a persisted value either, it adds
# a third, empty entry. Before the fix, invoking push_with_app_token while a
# persisted extraheader value exists leaves BOTH values in effect, and
# GitHub's server rejects the resulting double `Authorization` header with a
# 400 ("Duplicate header"). The fix must remove the persisted local-config
# entry before setting the App token's own header via -c, so exactly one
# Authorization header is in effect.
#
# This mirrors push_with_app_token with its trailing git subcommand parameterized
# (production hardcodes `push`) so the test can inspect the resulting git
# config state via `config --get-all` instead of needing a real remote push.
# ---------------------------------------------------------------------------
# shellcheck disable=SC2016 # literal shell text for the generated script
push_with_app_token_header_block() {
  echo 'set -euo pipefail'
  echo '_app_token="${APP_TOKEN:-}"'
  echo 'unset APP_TOKEN'
  workflow_function push_with_app_token
  cat <<'SHELL'
# Print the headers the push would send instead of pushing. `git -c <header>
# config --get-all` shows the persisted header and the -c header together,
# as the push itself would send them.
git() {
  if [[ "${1:-}" == "-c" && "${3:-}" == "push" ]]; then
    command git -c "$2" config --get-all http.https://github.com/.extraheader
    return
  fi
  command git "$@"
}
push_with_app_token origin forward-merge/test
SHELL
}

test_1540_push_with_app_token_clears_persisted_header_before_setting_new() {
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

  out=$(cd "$repo" && APP_TOKEN="fake-app-token" \
    bash -c "$(push_with_app_token_header_block)" 2>&1)
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
test_1540_push_with_app_token_clears_includeif_persisted_credential() {
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

  out=$(cd "$repo" && APP_TOKEN="fake-app-token" \
    bash -c "$(push_with_app_token_header_block)" 2>&1)
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
# Test (Issue #1540 follow-up code review): the includeIf enumeration itself
# must surface a real failure (e.g. exit 128 outside a git work tree), not
# just the benign "no matches" case (exit 1) that a bare `2>/dev/null ||
# true` would swallow identically.
# ---------------------------------------------------------------------------
test_1540_includeif_enumeration_real_failure_is_surfaced_and_halts() {
  local tmpdir

  tmpdir=$(mktemp -d)

  cat > "$tmpdir/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "config" && "$2" == "--local" && "$3" == "--unset-all" && "$4" == "http.https://github.com/.extraheader" ]]; then
  exit 5
fi
if [[ "$1" == "config" && "$2" == "--local" && "$3" == "--name-only" && "$4" == "--get-regexp" ]]; then
  echo "fatal: --local can only be used inside a git repository" >&2
  exit 128
fi
echo "::error::git call reached past the enumeration failure: $*" >&2
exit 99
STUB
  chmod +x "$tmpdir/git"

  local script out rc
  script=$(push_with_app_token_header_block)
  export APP_TOKEN="fake-app-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1)
  rc=$?
  unset APP_TOKEN
  rm -rf "$tmpdir"

  if [[ $rc -ne 0 ]] \
      && echo "$out" | grep -q "::error::failed to enumerate includeIf.gitdir entries before the App push (git config exit 128)" \
      && ! echo "$out" | grep -q "git call reached past"; then
    return 0
  fi
  printf '  rc: %s\n' "$rc" >&2
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540 follow-up code review): once push_with_app_token removes an
# includeIf entry pointing at a checkout-created credentials file, it must
# also delete that file itself — actions/checkout's own Post Checkout
# cleanup discovers the file path only by reading the includeIf value, so
# once the key is gone, Post Checkout has nothing left to read and would
# otherwise leave the file orphaned on the runner.
# ---------------------------------------------------------------------------
test_1540_includeif_removal_deletes_the_credentials_file() {
  local runner_temp repo credfile out file_survived
  runner_temp=$(mktemp -d)
  repo=$(mktemp -d)
  credfile="$runner_temp/git-credentials-test-uuid.config"
  printf '[http "https://github.com/"]\n\textraheader = AUTHORIZATION: basic PERSISTED_TOKEN\n' \
    > "$credfile"
  (
    cd "$repo" || exit 1
    git init -q .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    git config --local "includeIf.gitdir:$repo/.git.path" "$credfile"
  )

  out=$(cd "$repo" && APP_TOKEN="fake-app-token" RUNNER_TEMP="$runner_temp" \
    bash -c "$(push_with_app_token_header_block)" 2>&1)
  file_survived=0
  [[ -f "$credfile" ]] && file_survived=1
  trash "$repo" 2>/dev/null || true
  trash "$runner_temp" 2>/dev/null || true

  if [[ "$file_survived" -eq 0 ]] && echo "$out" | grep -q '^AUTHORIZATION: basic'; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  file_survived: %s\n' "$file_survived" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540 follow-up code review): a credentials file OUTSIDE
# $RUNNER_TEMP must never be deleted — the match is deliberately narrow so
# push_with_app_token can't be tricked into removing an unrelated file.
# ---------------------------------------------------------------------------
test_1540_includeif_removal_does_not_delete_non_matching_files() {
  local runner_temp repo credfile out file_survived
  runner_temp=$(mktemp -d)
  repo=$(mktemp -d)
  # Deliberately outside $RUNNER_TEMP.
  credfile=$(mktemp)
  printf '[http "https://github.com/"]\n\textraheader = AUTHORIZATION: basic PERSISTED_TOKEN\n' \
    > "$credfile"
  (
    cd "$repo" || exit 1
    git init -q .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    git config --local "includeIf.gitdir:$repo/.git.path" "$credfile"
  )

  out=$(cd "$repo" && APP_TOKEN="fake-app-token" RUNNER_TEMP="$runner_temp" \
    bash -c "$(push_with_app_token_header_block)" 2>&1)
  file_survived=0
  [[ -f "$credfile" ]] && file_survived=1
  trash "$repo" 2>/dev/null || true
  trash "$runner_temp" 2>/dev/null || true
  trash "$credfile" 2>/dev/null || true

  if [[ "$file_survived" -eq 1 ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
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
  script=$(push_with_app_token_header_block)
  export APP_TOKEN="fake-app-token"

  out=$(PATH="$tmpdir:$PATH" bash -c "$script" 2>&1)
  rc=$?
  unset APP_TOKEN
  rm -rf "$tmpdir"

  if [[ $rc -ne 0 ]] \
      && echo "$out" | grep -q "::error::failed to clear persisted extraheader before the App push (git config exit 255)" \
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
# App push proceeds normally.
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

  out=$(cd "$repo" && APP_TOKEN="fake-app-token" \
    bash -c "$(push_with_app_token_header_block)" 2>&1)
  trash "$repo" 2>/dev/null || true

  if echo "$out" | grep -q "^AUTHORIZATION: basic" && ! echo "$out" | grep -q "::error::"; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #1540 follow-up code review): calling push_with_app_token a second
# time in the same job -- after it already dropped the persisted checkout
# credential -- must fail loudly rather than silently push with no
# credential at all. This enforces the single-last-call invariant the
# surrounding comment documents but the code didn't check at runtime.
# ---------------------------------------------------------------------------
test_1540_push_with_app_token_second_call_is_rejected() {
  local repo out rc
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
  )

  local script
  script="$(push_with_app_token_header_block)
push_with_app_token config --get-all http.https://github.com/.extraheader"
  out=$(cd "$repo" && APP_TOKEN="fake-app-token" bash -c "$script" 2>&1)
  rc=$?
  trash "$repo" 2>/dev/null || true

  if [[ $rc -ne 0 ]] \
      && echo "$out" | grep -q "::error::push_with_app_token called more than once"; then
    return 0
  fi
  printf '  rc: %s\n' "$rc" >&2
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #1543: auto_resolve_conflicts' file_display sanitization.
#
# $file inside auto_resolve_conflicts' per-file loop is an arbitrary path
# from the conflicting merge -- an attacker who can land a commit on
# release/** controls it (e.g. a crate directory matching
# `crates/*/Cargo.toml`, or any path falling into the no-auto-resolution
# case). GitHub Actions' `::add-mask::` (used elsewhere in this same job to
# mask the token in push_with_app_token) can be bypassed by an injected
# `::stop-commands::` sequence in log output. Strip any `::` from the copy
# used in the loop's own echo/error lines so they can never be used to
# smuggle one in.
# ---------------------------------------------------------------------------
file_display_sanitize_block() {
  cat <<'SHELL'
file="$1"
file_display="${file//::/  }"
printf '%s\n' "$file_display"
SHELL
}

test_1543_stop_commands_sequence_stripped_from_display() {
  local out
  out=$(bash -c "$(file_display_sanitize_block)" _ 'crates/evil::stop-commands::MARKER/Cargo.toml')

  if [[ "$out" != *"::stop-commands::"* ]] && [[ "$out" == *"crates/evil"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  return 1
}

test_1543_normal_filename_unchanged() {
  local out
  out=$(bash -c "$(file_display_sanitize_block)" _ 'website/docs/changelog.md')

  if [[ "$out" == "website/docs/changelog.md" ]]; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  return 1
}

# Drift guard: file_display_sanitize_block above mirrors the sanitization
# expression added to auto_resolve_conflicts in forward-merge-release.yml,
# and its three call sites (changelog.md, Cargo.toml/lock, and the
# no-auto-resolution fallback) must all use the sanitized copy in their log
# output, not the raw path. manifest_keeps_target declares the same
# expression independently (#2177) -- two declarations total, not one.
test_1543_test_mirror_matches_production_file_display_sanitization() {
  local workflow_file count
  workflow_file="$WORKFLOWS_DIR/forward-merge-release.yml"

  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  count=$(grep -c 'file_display="${file//::/  }"' "$workflow_file")
  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  if [[ "$count" -eq 2 ]] \
      && grep -qF '"  $file_display: regenerating from the merged CHANGELOG-*.md sources"' "$workflow_file" \
      && grep -qF "\"  \$file_display: keeping \$TARGET's own version\"" "$workflow_file" \
      && grep -qF '"::error::$file_display: conflict has no auto-resolution rule"' "$workflow_file"; then
    return 0
  fi
  echo "  production's file_display sanitization no longer matches the lines this file mirrors -- update auto_resolve_conflicts and file_display_sanitize_block" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #1543 follow-up: git's own merge output (not this workflow's echo
# lines) also leaks an attacker-controlled conflicting path. Verified
# against real git: "Auto-merging <path>" and "CONFLICT (content): Merge
# conflict in <path>" both print to STDOUT, unsanitized, even under the
# `2>/dev/null` the trial merge used before this fix -- before
# auto_resolve_conflicts' own file_display sanitization ever runs. Both
# merge invocations must fully suppress their own output (>/dev/null 2>&1),
# since only their exit status is used.
# ---------------------------------------------------------------------------
make_merge_stdout_leak_repo() {
  local repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    printf 'base\n' > 'conflict::stop-commands::marker.txt'
    git add .
    git commit -q -m base

    git switch -q -c source
    printf 'source change\n' > 'conflict::stop-commands::marker.txt'
    git commit -q -am "source"

    git switch -q target
    printf 'target change\n' > 'conflict::stop-commands::marker.txt'
    git commit -q -am "target"
  )
  printf '%s\n' "$repo"
}

test_1543_git_merge_stdout_suppressed_on_conflict() {
  local repo out rc
  repo=$(make_merge_stdout_leak_repo)

  out=$(cd "$repo" && git merge --no-commit --no-ff source >/dev/null 2>&1)
  rc=$?
  (cd "$repo" && git merge --abort 2>/dev/null || true)
  trash "$repo" 2>/dev/null || true

  if [[ -z "$out" ]] && [[ "$rc" -ne 0 ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

# Drift guard: both git-merge invocations in the per-target loop must fully
# suppress their own stdout+stderr, not just stderr -- a bare `2>/dev/null`
# (the pre-fix form) still lets git's own conflict/auto-merge messages
# print an attacker-controlled path to STDOUT unsanitized.
test_1543_test_mirror_matches_production_merge_output_suppression() {
  local workflow_file
  workflow_file="$WORKFLOWS_DIR/forward-merge-release.yml"

  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  if grep -qF 'MERGE_ERR=$(git merge --no-commit --no-ff "$SOURCE_REF" 2>&1 >/dev/null)' "$workflow_file" \
      && grep -qF 'git merge --no-edit "$SOURCE_REF" >/dev/null 2>&1' "$workflow_file"; then
    return 0
  fi
  echo "  production's merge invocations no longer discard their stdout -- update forward-merge-release.yml (the trial merge must use 2>&1 >/dev/null into MERGE_ERR, the real merge >/dev/null 2>&1)" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #1675: "Determine source branch" step — mirrors forward-merge-
# release.yml's step of the same name. A push-triggered run treats the
# pushed branch as the source, same as before. A pull_request-triggered
# retry (a merged forward-merge/<source>-to-<target> PR) has no pushed
# branch to read — the source is parsed out of the PR's own head ref name.
# Callers export EVENT_NAME PUSHED_REF PR_HEAD_REF.
# ---------------------------------------------------------------------------
determine_source_branch_block() {
  cat <<'SHELL'
set -euo pipefail
if [[ "$EVENT_NAME" == "push" ]]; then
  echo "ref=$PUSHED_REF"
else
  rest="${PR_HEAD_REF#forward-merge/}"
  source_branch="${rest%-to-*}"
  if [[ ! "$source_branch" =~ ^(release/[0-9]+\.x|main)$ ]]; then
    head_ref_display="${PR_HEAD_REF//::/  }"
    echo "::error::could not parse a valid source branch out of forward-merge PR head ref '$head_ref_display' (expected forward-merge/<source>-to-<target>, source one of release/X.x or main)"
    exit 1
  fi
  echo "ref=$source_branch"
fi
SHELL
}

test_1675_push_event_uses_pushed_branch_as_source() {
  local out
  out=$(EVENT_NAME="push" PUSHED_REF="release/4.x" PR_HEAD_REF="" \
    bash -c "$(determine_source_branch_block)" 2>&1)

  if [[ "$out" == "ref=release/4.x" ]]; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  return 1
}

test_1675_merged_pr_event_parses_source_from_head_ref() {
  local out
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/release/4.x-to-main" \
    bash -c "$(determine_source_branch_block)" 2>&1)

  if [[ "$out" == "ref=release/4.x" ]]; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  return 1
}

test_1675_merged_pr_event_parses_source_between_two_release_lines() {
  local out
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/release/3.x-to-release/4.x" \
    bash -c "$(determine_source_branch_block)" 2>&1)

  if [[ "$out" == "ref=release/3.x" ]]; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #1675 review follow-up (silent-failure-hunter / security-audit /
# variant-bug-hunter, converging on the same gap): a head ref that passes the
# job-level `startsWith(head.ref, 'forward-merge/')` check but isn't shaped
# forward-merge/<source>-to-<target> must fail loudly, not silently produce
# an empty or wrong ref. An empty `ref:` reaching actions/checkout falls back
# to the triggering event's own ref (the PR's base branch for a closed
# pull_request) with no error at all -- exactly the silent wrong-branch
# checkout these three findings each reproduced independently.
# ---------------------------------------------------------------------------
test_1675_empty_source_after_prefix_strip_fails_loudly() {
  local out rc
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/-to-main" \
    bash -c "$(determine_source_branch_block)" 2>&1)
  rc=$?

  if [[ $rc -ne 0 ]] && echo "$out" | grep -q "::error::could not parse a valid source branch"; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

test_1675_missing_to_separator_fails_loudly() {
  local out rc
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/onlysourcename" \
    bash -c "$(determine_source_branch_block)" 2>&1)
  rc=$?

  if [[ $rc -ne 0 ]] && echo "$out" | grep -q "::error::could not parse a valid source branch"; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

test_1675_multiple_to_separators_fails_loudly() {
  local out rc
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/release/4.x-to-release/5.x-to-main" \
    bash -c "$(determine_source_branch_block)" 2>&1)
  rc=$?

  if [[ $rc -ne 0 ]] && echo "$out" | grep -q "::error::could not parse a valid source branch"; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

test_1675_arbitrary_ref_embedded_in_head_fails_loudly() {
  local out rc
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/refs/pull/999/head-to-x" \
    bash -c "$(determine_source_branch_block)" 2>&1)
  rc=$?

  if [[ $rc -ne 0 ]] && echo "$out" | grep -q "::error::could not parse a valid source branch"; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #1675: the job-level `if:` gate that admits a pull_request retry only
# for a merged PR whose head is one of this workflow's own forward-merge/*
# staging branches. Mirrored here as a bash predicate over the same three
# inputs GitHub Actions' expression evaluates (actor, merged, head ref) — not
# a run of the real expression engine, since that only runs inside GitHub
# Actions itself.
# ---------------------------------------------------------------------------
job_admits_run() {
  local actor="$1" event_name="$2" merged="$3" head_ref="$4" \
    head_repo="${5:-owner/repo}" base_repo="${6:-owner/repo}" base_ref="${7:-main}"
  [[ "$actor" != "github-actions[bot]" ]] || return 1
  [[ "$event_name" == "push" ]] && return 0
  [[ "$merged" == "true" ]] || return 1
  [[ "$head_ref" == forward-merge/* ]] || return 1
  [[ "$head_repo" == "$base_repo" ]] || return 1
  [[ "$base_ref" == release/* || "$base_ref" == "main" ]] || return 1
  return 0
}

test_1675_job_admits_push_from_a_human() {
  job_admits_run "a-human" "push" "" ""
}

test_1675_job_rejects_push_from_the_bot() {
  ! job_admits_run "github-actions[bot]" "push" "" ""
}

test_1675_job_admits_a_merged_forward_merge_pr() {
  job_admits_run "a-human" "pull_request" "true" "forward-merge/release/4.x-to-main"
}

test_1675_job_rejects_an_unmerged_closed_forward_merge_pr() {
  ! job_admits_run "a-human" "pull_request" "false" "forward-merge/release/4.x-to-main"
}

test_1675_job_rejects_a_merged_pr_that_is_not_a_forward_merge_branch() {
  ! job_admits_run "a-human" "pull_request" "true" "some-unrelated-branch"
}

# ---------------------------------------------------------------------------
# Issue #1675 review follow-up (security-audit): branch-name prefix alone is
# not proof of provenance -- a fork PR can name its own head branch anything,
# including a string starting with "forward-merge/". Blast radius is capped
# today by GitHub withholding write tokens/secrets from a `pull_request` run
# on a fork PR regardless, but the job's own `if:` gate should not rely on
# that alone; it must also check the head repo matches this repo, and that
# the PR targeted a real release/main line rather than an arbitrary branch.
# ---------------------------------------------------------------------------
test_1675_job_rejects_a_merged_pr_from_a_fork_with_matching_branch_name() {
  ! job_admits_run "a-human" "pull_request" "true" "forward-merge/release/4.x-to-main" \
    "attacker/repo" "owner/repo" "main"
}

test_1675_job_rejects_a_merged_same_repo_pr_targeting_an_unrelated_branch() {
  ! job_admits_run "a-human" "pull_request" "true" "forward-merge/release/4.x-to-main" \
    "owner/repo" "owner/repo" "some-feature-branch"
}

test_1675_job_admits_a_merged_same_repo_pr_targeting_a_release_branch() {
  job_admits_run "a-human" "pull_request" "true" "forward-merge/release/3.x-to-release/4.x" \
    "owner/repo" "owner/repo" "release/4.x"
}

# Drift guard: determine_source_branch_block and job_admits_run above are
# hand-maintained mirrors of forward-merge-release.yml's "Determine source
# branch" step and job-level `if:` gate, not an extraction — assert the
# exact lines are still present in production so a future edit there fails
# loudly here instead of drifting unnoticed.
test_1675_test_mirror_matches_production_source_branch_retry() {
  local workflow_file
  workflow_file="$WORKFLOWS_DIR/forward-merge-release.yml"

  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  if grep -qF "rest=\"\${PR_HEAD_REF#forward-merge/}\"" "$workflow_file" \
      && grep -qF 'source_branch="${rest%-to-*}"' "$workflow_file" \
      && grep -qF "startsWith(github.event.pull_request.head.ref, 'forward-merge/')" "$workflow_file" \
      && grep -qF 'github.event.pull_request.merged == true' "$workflow_file" \
      && grep -qF 'github.event.pull_request.head.repo.full_name == github.repository' "$workflow_file" \
      && grep -qF "startsWith(github.event.pull_request.base.ref, 'release/')" "$workflow_file" \
      && grep -qF 'if [[ ! "$source_branch" =~ ^(release/[0-9]+\.x|main)$ ]]; then' "$workflow_file" \
      && grep -qF 'head_ref_display="${PR_HEAD_REF//::/  }"' "$workflow_file" \
      && grep -qF "forward-merge PR head ref '\$head_ref_display'" "$workflow_file"; then
    return 0
  fi
  echo "  production's source-branch retry logic no longer matches the lines this file mirrors -- update determine_source_branch_block/job_admits_run above" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #2240: a branch name reaches a workflow-command echo raw at only two
# sites, before the regex check. Every later echo reads a name that the
# detect step's `grep -E '^release/[0-9]+\.x$'` (or the literal `main`)
# already bounded, so a `::` cannot reach it.
# ---------------------------------------------------------------------------

# The target-list block of the cascade step, extracted from production (not
# hand-copied) so it cannot drift. It prints TARGETS, one per line.
target_list_block() {
  echo "set -euo pipefail"
  sed -n '/# Build target list/,/TARGETS+=("main")/p' "$WORKFLOW" | sed 's/^          //'
  cat <<'SHELL'
printf '%s\n' "${TARGETS[@]}"
SHELL
}

test_2240_malformed_head_ref_error_is_sanitized() {
  local out rc
  out=$(EVENT_NAME="pull_request" PUSHED_REF="" \
    PR_HEAD_REF="forward-merge/x::stop-commands::tok-to-main" \
    bash -c "$(determine_source_branch_block)" 2>&1)
  rc=$?

  if [[ $rc -ne 0 ]] && [[ "$out" == *"::error::could not parse a valid source branch"* ]] \
      && [[ "$out" != *"::stop-commands::"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "$out" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

test_2240_unversioned_branch_warning_is_sanitized() {
  local out rc
  out=$(CURRENT="release/x::stop-commands::tok" BRANCHES="release/3.x" \
    bash -c "$(target_list_block)" 2>&1)
  rc=$?

  if [[ $rc -eq 0 ]] && [[ "$out" == *"::warning::"*"is not a versioned release/X.x branch"* ]] \
      && [[ "$out" != *"::stop-commands::"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  rc: %s\n' "$rc" >&2
  return 1
}

# The later echoes stay safe only while two facts hold: the detect step
# filters BRANCHES through the release-line regex, and the cascade stops when
# CURRENT is not one of those branches. The targets are then those branches
# plus `main`.
test_2240_cascade_targets_are_regex_bounded() {
  local stubs branches out
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<'STUB'
#!/usr/bin/env bash
printf '  origin/release/3.x\n  origin/release/x::stop-commands::tok\n  origin/release/4.x\n  origin/feature\n'
STUB
  chmod +x "$stubs/git"
  branches=$(PATH="$stubs:$PATH" bash -c "$(detect_branches_block)" 2>&1)
  trash "$stubs" 2>/dev/null || true
  out=$(CURRENT="release/3.x" BRANCHES="$branches" bash -c "$(target_list_block)" 2>&1)

  if [[ "$branches" == $'release/3.x\nrelease/4.x' ]] && [[ "$out" == $'release/4.x\nmain' ]]; then
    return 0
  fi
  echo "  the detect step no longer bounds branch names to release/X.x, or the target list changed shape -- re-check every ::error::/::warning:: echo of \$CURRENT/\$TARGET/\$SOURCE_DESC/\$MERGE_BRANCH for #2240" >&2
  printf '  branches: %s\n  out: %s\n' "${branches//$'\n'/ | }" "${out//$'\n'/ | }" >&2
  return 1
}

# The detect step's BRANCHES pipeline, extracted from production. It prints
# BRANCHES. Callers put a `git` stub on PATH that prints `git branch -r` output.
detect_branches_block() {
  echo "set -euo pipefail"
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *BRANCHES=\$(git branch -r/,/sort -V)$/p' "$WORKFLOW" | sed 's/^          //'
  # shellcheck disable=SC2016 # literal text for the child shell to expand
  echo 'printf "%s" "$BRANCHES"'
}

# ---------------------------------------------------------------------------
# Issue #2271: auto_resolve_conflicts runs inside `if`, so `set -e` does not
# apply to it. Each failure below must return non-zero on its own.
# ---------------------------------------------------------------------------

# A merge can fail for a reason other than a conflict (unrelated histories,
# a dirty tree). No file is then conflicted, and "nothing to resolve" must not
# read as "resolved".
test_2271_no_conflicted_files_bails() {
  local repo out
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    printf 'x\n' > file
    git add file
    git commit -q -m base
  )
  out=$(cd "$repo" && SOURCE_REF=target TARGET=release/4.x SOURCE_DESC=release/3.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  [[ "$out" == *BAILED* ]] && [[ "$out" == *"no conflicted files"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# forward_merge_manifest.py exits 2 on a TOML file it cannot parse. That is
# "could not check", not "found a real change".
test_2271_manifest_parse_failure_is_could_not_check() {
  local repo out
  repo=$(make_version_conflict_repo '[broken')
  out=$(cd "$repo" && SOURCE_REF=source TARGET=release/5.x SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  [[ "$out" == *BAILED* ]] && [[ "$out" == *"could not check whether keeping"* ]] \
    && [[ "$out" != *"more than version numbers"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# If `checkout --ours` fails, a later `git add` stages the file with its
# conflict markers, which clears the conflict and passes the closing check.
test_2271_manifest_checkout_failure_bails() {
  local repo stubs real_git out
  repo=$(make_version_conflict_repo)
  stubs=$(mktemp -d)
  real_git=$(command -v git)
  cat > "$stubs/git" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == checkout && "\$2" == --ours ]]; then
  exit 1
fi
exec "$real_git" "\$@"
STUB
  chmod +x "$stubs/git"
  out=$(cd "$repo" && PATH="$stubs:$PATH" SOURCE_REF=source TARGET=release/5.x \
    SOURCE_DESC=release/4.x bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" "$stubs" 2>/dev/null || true

  [[ "$out" == *BAILED* ]] && [[ "$out" == *"git checkout --ours failed"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# The new-merge-branch path of the cascade, extracted from production: create
# the branch, push it, open the PR. Prints HALTED. Callers put git and gh
# stubs on PATH and export MERGE_BRANCH TARGET SOURCE_DESC.
new_merge_branch_block() {
  cat <<'SHELL'
set -euo pipefail
HALTED=""
HELD=""
push_with_app_token() { git push "$@"; }
approve_pending_runs() { :; }
enable_auto_merge() { :; }
for _once in once; do
SHELL
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *# Create the branch at HEAD (the merge commit made above)/,/^ *HELD="PR opened for \$TARGET"$/p' \
    "$WORKFLOW"
  cat <<'SHELL'
fi
done
echo "HALTED=${HALTED:-}"
echo "HELD=${HELD:-}"
SHELL
}

test_2271_failed_pr_create_is_not_reported_as_opened() {
  local stubs out
  stubs=$(mktemp -d)
  printf '#!/usr/bin/env bash\nexit 0\n' > "$stubs/git"
  printf '#!/usr/bin/env bash\nexit 1\n' > "$stubs/gh"
  chmod +x "$stubs/git" "$stubs/gh"
  out=$(PATH="$stubs:$PATH" MERGE_BRANCH="forward-merge/release/3.x-to-release/4.x" \
    TARGET=release/4.x SOURCE_DESC=release/3.x bash -c "$(new_merge_branch_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true

  [[ "$out" == *"::error::Failed to create PR for"* ]] \
    && [[ "$out" == *"HALTED=PR creation failed for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2271_ls_remote_failure_without_stderr_says_so() {
  local stubs out
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<'STUB'
#!/usr/bin/env bash
[[ "$1" == ls-remote ]] && exit 128
exit 0
STUB
  chmod +x "$stubs/git"
  out=$(PATH="$stubs:$PATH" MERGE_BRANCH="forward-merge/release/3.x-to-release/4.x" \
    TARGET=release/4.x bash -c "$(ls_remote_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true

  [[ "$out" == *"::error::ls-remote for forward-merge/release/3.x-to-release/4.x failed (exit 128; no stderr)"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #2273: approve_pending_runs, extracted from production. `sleep` is a
# no-op. A PATH `gh` stub answers the run list from $STUB_DIR/runs.<round>
# (empty when the file is missing) and records each approval.
# ---------------------------------------------------------------------------
approve_block() {
  cat <<'SHELL'
set -euo pipefail
GITHUB_REPOSITORY=owner/repo
sleep() { :; }
SHELL
  sed -n '/^ *approve_pending_runs() {$/,/^          }$/p' "$WORKFLOW"
}

make_approve_stubs() {
  local dir
  dir=$(mktemp -d)
  # APPROVE_FAIL makes every approval fail, APPROVE_FAIL_ONCE only the first.
  # LIST_FAIL makes every list fail, LIST_FAIL_ROUND only that round.
  cat > "$dir/gh" <<'STUB'
#!/usr/bin/env bash
if [[ "$1 $2 $3" == "api -X POST" ]]; then
  if [[ -n "${APPROVE_FAIL:-}" ]] \
      || { [[ -n "${APPROVE_FAIL_ONCE:-}" ]] && [[ ! -f "$STUB_DIR/failed_once" ]]; }; then
    touch "$STUB_DIR/failed_once"
    echo "HTTP 403 ::stop-commands::tok" >&2
    exit 1
  fi
  echo "$4" >> "$STUB_DIR/approved"
  exit 0
fi
echo "$*" >> "$STUB_DIR/listed"
n=$(( $(cat "$STUB_DIR/round" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$STUB_DIR/round"
if [[ -n "${LIST_FAIL:-}" || "${LIST_FAIL_ROUND:-}" == "$n" ]]; then
  echo "HTTP 401 Bad credentials ::stop-commands::tok" >&2
  exit 1
fi
if [[ -f "$STUB_DIR/runs.$n" ]]; then cat "$STUB_DIR/runs.$n"; else echo '{"workflow_runs":[]}'; fi
STUB
  chmod +x "$dir/gh"
  printf '%s\n' "$dir"
}

SHA40=0123456789abcdef0123456789abcdef01234567
MB="forward-merge/release/3.x-to-release/4.x"

# One run of each kind: ours, a fork's, another branch's, and one not gated.
runs_json() {
  cat <<EOF
{"workflow_runs":[
 {"id":11,"conclusion":"action_required","head_branch":"$MB","head_repository":{"full_name":"owner/repo"}},
 {"id":12,"conclusion":"action_required","head_branch":"$MB","head_repository":{"full_name":"evil/fork"}},
 {"id":13,"conclusion":"action_required","head_branch":"other","head_repository":{"full_name":"owner/repo"}},
 {"id":14,"conclusion":null,"head_branch":"$MB","head_repository":{"full_name":"owner/repo"}}
]}
EOF
}

run_approve() {
  local dir="$1" sha="$2"
  PATH="$dir:$PATH" STUB_DIR="$dir" bash -c "$(approve_block)
approve_pending_runs '$sha' '$MB'" 2>&1 || true
}

test_2273_approves_only_own_runs_on_the_merge_branch() {
  local dir out approved
  dir=$(make_approve_stubs)
  runs_json > "$dir/runs.1"
  out=$(run_approve "$dir" "$SHA40")
  approved=$(cat "$dir/approved" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ "$approved" == "repos/owner/repo/actions/runs/11/approve" ]] && return 0
  printf '  approved: %s\n  out: %s\n' "${approved//$'\n'/ | }" "${out//$'\n'/ | }" >&2
  return 1
}

test_2273_approves_a_run_gated_after_the_first_hit() {
  local dir out approved
  dir=$(make_approve_stubs)
  runs_json > "$dir/runs.1"
  runs_json | sed 's/"id":13,"conclusion":"action_required","head_branch":"other"/"id":15,"conclusion":"action_required","head_branch":"'"${MB//\//\\/}"'"/' \
    > "$dir/runs.3"
  out=$(run_approve "$dir" "$SHA40")
  approved=$(sort "$dir/approved" 2>/dev/null | tr '\n' ' ')
  trash "$dir" 2>/dev/null || true
  [[ "$approved" == "repos/owner/repo/actions/runs/11/approve repos/owner/repo/actions/runs/15/approve " ]] \
    && return 0
  printf '  approved: %s\n  out: %s\n' "$approved" "${out//$'\n'/ | }" >&2
  return 1
}

test_2273_list_failure_is_a_sanitized_warning() {
  local dir out
  dir=$(make_approve_stubs)
  out=$(LIST_FAIL=1 run_approve "$dir" "$SHA40")
  trash "$dir" 2>/dev/null || true
  [[ "$out" == *"::warning::"*"HTTP 401 Bad credentials"* ]] && [[ "$out" != *"::stop-commands::"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2273_approve_failure_is_a_sanitized_warning() {
  local dir out
  dir=$(make_approve_stubs)
  runs_json > "$dir/runs.1"
  out=$(APPROVE_FAIL=1 run_approve "$dir" "$SHA40")
  trash "$dir" 2>/dev/null || true
  [[ "$out" == *"::warning::failed to approve run 11"*"HTTP 403"* ]] && [[ "$out" != *"::stop-commands::"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# The server filters by status and branch and every page is read, so a flood
# of fork runs on the same SHA cannot push this repo's run off page one.
test_2273_list_is_filtered_and_paginated() {
  local dir out listed
  dir=$(make_approve_stubs)
  out=$(run_approve "$dir" "$SHA40")
  listed=$(head -1 "$dir/listed" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ "$listed" == *"--paginate"* ]] && [[ "$listed" == *"head_sha=$SHA40"* ]] \
    && [[ "$listed" == *"status=action_required"* ]] && [[ "$listed" == *"branch=$MB"* ]] \
    && [[ "$listed" == *"per_page=100"* ]] && return 0
  printf '  listed: %s\n  out: %s\n' "$listed" "${out//$'\n'/ | }" >&2
  return 1
}

test_2273_failed_approval_is_retried() {
  local dir out approved
  dir=$(make_approve_stubs)
  runs_json > "$dir/runs.1"
  runs_json > "$dir/runs.2"
  out=$(APPROVE_FAIL_ONCE=1 run_approve "$dir" "$SHA40")
  approved=$(cat "$dir/approved" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ "$approved" == "repos/owner/repo/actions/runs/11/approve" ]] && return 0
  printf '  approved: %s\n  out: %s\n' "$approved" "${out//$'\n'/ | }" >&2
  return 1
}

# One failed round after a good one is not "could not list runs".
test_2273_one_failed_round_is_not_a_list_warning() {
  local dir out
  dir=$(make_approve_stubs)
  runs_json > "$dir/runs.1"
  out=$(LIST_FAIL_ROUND=6 run_approve "$dir" "$SHA40")
  trash "$dir" 2>/dev/null || true
  [[ "$out" != *"could not list runs"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Runs with missing or null fields must neither crash the filter nor match.
test_2273_filter_skips_runs_with_missing_fields() {
  local dir out approved
  dir=$(make_approve_stubs)
  cat > "$dir/runs.1" <<EOF
{"workflow_runs":[
 {"id":21,"conclusion":"action_required","head_branch":"$MB","head_repository":null},
 {"id":22,"conclusion":"action_required","head_branch":"$MB"},
 {"id":23,"conclusion":"action_required"},
 {"id":24,"conclusion":"action_required","head_branch":"$MB","head_repository":{"full_name":"owner/repo"}}
]}
EOF
  out=$(run_approve "$dir" "$SHA40")
  approved=$(cat "$dir/approved" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ "$approved" == "repos/owner/repo/actions/runs/24/approve" ]] && [[ "$out" != *"::warning::"* ]] \
    && return 0
  printf '  approved: %s\n  out: %s\n' "$approved" "${out//$'\n'/ | }" >&2
  return 1
}

# The SHA must be exactly 40 lowercase hex digits.
test_2273_sha_shape_boundaries() {
  local sha dir listed bad=""
  for sha in "${SHA40:0:39}" "${SHA40}0" "${SHA40^^}" "${SHA40:0:39}g" " $SHA40"; do
    dir=$(make_approve_stubs)
    run_approve "$dir" "$sha" >/dev/null
    listed=$(cat "$dir/listed" 2>/dev/null)
    trash "$dir" 2>/dev/null || true
    [[ -z "$listed" ]] || bad+=" [$sha]"
  done
  dir=$(make_approve_stubs)
  run_approve "$dir" "$SHA40" >/dev/null
  listed=$(cat "$dir/listed" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ -n "$listed" ]] || bad+=" [valid SHA made no call]"
  [[ -z "$bad" ]] && return 0
  echo "  wrong result for:$bad" >&2
  return 1
}

test_2273_nothing_pending_never_approves() {
  local dir out
  dir=$(make_approve_stubs)
  out=$(run_approve "$dir" "$SHA40")
  local approved
  approved=$(cat "$dir/approved" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ -z "$approved" ]] && [[ "$out" == *"No run for $SHA40 waited for approval"* ]] \
    && [[ "$out" != *"::warning::"* ]] && return 0
  printf '  approved: %s\n  out: %s\n' "$approved" "${out//$'\n'/ | }" >&2
  return 1
}

# A failed approval must never halt the cascade (#1564).
test_2273_approve_failure_does_not_halt() {
  local dir out
  dir=$(make_approve_stubs)
  runs_json > "$dir/runs.1"
  out=$(PATH="$dir:$PATH" STUB_DIR="$dir" APPROVE_FAIL=1 bash -c "$(approve_block)
approve_pending_runs '$SHA40' '$MB'
echo reached-end" 2>&1)
  local rc=$?
  trash "$dir" 2>/dev/null || true
  [[ $rc -eq 0 ]] && [[ "$out" == *"reached-end"* ]] && return 0
  printf '  rc: %s\n  out: %s\n' "$rc" "${out//$'\n'/ | }" >&2
  return 1
}

test_2273_invalid_sha_makes_no_api_call() {
  local dir out listed
  dir=$(make_approve_stubs)
  out=$(run_approve "$dir" "")
  listed=$(cat "$dir/listed" 2>/dev/null)
  trash "$dir" 2>/dev/null || true
  [[ -z "$listed" ]] && [[ "$out" == *"::warning::"*"no valid commit SHA"* ]] && return 0
  printf '  listed: %s\n  out: %s\n' "$listed" "${out//$'\n'/ | }" >&2
  return 1
}

# A test that runs a broken extraction can run real production code. Outside
# any repo and with no GitHub credentials, a stray git or gh call fails (#2299).
test_2299_harness_is_isolated() {
  if git rev-parse --git-dir >/dev/null 2>&1; then
    echo "  the harness cwd ($PWD) is inside a git repo" >&2
    return 1
  fi
  if [[ -n "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ]]; then
    echo "  GH_TOKEN or GITHUB_TOKEN is set in the harness" >&2
    return 1
  fi
  if [[ "${GH_CONFIG_DIR:-}" != "$HARNESS_SCRATCH"/* ]]; then
    echo "  GH_CONFIG_DIR does not point into the harness scratch dir" >&2
    return 1
  fi
  # A gh wrapper can read a token from the OS keychain, so an empty config
  # alone does not stop it. The first gh on PATH must be the blocking stub.
  local rc=0
  gh auth token >/dev/null 2>&1 || rc=$?
  if [[ $rc -ne 97 ]]; then
    echo "  a bare gh call is not blocked (exit $rc, expected 97)" >&2
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Issue #2294: when a forward-merge PR merges, its pull_request retry and the
# push cascade from the PR's base start together. The retry must stop at its
# own target once that target already holds the source, or both chain on and
# race on the next merge branch.
# ---------------------------------------------------------------------------

# The target list plus the retry-target check, extracted from production. It
# prints TARGETS, one per line.
retry_targets_block() {
  echo "set -euo pipefail"
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/# Build target list/,/^ *echo "::error::retry target/p' "$WORKFLOW" | sed 's/^          //'
  cat <<'SHELL'
exit 1
fi
fi
printf '%s\n' "${TARGETS[@]}"
SHELL
}

test_2294_valid_retry_keeps_every_target() {
  local out
  out=$(CURRENT=release/3.x BRANCHES=$'release/3.x\nrelease/4.x' RETRY_TARGET=release/4.x \
    RETRY_HEAD=forward-merge/release/3.x-to-release/4.x bash -c "$(retry_targets_block)" 2>&1)
  [[ "$out" == $'release/4.x\nmain' ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2294_retry_target_not_downstream_fails() {
  local out rc
  out=$(CURRENT=release/4.x BRANCHES=$'release/3.x\nrelease/4.x' \
    RETRY_TARGET="release/3.x::stop-commands::tok" \
    RETRY_HEAD="forward-merge/release/4.x-to-release/3.x::stop-commands::tok" \
    bash -c "$(retry_targets_block)" 2>&1)
  rc=$?
  [[ $rc -ne 0 ]] && [[ "$out" == *"::error::retry target"* ]] \
    && [[ "$out" != *"::stop-commands::"* ]] && return 0
  printf '  out: %s\n  rc: %s\n' "${out//$'\n'/ | }" "$rc" >&2
  return 1
}

# A writer can change a forward-merge PR's base after it opens. The base must
# still be the target that the head branch names.
test_2294_retry_base_must_match_head_target() {
  local out rc
  out=$(CURRENT=release/3.x BRANCHES=$'release/3.x\nrelease/4.x' RETRY_TARGET=main \
    RETRY_HEAD=forward-merge/release/3.x-to-release/4.x bash -c "$(retry_targets_block)" 2>&1)
  rc=$?
  [[ $rc -ne 0 ]] && [[ "$out" == *"::error::retry target main does not match"* ]] && return 0
  printf '  out: %s\n  rc: %s\n' "${out//$'\n'/ | }" "$rc" >&2
  return 1
}

# The race case: the retry finds its own target already holding the source.
test_2294_retry_stops_at_its_up_to_date_target() {
  local stubs out
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  rev-parse) echo deadbeef ;;
  merge-base) [[ "$3" == origin/release/3.x && "$4" == origin/release/4.x ]] && exit 0; exit 1 ;;
esac
exit 0
STUB
  chmod +x "$stubs/git"
  out=$(PATH="$stubs:$PATH" RETRY_TARGET=release/4.x bash -c "$(up_to_date_skip_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true
  [[ "$out" == *"Retry reached its own target release/4.x"* ]] && [[ "$out" != *"INTO:main"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #2280: a failed rev-parse or merge-base read as a normal answer.
# ---------------------------------------------------------------------------

# The per-target ref checks, extracted from production through the
# ancestry-failure arm. Prints HALTED, or NEXT when the loop would merge.
target_check_block() {
  cat <<'SHELL'
set -euo pipefail
HALTED=""
HELD=""
SOURCE_REF="origin/release/3.x"
SOURCE_DESC="release/3.x"
for TARGET in release/4.x; do
SHELL
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *# #2280: an unreadable target ref is a failure/,/^ *HALTED="ancestry check failed at \$TARGET"$/p' "$WORKFLOW"
  cat <<'SHELL'
break
fi
echo NEXT
done
echo "HALTED=${HALTED:-}"
echo "HELD=${HELD:-}"
SHELL
}

# Args: rev-parse exit code, merge-base exit code. Echoes the stub dir.
target_check_stubs() {
  local dir
  dir=$(mktemp -d)
  cat > "$dir/git" <<STUB
#!/usr/bin/env bash
case "\$1" in
  rev-parse) [[ $1 -eq 0 ]] && echo deadbeef; exit $1 ;;
  merge-base) exit $2 ;;
esac
exit 0
STUB
  chmod +x "$dir/git"
  printf '%s\n' "$dir"
}

test_2280_unreadable_target_ref_halts() {
  local dir out
  dir=$(target_check_stubs 128 1)
  out=$(PATH="$dir:$PATH" bash -c "$(target_check_block)" 2>&1 || true)
  trash "$dir" 2>/dev/null || true
  [[ "$out" == *"::error::cannot read origin/release/4.x"* ]] && [[ "$out" != *NEXT* ]] \
    && [[ "$out" == *"HALTED=target ref unreadable at release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2280_ancestry_check_failure_halts() {
  local dir out
  dir=$(target_check_stubs 0 128)
  out=$(PATH="$dir:$PATH" bash -c "$(target_check_block)" 2>&1 || true)
  trash "$dir" 2>/dev/null || true
  [[ "$out" == *"::error::git merge-base --is-ancestor failed (exit 128)"* ]] && [[ "$out" != *NEXT* ]] \
    && [[ "$out" == *"HALTED=ancestry check failed at release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2280_not_an_ancestor_still_merges() {
  local dir out
  dir=$(target_check_stubs 0 1)
  out=$(PATH="$dir:$PATH" bash -c "$(target_check_block)" 2>&1 || true)
  trash "$dir" 2>/dev/null || true
  [[ "$out" == *NEXT* ]] && [[ "$out" == *"HALTED="* ]] && [[ "$out" != *"::error::"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# The per-target "already contains" check of the cascade loop, extracted from
# production. After it, the block prints which ref the next merge would use.
up_to_date_skip_block() {
  cat <<'SHELL'
set -euo pipefail
SOURCE_REF="origin/release/3.x"
SOURCE_DESC="release/3.x"
for TARGET in release/4.x main; do
SHELL
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *# #2280: an unreadable target ref is a failure/,/^ *continue$/p' "$WORKFLOW"
  cat <<'SHELL'
fi
echo "MERGE_FROM:$SOURCE_REF DESC:$SOURCE_DESC INTO:$TARGET"
done
SHELL
}

# release/4.x already holds release/3.x (its forward-merge PR merged), but main
# does not hold release/4.x yet. main must be merged from release/4.x, not
# from release/3.x directly (#2285).
test_2285_up_to_date_target_advances_the_chain() {
  local stubs out
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  rev-parse) echo deadbeef ;;
  merge-base) [[ "$3" == origin/release/3.x && "$4" == origin/release/4.x ]] && exit 0; exit 1 ;;
esac
exit 0
STUB
  chmod +x "$stubs/git"
  out=$(PATH="$stubs:$PATH" bash -c "$(up_to_date_skip_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true

  [[ "$out" == *"MERGE_FROM:origin/release/4.x DESC:release/4.x INTO:main"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# The trial merge of the cascade, extracted from production through its
# "no merge in progress" arm. Prints HALTED.
trial_merge_block() {
  cat <<'SHELL'
set -euo pipefail
HALTED=""
HELD=""
for _once in once; do
SHELL
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *if ! MERGE_ERR=\$(git merge --no-commit --no-ff/,/^ *HALTED="merge failed at \$TARGET"$/p' \
    "$WORKFLOW"
  cat <<'SHELL'
break
fi
fi
done
echo "HALTED=${HALTED:-}"
echo "HELD=${HELD:-}"
SHELL
}

# Unrelated histories make git refuse the merge before it writes MERGE_HEAD.
# Then `git merge --abort` fails too, and the run must not report a
# corrupt tree.
test_2271_merge_without_merge_head_names_git_reason() {
  local repo out
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    printf 'a\n' > a
    git add a
    git commit -q -m target
    git switch -q --orphan source
    printf 'b\n' > b
    git add b
    git commit -q -m source
    git switch -q target
  )
  out=$(cd "$repo" && SOURCE_REF=source TARGET=release/4.x SOURCE_DESC=release/3.x \
    bash -c "$(trial_merge_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  [[ "$out" == *"::error::git merge of release/3.x into release/4.x failed with no merge in progress: fatal: refusing to merge unrelated histories"* ]] \
    && [[ "$out" == *"HALTED=merge failed at release/4.x"* ]] \
    && [[ "$out" != *"Failed to abort merge"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2271_stale_branch_merge_without_merge_head_names_git_reason() {
  local stubs out
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  merge) echo "fatal: refusing to merge unrelated histories" >&2; exit 128 ;;
  merge-base) exit 1 ;;
  rev-parse) [[ "$*" == *MERGE_HEAD* ]] && exit 1 ;;
esac
exit 0
STUB
  chmod +x "$stubs/git"
  out=$(PATH="$stubs:$PATH" SOURCE_REF=origin/source TARGET=release/4.x \
    SOURCE_DESC=release/3.x MERGE_BRANCH="forward-merge/release/3.x-to-release/4.x" ls_rc=0 \
    bash -c "$(existing_branch_update_block)
echo \"HALTED=\${HALTED:-}\"; echo \"HELD=\${HELD:-}\"" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true

  [[ "$out" == *"failed with no merge in progress: fatal: refusing to merge unrelated histories"* ]] \
    && [[ "$out" == *"HALTED=merge failed updating forward-merge/release/3.x-to-release/4.x"* ]] \
    && [[ "$out" != *"Failed to abort merge"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Issue #2272: a forward-merge branch can outlive its PR. delete_branch_on_merge
# removes it only after a merge, so a PR closed without a merge leaves it.
# ---------------------------------------------------------------------------

# git stub for the stale-branch arm. `merge-base --is-ancestor` exits with $1:
# 0 means the branch already holds the source, 1 means it needs the merge.
# Echoes the stub directory.
stale_arm_git_stub() {
  local dir
  dir=$(mktemp -d)
  cat > "$dir/git" <<STUB
#!/usr/bin/env bash
case "\$1" in
  merge-base) exit $1 ;;
  push) echo "PUSHED:\$*" ;;
  rev-parse) echo deadbeef ;;
  log) printf '%s' "\${GIT_LOG_OUT:-}" ;;
esac
exit 0
STUB
  # A second guard: if a call ever escapes the gh function, fail loudly.
  printf '#!/usr/bin/env bash\necho "real gh reached from a test: $*" >&2\nexit 97\n' > "$dir/gh"
  chmod +x "$dir/git" "$dir/gh"
  printf '%s\n' "$dir"
}

# Run the stale-branch arm. Args: merge-base exit code. Callers set
# GH_PRS_JSON, GH_FAIL or GH_CREATE_FAIL. Prints the output, then HALTED.
run_stale_arm() {
  local stubs
  stubs=$(stale_arm_git_stub "$1")
  env -u GH_TOKEN -u GITHUB_TOKEN PATH="$stubs:$PATH" SOURCE_REF=origin/release/3.x TARGET=release/4.x \
    SOURCE_DESC=release/3.x MERGE_BRANCH="forward-merge/release/3.x-to-release/4.x" ls_rc=0 \
    bash -c "$(existing_branch_update_block)
echo \"HALTED=\${HALTED:-}\"; echo \"HELD=\${HELD:-}\"" 2>&1 || true
  trash "$stubs" 2>/dev/null || true
}

test_2272_orphan_branch_is_updated_and_gets_a_new_pr() {
  local out
  out=$(GH_PRS_JSON="[]" run_stale_arm 1)
  [[ "$out" == *"PUSHED:"* ]] && [[ "$out" == *"GH_CALL:pr create --base release/4.x"* ]] \
    && [[ "$out" == *"HELD=PR opened for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2272_orphan_branch_already_current_gets_a_new_pr() {
  local out
  out=$(GH_PRS_JSON="[]" run_stale_arm 0)
  [[ "$out" != *"PUSHED:"* ]] && [[ "$out" == *"GH_CALL:pr create --base release/4.x"* ]] \
    && [[ "$out" == *"HELD=PR opened for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2272_open_pr_is_updated_in_place() {
  local out
  local log
  log=$(mktemp)
  out=$(GH_LOG="$log" run_stale_arm 1)
  local calls
  calls=$(cat "$log")
  trash "$log" 2>/dev/null || true
  [[ "$out" == *"PUSHED:"* ]] && [[ "$out" != *"GH_CALL:pr create"* ]] \
    && [[ "$out" == *"HELD=PR already open for release/4.x (branch updated with release/3.x)"* ]] \
    && [[ "$calls" == *"pr list --head forward-merge/release/3.x-to-release/4.x --state open"* ]] \
    && return 0
  printf '  calls: %s\n' "$calls" >&2
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# A fork can open a PR from a branch with the same name. It must not count as
# the cascade's own open PR.
test_2272_fork_pr_with_same_head_name_does_not_count() {
  local out
  out=$(GH_PRS_JSON='[{"number":9,"isCrossRepository":true}]' run_stale_arm 1)
  [[ "$out" == *"GH_CALL:pr create --base release/4.x"* ]] \
    && [[ "$out" == *"HELD=PR opened for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Commits that only the branch holds may come from a PR a reviewer closed on
# purpose, so the new PR names them.
test_2272_new_pr_lists_the_branch_own_commits() {
  local out
  out=$(GH_PRS_JSON="[]" GIT_LOG_OUT="- abc1234 hand-made resolution" run_stale_arm 1)
  [[ "$out" == *"GH_CALL:pr create"*"Review them before you merge:"*"- abc1234 hand-made resolution"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2272_current_branch_with_open_pr_halts_without_push() {
  local out
  out=$(run_stale_arm 0)
  [[ "$out" != *"PUSHED:"* ]] && [[ "$out" != *"GH_CALL:pr create"* ]] \
    && [[ "$out" == *"HELD=PR already open for release/4.x"* ]] \
    && [[ "$out" != *"(branch updated"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2272_unreadable_pr_list_halts() {
  local out
  out=$(GH_PRS_JSON="warning: x" run_stale_arm 1)
  [[ "$out" == *"returned output that is not a PR list"* ]] && [[ "$out" != *"PUSHED:"* ]] \
    && [[ "$out" != *"GH_CALL:pr create"* ]] \
    && [[ "$out" == *"HALTED=PR lookup failed for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2272_failed_pr_create_on_orphan_branch_halts() {
  local out
  out=$(GH_PRS_JSON="[]" GH_CREATE_FAIL=1 run_stale_arm 1)
  [[ "$out" == *"::error::Failed to create PR for forward-merge/release/3.x-to-release/4.x"* ]] \
    && [[ "$out" == *"HALTED=PR creation failed for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2272_pr_lookup_failure_halts() {
  local out
  out=$(GH_FAIL=1 run_stale_arm 1)
  [[ "$out" == *"::error::gh pr list for forward-merge/release/3.x-to-release/4.x failed: HTTP 502"* ]] \
    && [[ "$out" != *"::stop-commands::"* ]] && [[ "$out" != *"PUSHED:"* ]] \
    && [[ "$out" == *"HALTED=PR lookup failed for release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# The merge-branch lookup of the cascade, extracted from production through
# its failure arm. Prints HALTED.
ls_remote_block() {
  cat <<'SHELL'
set -euo pipefail
HALTED=""
HELD=""
for _once in once; do
SHELL
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *if LS_STDERR=\$(git ls-remote/,/^ *HALTED="ls-remote failed at \$TARGET"$/p' "$WORKFLOW"
  cat <<'SHELL'
break
fi
done
echo "HALTED=${HALTED:-}"
echo "HELD=${HELD:-}"
SHELL
}

test_2271_ls_remote_failure_prints_sanitized_stderr() {
  local stubs out
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == ls-remote ]]; then
  echo "fatal: Authentication failed ::stop-commands::tok" >&2
  exit 128
fi
exit 0
STUB
  chmod +x "$stubs/git"
  out=$(PATH="$stubs:$PATH" MERGE_BRANCH="forward-merge/release/3.x-to-release/4.x" \
    TARGET=release/4.x bash -c "$(ls_remote_block)" 2>&1 || true)
  trash "$stubs" 2>/dev/null || true

  [[ "$out" == *"::error::ls-remote for forward-merge/release/3.x-to-release/4.x failed (exit 128): fatal: Authentication failed"* ]] \
    && [[ "$out" != *"::stop-commands::"* ]] \
    && [[ "$out" == *"HALTED=ls-remote failed at release/4.x"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Build the "update an existing stale merge branch" block: what runs when
# $MERGE_BRANCH already exists. Extracted
# from production (not hand-copied) so the test can't drift from the real
# code. push_with_app_token/approve_pending_runs are stubbed (their real bodies
# depend on GitHub-credential/API specifics irrelevant to this control flow);
# bail_on_conflict/auto_resolve_conflicts are the real production functions.
# Wrapped in a one-iteration for-loop because production's `break` is only
# valid inside a loop.
# Callers export: SOURCE_REF TARGET SOURCE_DESC MERGE_BRANCH, set ls_rc=0,
# and provide git/gh on PATH; run from a checkout of $TARGET.
existing_branch_update_block() {
  echo 'set -euo pipefail'
  cat <<'SHELL'
push_with_app_token() { git push "$@"; }
approve_pending_runs() { :; }
enable_auto_merge() { :; }
# A function shadows any real gh on PATH, so a test never reaches GitHub.
# GH_PRS_JSON is the JSON that `gh pr list` prints (default: one same-repo
# open PR). GH_FAIL makes `pr list` fail, GH_CREATE_FAIL makes `pr create`
# fail. Every call is logged to GH_LOG when it is set.
gh() {
  [[ -n "${GH_LOG:-}" ]] && echo "$*" >> "$GH_LOG"
  if [[ "$1 $2" == "pr list" ]]; then
    [[ -n "${GH_FAIL:-}" ]] && { echo "HTTP 502 ::stop-commands::tok" >&2; return 1; }
    local default_prs='[{"number":7,"isCrossRepository":false}]'
    printf '%s\n' "${GH_PRS_JSON-$default_prs}"
  else
    echo "GH_CALL:$*"
    [[ "$1 $2" == "pr create" && -n "${GH_CREATE_FAIL:-}" ]] && return 1
    return 0
  fi
}
SHELL
  sed -n '/^ *bail_on_conflict() {/,/^ *for TARGET in/p' "$WORKFLOW" | sed '$d'
  echo 'for _once in once; do'
  # shellcheck disable=SC2016 # literal sed address, not an expression to expand
  sed -n '/^ *if \[\[ \$ls_rc -eq 0 \]\]; then$/,/^ *elif \[\[ \$ls_rc -ne 2 \]\]; then$/p' "$WORKFLOW" | sed '$d'
  echo 'fi'
  echo 'done'
}

# Build a bare "origin" + working clone. Branch layout:
#   target                         - base content
#   source                         - base, plus an older commit and (only
#                                    after the merge branch below is created)
#                                    a newer commit -- the newer one is what
#                                    the update-existing-branch block must
#                                    pick up.
#   forward-merge/rel-to-target    - an "existing" (possibly stale) merge of
#                                    source's OLDER commit into target. When
#                                    $1=conflict, this branch also carries a
#                                    human's own hand-edit that the new
#                                    source commit will conflict with.
# Leaves the working clone checked out on `target`. Echoes: working dir,
# then the bare origin dir (space-separated on one line).
make_stale_merge_branch_repo() {
  local mode="${1:-clean}" origin_dir work_dir
  origin_dir=$(mktemp -d)
  git init -q --bare "$origin_dir"
  work_dir=$(mktemp -d)
  (
    cd "$work_dir" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    git remote add origin "$origin_dir"

    echo base > shared.txt
    git add shared.txt
    git commit -q -m base
    git push -q origin target

    git switch -q -c source
    echo "source: older change" >> shared.txt
    git commit -q -am "source older commit"
    git push -q origin source

    # The existing (stale) forward-merge branch, built from source's older
    # commit only.
    git switch -q -c "forward-merge/rel-to-target" target
    git merge -q --no-edit source
    if [[ "$mode" == conflict ]]; then
      echo "human's own hand resolution" > shared.txt
      git commit -q -am "human hand-resolution on the stale branch"
    elif [[ "$mode" == bot-conflict ]]; then
      echo "cascade's own stale resolution" > shared.txt
      git -c user.name="github-actions[bot]" \
        -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
        commit -q -am "cascade resolution on the stale branch"
    elif [[ "$mode" == bot-author-human-committer ]]; then
      # A person amended a cascade commit: the author stays the bot, the committer changes.
      echo "human fix on a cascade commit" > shared.txt
      GIT_AUTHOR_NAME="github-actions[bot]" \
        GIT_AUTHOR_EMAIL="41898282+github-actions[bot]@users.noreply.github.com" \
        git commit -q -am "cascade resolution, amended by a person"
    fi
    git push -q origin "forward-merge/rel-to-target"

    # The new commit that landed on source after the branch above was
    # created -- what the block under test must merge forward.
    git switch -q source
    echo "source: newer change" >> shared.txt
    git commit -q -am "source newer commit"
    git push -q origin source

    git switch -q target
  )
  printf '%s %s\n' "$work_dir" "$origin_dir"
}

# ---------------------------------------------------------------------------
# Test (Issue #2220): a stale forward-merge branch is updated in place
# instead of halting the cascade forever.
#
# Scenario: $MERGE_BRANCH already exists (an earlier, now-stale attempt) and
# a new commit has since landed on the source branch. The old behaviour
# halted every run from here on, silently, until a human noticed and updated
# the branch by hand.
#
# Expected after fix: the new source commit is merged into the existing
# branch (a plain push, never --force) and the result is pushed back, so the
# already-open PR picks up the new commit automatically.
# ---------------------------------------------------------------------------
test_2220_stale_branch_is_updated_not_halted() {
  local dirs work_dir origin_dir git_stub out final_content
  dirs=$(make_stale_merge_branch_repo clean)
  work_dir="${dirs% *}"
  origin_dir="${dirs#* }"

  git_stub=$(mktemp -d)
  cat > "$git_stub/git" <<STUB
#!/usr/bin/env bash
for arg in "\$@"; do
  if [[ "\$arg" == "--force" || "\$arg" == "--force-with-lease" ]]; then
    echo "::error::force push used: \$*" >&2
    exit 99
  fi
done
exec "$(command -v git)" "\$@"
STUB
  chmod +x "$git_stub/git"

  local script out
  script=$(existing_branch_update_block)
  out=$(cd "$work_dir" && PATH="$git_stub:$PATH" \
    SOURCE_REF=origin/source TARGET=target SOURCE_DESC=release/x \
    MERGE_BRANCH="forward-merge/rel-to-target" ls_rc=0 \
    bash -c "$script" 2>&1)
  local rc=$?
  trash "$git_stub" 2>/dev/null || rm -rf "$git_stub"

  if [[ $rc -ne 0 ]]; then
    echo "  block exited $rc, expected 0. Output:" >&2
    printf '    %s\n' "${out//$'\n'/$'\n'    }" >&2
    rm -rf "$work_dir" "$origin_dir"
    return 1
  fi

  final_content=$(git -C "$origin_dir" show "forward-merge/rel-to-target:shared.txt")
  rm -rf "$work_dir" "$origin_dir"

  if ! echo "$final_content" | grep -q "older change"; then
    echo "  updated branch lost the pre-existing (older) content -- got: $final_content" >&2
    return 1
  fi
  if ! echo "$final_content" | grep -q "newer change"; then
    echo "  updated branch never picked up the new source commit -- got: $final_content" >&2
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Test (Issue #2220, review follow-up of #476): updating a stale branch must
# never destroy a human's in-progress conflict resolution. When the new
# source commit conflicts with what's already on the branch, bail instead of
# force-pushing over it.
# ---------------------------------------------------------------------------
test_2220_conflicting_update_never_force_pushes() {
  local dirs work_dir origin_dir git_stub out before_content after_content

  dirs=$(make_stale_merge_branch_repo conflict)
  work_dir="${dirs% *}"
  origin_dir="${dirs#* }"
  before_content=$(git -C "$origin_dir" show "forward-merge/rel-to-target:shared.txt")

  git_stub=$(mktemp -d)
  cat > "$git_stub/git" <<STUB
#!/usr/bin/env bash
for arg in "\$@"; do
  if [[ "\$arg" == "--force" || "\$arg" == "--force-with-lease" ]]; then
    echo "::error::force push used: \$*" >&2
    exit 99
  fi
done
exec "$(command -v git)" "\$@"
STUB
  chmod +x "$git_stub/git"

  local script
  script=$(existing_branch_update_block)
  out=$(cd "$work_dir" && PATH="$git_stub:$PATH" \
    SOURCE_REF=origin/source TARGET=target SOURCE_DESC=release/x \
    MERGE_BRANCH="forward-merge/rel-to-target" ls_rc=0 \
    bash -c "$script" 2>&1 || true)

  after_content=$(git -C "$origin_dir" show "forward-merge/rel-to-target:shared.txt")
  rm -rf "$work_dir" "$origin_dir" "$git_stub"

  if [[ "$before_content" != "$after_content" ]]; then
    echo "  remote branch content changed despite an unresolved conflict -- possible overwrite of human work" >&2
    echo "  before: $before_content" >&2
    echo "  after:  $after_content" >&2
    return 1
  fi
  if ! echo "$out" | grep -q "Merge conflict"; then
    echo "  expected a merge-conflict message; got:" >&2
    printf '    %s\n' "${out//$'\n'/$'\n'    }" >&2
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Issue #2504: a stale merge branch that only the cascade wrote, with no open
# PR, is reset to a fresh merge instead of halting every later run.
# ---------------------------------------------------------------------------

# Run the stale-branch block the way the cascade reaches it: HEAD holds the
# fresh merge of the source into the target. Args: fixture mode, PR list JSON
# (default: one open PR). Prints the block output, then BEFORE_SHA, AFTER_SHA
# and REMOTE_SHARED (the branch's shared.txt, lines joined with "|").
run_reset_case() {
  local mode="$1" prs="${2-}" dirs work_dir origin_dir before after shared out
  dirs=$(make_stale_merge_branch_repo "$mode")
  work_dir="${dirs% *}"
  origin_dir="${dirs#* }"
  before=$(git -C "$origin_dir" rev-parse "forward-merge/rel-to-target")
  (cd "$work_dir" && git switch -q --detach origin/target \
    && git merge -q --no-edit origin/source) || return 1
  if [[ -n "$prs" ]]; then export GH_PRS_JSON="$prs"; else unset GH_PRS_JSON; fi
  out=$(cd "$work_dir" && PATH="${RESET_CASE_PATH_PREFIX:+$RESET_CASE_PATH_PREFIX:}$PATH" \
    SOURCE_REF=origin/source TARGET=target SOURCE_DESC=release/x \
    MERGE_BRANCH="forward-merge/rel-to-target" ls_rc=0 \
    bash -c "$(existing_branch_update_block)" 2>&1 || true)
  after=$(git -C "$origin_dir" rev-parse "forward-merge/rel-to-target")
  shared=$(git -C "$origin_dir" show "forward-merge/rel-to-target:shared.txt" | paste -sd '|' -)
  rm -rf "$work_dir" "$origin_dir"
  printf '%s\nBEFORE_SHA=%s\nAFTER_SHA=%s\nREMOTE_SHARED=%s\n' "$out" "$before" "$after" "$shared"
}

reset_case_sha() { sed -n "s/^$2=//p" <<< "$1"; }

test_2504_cascade_only_stale_branch_with_no_pr_is_reset() {
  local out before after
  out=$(run_reset_case bot-conflict "[]")
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ -n "$before" && "$before" != "$after" ]] \
    && [[ "$out" == *"Reset stale forward-merge/rel-to-target (old tip $before)"* ]] \
    && [[ "$out" == *"REMOTE_SHARED=base|source: older change|source: newer change"* ]] \
    && [[ "$out" == *"GH_CALL:pr create --base target"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# The old tip must outlive the run log: the new PR names it and the commits
# the reset dropped, and the restore command in the log really restores it.
test_2504_reset_pr_body_names_the_old_tip_and_dropped_commits() {
  local out before
  out=$(run_reset_case bot-conflict "[]")
  before=$(reset_case_sha "$out" BEFORE_SHA)
  [[ "$out" == *"(forced update)"* ]] \
    && [[ "$out" == *"GH_CALL:pr create"*"reset it to a fresh merge"*"Old tip: $before"*"cascade resolution on the stale branch"* ]] \
    && [[ "$out" == *"Restore it with: git push --force-with-lease=forward-merge/rel-to-target:"*" origin $before:refs/heads/forward-merge/rel-to-target"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2504_stale_branch_with_an_open_pr_is_not_reset() {
  local out before after
  out=$(run_reset_case bot-conflict)
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ "$before" == "$after" ]] && [[ "$out" == *"Merge conflict"* ]] \
    && [[ "$out" != *"Reset stale"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2504_stale_branch_with_a_human_commit_is_not_reset() {
  local out before after
  out=$(run_reset_case conflict "[]")
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ "$before" == "$after" ]] && [[ "$out" == *"Merge conflict"* ]] \
    && [[ "$out" != *"Reset stale"* ]] && [[ "$out" != *"GH_CALL:pr create"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

test_2504_stale_branch_amended_by_a_person_is_not_reset() {
  local out before after
  out=$(run_reset_case bot-author-human-committer "[]")
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ "$before" == "$after" ]] && [[ "$out" == *"Merge conflict"* ]] \
    && [[ "$out" != *"Reset stale"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# When git cannot list the branch's own commits, the run must say why and halt
# without a reset, instead of guessing that the branch is safe to discard.
test_2504_unreadable_branch_history_halts_and_names_the_error() {
  local stubs out before after
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == log && "\$*" == *"--format=%ae %ce"* ]]; then
  echo "fatal: bad object deadbeef" >&2
  exit 128
fi
exec "$(command -v git)" "\$@"
STUB
  chmod +x "$stubs/git"
  out=$(RESET_CASE_PATH_PREFIX="$stubs" run_reset_case bot-conflict "[]")
  trash "$stubs" 2>/dev/null || true
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ "$before" == "$after" ]] && [[ "$out" != *"Reset stale"* ]] \
    && [[ "$out" == *"fatal: bad object deadbeef"* ]] && [[ "$out" == *"Merge conflict"* ]] \
    && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# #2795: with no open PR, a cascade-only branch is reset even when the merge
# would be clean, so old cascade merges do not pile up on it.
test_2795_stale_branch_that_merges_cleanly_is_reset() {
  local out before after
  out=$(run_reset_case clean "[]")
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ "$before" != "$after" ]] && [[ "$out" == *"Reset stale"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Drift guard: existing_branch_update_block extracts real production lines
# (not a hand copy) -- assert they're still shaped the way the sed ranges
# above expect, so a future edit there fails loudly here instead of quietly
# extracting nothing.
test_2220_test_mirror_matches_production_stale_branch_update() {
  # shellcheck disable=SC2016 # literal grep -F patterns, not expressions to expand
  if grep -qF 'if [[ $ls_rc -eq 0 ]]; then' "$WORKFLOW" \
      && grep -qF 'elif [[ $ls_rc -ne 2 ]]; then' "$WORKFLOW" \
      && grep -qF 'if ! git fetch origin "$MERGE_BRANCH"; then' "$WORKFLOW" \
      && grep -qF 'if ! git checkout -B "$MERGE_BRANCH" "origin/$MERGE_BRANCH"; then' "$WORKFLOW" \
      && grep -qF 'if ! push_with_app_token origin "$MERGE_BRANCH" ${RESET_LEASE:+"$RESET_LEASE"}; then' "$WORKFLOW" \
      && grep -qF 'FRESH_SHA=$(git rev-parse HEAD)' "$WORKFLOW" \
      && grep -qF 'RESET_LEASE="--force-with-lease=$MERGE_BRANCH:$OLD_SHA"' "$WORKFLOW"; then
    return 0
  fi
  echo "  production's stale-branch-update block no longer matches the lines this file mirrors -- update existing_branch_update_block above" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------




run_test "Issue #482: fetch failure with stderr is logged and cascade halted" \
  test_482_fetch_fail_with_stderr

run_test "Issue #482: fetch failure with empty stderr is logged" \
  test_482_fetch_fail_empty_stderr


run_test "Issue #1381: version-only manifest conflict keeps the target's version" \
  test_1381_version_only_conflict_keeps_target_version

run_test "Issue #1381: a manifest change beyond the version bails instead of dropping it" \
  test_1381_non_version_change_bails

run_test "Issue #1525: a git diff failure inside auto_resolve_conflicts fails loudly" \
  test_1525_auto_resolve_conflicts_fails_on_git_diff_failure

run_test "Issue #1525: a sync-changelog-doc.sh failure in the changelog branch bails" \
  test_1525_changelog_regeneration_failure_bails

run_test "Issue #1534: a script-only change with no changelog.md conflict is caught" \
  test_1534_script_only_change_without_conflict_is_caught

run_test "Issue #1534: an identical script with no conflict proceeds" \
  test_1534_identical_script_without_conflict_proceeds

run_test "Issue #1534: a script added on only one side halts instead of skipping" \
  test_1534_script_added_only_on_source_halts

run_test "Issue #1534: a script missing on both sides skips the check" \
  test_1534_script_missing_on_both_sides_skips_check

run_test "Issue #1534: the test mirror still matches production's script-integrity guard" \
  test_1534_test_mirror_matches_production_script_integrity_guard





run_test "Issue #1532: identical scripts/sync-changelog-doc.sh runs the target's pinned copy" \
  test_1532_identical_script_runs_target_copy

run_test "Issue #1532: a differing scripts/sync-changelog-doc.sh bails instead of running either copy" \
  test_1532_differing_script_bails

run_test "Issue #1532: APP_TOKEN is not inherited by a spawned subprocess" \
  test_1532_app_token_not_inherited_by_subprocess

run_test "Issue #1532: the real sync-changelog-doc.sh regenerates changelog.md through the pinned-copy path" \
  test_1532_real_script_regenerates_changelog

run_test "Issue #1540: push_with_app_token clears the persisted extraheader before setting its own" \
  test_1540_push_with_app_token_clears_persisted_header_before_setting_new

run_test "Issue #1540 follow-up: push_with_app_token clears an includeIf-referenced persisted credential" \
  test_1540_push_with_app_token_clears_includeif_persisted_credential

run_test "Issue #1540 follow-up: a real includeIf enumeration failure is surfaced and halts" \
  test_1540_includeif_enumeration_real_failure_is_surfaced_and_halts

run_test "Issue #1540 follow-up: removing an includeIf entry also deletes its credentials file" \
  test_1540_includeif_removal_deletes_the_credentials_file

run_test "Issue #1540 follow-up: a non-matching credentials file is never deleted" \
  test_1540_includeif_removal_does_not_delete_non_matching_files

run_test "Issue #1540 follow-up: a second push_with_app_token call in the same job is rejected" \
  test_1540_push_with_app_token_second_call_is_rejected

run_test "Issue #1540: a real config-write failure while clearing the header is surfaced and halts" \
  test_1540_unset_all_real_failure_is_surfaced_and_halts

run_test "Issue #1540: the benign no-such-key case is silently tolerated" \
  test_1540_unset_all_missing_key_is_silently_tolerated



run_test "Issue #1543: a stop-commands sequence in a filename is stripped from log display" \
  test_1543_stop_commands_sequence_stripped_from_display
run_test "Issue #1543: a normal filename is left unchanged" \
  test_1543_normal_filename_unchanged
run_test "Issue #1543: the test mirror still matches production's file_display sanitization" \
  test_1543_test_mirror_matches_production_file_display_sanitization
run_test "Issue #1543 follow-up: git merge's own conflict output is fully suppressed" \
  test_1543_git_merge_stdout_suppressed_on_conflict
run_test "Issue #1543 follow-up: the test mirror still matches production's merge output suppression" \
  test_1543_test_mirror_matches_production_merge_output_suppression

run_test "Issue #2166: same pin bumped on both lines and lockfiles resolve with no human step" \
  test_2166_same_pin_bump_and_lockfiles_resolve

run_test "Issue #2166: a source pin newer than the target still bails" \
  test_2166_newer_source_pin_bails

run_test "Issue #2166: a failing lockfile tool bails and names the command" \
  test_2166_lockfile_tool_failure_bails_naming_command

run_test "Issue #2166: a target without the manifest script bails instead of running the source's copy" \
  test_2166_target_without_manifest_script_bails

run_test "Issue #1675: a push event uses the pushed branch as the source" \
  test_1675_push_event_uses_pushed_branch_as_source
run_test "Issue #1675: a merged forward-merge PR parses its source from the head ref" \
  test_1675_merged_pr_event_parses_source_from_head_ref
run_test "Issue #1675: parsing works between two release lines, not just release->main" \
  test_1675_merged_pr_event_parses_source_between_two_release_lines
run_test "Issue #1675 review follow-up: an empty source after prefix-strip fails loudly" \
  test_1675_empty_source_after_prefix_strip_fails_loudly
run_test "Issue #1675 review follow-up: a missing -to- separator fails loudly" \
  test_1675_missing_to_separator_fails_loudly
run_test "Issue #1675 review follow-up: multiple -to- separators fail loudly" \
  test_1675_multiple_to_separators_fails_loudly
run_test "Issue #1675 review follow-up: an arbitrary ref embedded in the head fails loudly" \
  test_1675_arbitrary_ref_embedded_in_head_fails_loudly
run_test "Issue #1675: the job admits a push from a human" \
  test_1675_job_admits_push_from_a_human
run_test "Issue #1675: the job rejects a push attributed to the bot" \
  test_1675_job_rejects_push_from_the_bot
run_test "Issue #1675: the job admits a merged forward-merge PR" \
  test_1675_job_admits_a_merged_forward_merge_pr
run_test "Issue #1675: the job rejects an unmerged closed forward-merge PR" \
  test_1675_job_rejects_an_unmerged_closed_forward_merge_pr
run_test "Issue #1675: the job rejects a merged PR that isn't a forward-merge branch" \
  test_1675_job_rejects_a_merged_pr_that_is_not_a_forward_merge_branch
run_test "Issue #1675 review follow-up: the job rejects a fork PR with a matching branch name" \
  test_1675_job_rejects_a_merged_pr_from_a_fork_with_matching_branch_name
run_test "Issue #1675 review follow-up: the job rejects a same-repo PR targeting an unrelated branch" \
  test_1675_job_rejects_a_merged_same_repo_pr_targeting_an_unrelated_branch
run_test "Issue #1675 review follow-up: the job admits a same-repo PR targeting a release branch" \
  test_1675_job_admits_a_merged_same_repo_pr_targeting_a_release_branch
run_test "Issue #1675: the test mirror still matches production's source-branch retry logic" \
  test_1675_test_mirror_matches_production_source_branch_retry
run_test "Issue #2220: a stale forward-merge branch is updated in place, not halted forever" \
  test_2220_stale_branch_is_updated_not_halted
run_test "Issue #2220: a conflicting update never force-pushes over a human's work" \
  test_2220_conflicting_update_never_force_pushes
run_test "Issue #2220: the test mirror still matches production's stale-branch-update logic" \
  test_2220_test_mirror_matches_production_stale_branch_update
run_test "Issue #2240: a malformed PR head ref is printed with :: removed" \
  test_2240_malformed_head_ref_error_is_sanitized
run_test "Issue #2240: a branch that is not a release line is printed with :: removed" \
  test_2240_unversioned_branch_warning_is_sanitized
run_test "Issue #2240: every later echo reads only regex-bounded branch names" \
  test_2240_cascade_targets_are_regex_bounded
run_test "Issue #2271: a failed merge with no conflicted file bails, not resolves" \
  test_2271_no_conflicted_files_bails
run_test "Issue #2271: a manifest check that cannot parse a file says so" \
  test_2271_manifest_parse_failure_is_could_not_check
run_test "Issue #2271: a failed checkout --ours on a manifest bails" \
  test_2271_manifest_checkout_failure_bails
run_test "Issue #2271: a failed gh pr create is not reported as an opened PR" \
  test_2271_failed_pr_create_is_not_reported_as_opened
run_test "Issue #2271: a failed ls-remote prints git's sanitized stderr" \
  test_2271_ls_remote_failure_prints_sanitized_stderr
run_test "Issue #2271: cargo output from merged manifests is printed with :: removed" \
  test_2271_lockfile_tool_output_is_sanitized
run_test "Issue #2271: a failed ls-remote with no stderr says so" \
  test_2271_ls_remote_failure_without_stderr_says_so
run_test "Issue #2271: a stale-branch merge with no MERGE_HEAD halts with git's reason" \
  test_2271_stale_branch_merge_without_merge_head_names_git_reason
run_test "Issue #2271: a merge that fails with no MERGE_HEAD halts with git's reason" \
  test_2271_merge_without_merge_head_names_git_reason

run_test "Issue #2272: an orphan merge branch is updated and gets a new PR" \
  test_2272_orphan_branch_is_updated_and_gets_a_new_pr
run_test "Issue #2272: an orphan merge branch that is already current gets a new PR" \
  test_2272_orphan_branch_already_current_gets_a_new_pr
run_test "Issue #2272: a branch with an open PR is updated in place" \
  test_2272_open_pr_is_updated_in_place
run_test "Issue #2272: a fork PR with the same head name does not count as open" \
  test_2272_fork_pr_with_same_head_name_does_not_count
run_test "Issue #2272: a new PR on an existing branch lists the branch's own commits" \
  test_2272_new_pr_lists_the_branch_own_commits
run_test "Issue #2272: a current branch with an open PR halts without a push" \
  test_2272_current_branch_with_open_pr_halts_without_push
run_test "Issue #2272: unreadable gh pr list output halts" \
  test_2272_unreadable_pr_list_halts
run_test "Issue #2272: a failed gh pr create on an orphan branch halts" \
  test_2272_failed_pr_create_on_orphan_branch_halts
run_test "Issue #2272: a failed open-PR lookup halts with a sanitized error" \
  test_2272_pr_lookup_failure_halts
run_test "Issue #2273: only this repo's gated runs on the merge branch are approved" \
  test_2273_approves_only_own_runs_on_the_merge_branch
run_test "Issue #2273: a run gated after the first hit is still approved" \
  test_2273_approves_a_run_gated_after_the_first_hit
run_test "Issue #2273: a failed run list is a sanitized warning" \
  test_2273_list_failure_is_a_sanitized_warning
run_test "Issue #2273: a failed approval is a sanitized warning" \
  test_2273_approve_failure_is_a_sanitized_warning
run_test "Issue #2273: the run list is filtered on the server and paginated" \
  test_2273_list_is_filtered_and_paginated
run_test "Issue #2273: a failed approval is retried on a later round" \
  test_2273_failed_approval_is_retried
run_test "Issue #2273: one failed list round after a good one is not a warning" \
  test_2273_one_failed_round_is_not_a_list_warning
run_test "Issue #2273: runs with missing fields neither crash nor match" \
  test_2273_filter_skips_runs_with_missing_fields
run_test "Issue #2273: the SHA check accepts only 40 lowercase hex digits" \
  test_2273_sha_shape_boundaries
run_test "Issue #2273: nothing pending means no approval and no warning" \
  test_2273_nothing_pending_never_approves
run_test "Issue #2273: a failed approval does not halt the cascade" \
  test_2273_approve_failure_does_not_halt
run_test "Issue #2273: an invalid SHA makes no API call" \
  test_2273_invalid_sha_makes_no_api_call
run_test "Issue #2294: a valid retry keeps every downstream target" \
  test_2294_valid_retry_keeps_every_target
run_test "Issue #2294: a retry target that is not downstream fails loudly" \
  test_2294_retry_target_not_downstream_fails
run_test "Issue #2294: a retry base must be the target its head branch names" \
  test_2294_retry_base_must_match_head_target
run_test "Issue #2294: a retry stops at its own target once that target is current" \
  test_2294_retry_stops_at_its_up_to_date_target
run_test "Issue #2280: an unreadable target ref halts" \
  test_2280_unreadable_target_ref_halts
run_test "Issue #2280: a failed ancestry check halts instead of merging" \
  test_2280_ancestry_check_failure_halts
run_test "Issue #2280: a target that lacks the source still merges" \
  test_2280_not_an_ancestor_still_merges
run_test "Issue #2285: an up-to-date target advances the chain to the next target" \
  test_2285_up_to_date_target_advances_the_chain

run_test "Issue #2299: the harness runs outside the repo with no GitHub credentials" \
  test_2299_harness_is_isolated

run_test "Issue #2504: a cascade-only stale branch with no PR is reset to a fresh merge" \
  test_2504_cascade_only_stale_branch_with_no_pr_is_reset
run_test "Issue #2504: the PR for a reset branch names the old tip and the dropped commits" \
  test_2504_reset_pr_body_names_the_old_tip_and_dropped_commits
run_test "Issue #2504: a stale branch with an open PR is not reset" \
  test_2504_stale_branch_with_an_open_pr_is_not_reset
run_test "Issue #2504: a stale branch with a human commit is not reset" \
  test_2504_stale_branch_with_a_human_commit_is_not_reset
run_test "Issue #2504: a cascade commit amended by a person is not reset" \
  test_2504_stale_branch_amended_by_a_person_is_not_reset
run_test "Issue #2504: an unreadable branch history keeps the branch and names the git error" \
  test_2504_unreadable_branch_history_halts_and_names_the_error
run_test "Issue #2795: a stale branch that merges cleanly is reset" \
  test_2795_stale_branch_that_merges_cleanly_is_reset



test_2796_app_credentials_select_the_app() {
  run_helper check_cascade_credential SYNC_APP_ID=1 SYNC_APP_PRIVATE_KEY=k
  [[ "$RUN_RC" -eq 0 && -z "$RUN_ERR" ]]
}

test_2796_no_credentials_fails_and_names_the_fix() {
  run_helper check_cascade_credential
  [[ "$RUN_RC" -ne 0 && "$RUN_ERR" == *"SYNC_APP_ID"* && "$RUN_ERR" == *"SYNC_APP_PRIVATE_KEY"* ]]
}

# Stubs gh. Records its arguments in $GH_LOG and exits with $GH_STUB_RC.
make_gh_stub() {
  local dir="$1"
  cat > "$dir/gh" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$GH_LOG"
[[ "$GH_STUB_RC" -eq 0 ]] || echo "auto-merge is disabled" >&2
exit "$GH_STUB_RC"
STUB
  chmod +x "$dir/gh"
}

test_2796_auto_merge_requests_a_merge_commit() {
  local dir
  dir=$(mktemp -d)
  make_gh_stub "$dir"
  env -i PATH="$dir:$PATH" GH_LOG="$dir/log" GH_STUB_RC=0 bash -c \
    "$(workflow_function enable_auto_merge)
enable_auto_merge forward-merge/release/3.x-to-main" >/dev/null 2>&1 || true
  local logged
  logged=$(<"$dir/log")
  rm -rf "$dir"
  [[ "$logged" == "pr merge forward-merge/release/3.x-to-main --auto --merge" ]]
}

test_2796_failed_auto_merge_warns_and_keeps_the_cascade_running() {
  local dir out rc=0
  dir=$(mktemp -d)
  make_gh_stub "$dir"
  out=$(env -i PATH="$dir:$PATH" GH_LOG="$dir/log" GH_STUB_RC=1 GITHUB_STEP_SUMMARY="$dir/summary" bash -c \
    "$(workflow_function enable_auto_merge)
enable_auto_merge forward-merge/release/3.x-to-main" 2>&1) || rc=$?
  local logged summary
  logged=$(<"$dir/log")
  summary=$(<"$dir/summary")
  rm -rf "$dir"
  [[ "$rc" -eq 0 && "$out" == *"::warning::"* && "$out" == *"auto-merge is disabled"* ]] \
    && [[ "$logged" == *"pr comment forward-merge/release/3.x-to-main"* ]] \
    && [[ "$summary" == *"Auto-merge refused for forward-merge/release/3.x-to-main"* ]]
}

test_2796_pr_opened_exits_zero_and_writes_the_summary() {
  local summary
  summary=$(mktemp)
  run_helper finish_cascade HELD="PR opened for release/4.x" GITHUB_STEP_SUMMARY="$summary"
  local text
  text=$(<"$summary")
  rm -f "$summary"
  [[ "$RUN_RC" -eq 0 && "$text" == *"PR opened for release/4.x"* ]]
}

test_2796_halted_cascade_still_exits_nonzero() {
  run_helper finish_cascade HALTED="conflict updating existing forward-merge/release/3.x-to-main"
  [[ "$RUN_RC" -ne 0 && "$RUN_OUT" == *"halted"* ]]
}

test_2796_clean_cascade_exits_zero() {
  run_helper finish_cascade
  [[ "$RUN_RC" -eq 0 && "$RUN_OUT" == *"completed"* ]]
}

run_test "Issue #2796: App credentials select the App" \
  test_2796_app_credentials_select_the_app
run_test "Issue #2796: no credentials fails and names the secrets" \
  test_2796_no_credentials_fails_and_names_the_fix
run_test "Issue #2796: auto-merge requests a merge commit" \
  test_2796_auto_merge_requests_a_merge_commit
run_test "Issue #2796: a failed auto-merge warns and the cascade keeps running" \
  test_2796_failed_auto_merge_warns_and_keeps_the_cascade_running
run_test "Issue #2796: an opened PR exits zero and writes the job summary" \
  test_2796_pr_opened_exits_zero_and_writes_the_summary
run_test "Issue #2796: a halted cascade still exits nonzero" \
  test_2796_halted_cascade_still_exits_nonzero
run_test "Issue #2796: a clean cascade exits zero" \
  test_2796_clean_cascade_exits_zero

test_2796_missing_app_key_fails_and_names_the_secret() {
  run_helper check_cascade_credential SYNC_APP_ID=1
  [[ "$RUN_RC" -ne 0 && "$RUN_ERR" == *"Missing SYNC_APP_PRIVATE_KEY."* ]]
}

test_2796_missing_app_token_fails() {
  run_helper require_app_token
  [[ "$RUN_RC" -ne 0 && "$RUN_ERR" == *"No App token was minted"* ]]
}

test_2796_app_mode_with_a_token_passes() {
  run_helper require_app_token _app_token=tok
  [[ "$RUN_RC" -eq 0 ]]
}

test_2796_unset_step_summary_fails_loudly() {
  local rc=0 err
  err=$(env -i PATH="$PATH" HELD="PR opened for release/4.x" bash -c "set -euo pipefail
$(workflow_function finish_cascade)
finish_cascade" 2>&1 >/dev/null) || rc=$?
  [[ "$rc" -ne 0 && "$err" == *"GITHUB_STEP_SUMMARY is unset"* ]]
}

# shellcheck disable=SC2016 # literal workflow text, not an expression to expand
test_2796_app_token_stays_out_of_checkout_and_step_env() {
  [[ -r "$WORKFLOW" ]] \
    && grep -Fq 'APP_TOKEN: ${{ steps.app.outputs.token }}' "$WORKFLOW" \
    && ! grep -Fq 'token: ${{ steps.app.outputs.token' "$WORKFLOW" \
    && ! grep -Fq 'GH_TOKEN: ${{ steps.app.outputs.token' "$WORKFLOW"
}

run_test "Issue #2797: a missing App key fails and names the secret" \
  test_2796_missing_app_key_fails_and_names_the_secret
run_test "Issue #2797: a missing App token fails the cascade" \
  test_2796_missing_app_token_fails
run_test "Issue #2796: an App token passes" \
  test_2796_app_mode_with_a_token_passes
run_test "Issue #2796: an unset step summary fails loudly" \
  test_2796_unset_step_summary_fails_loudly
run_test "Issue #2796: the App token stays out of checkout and GH_TOKEN" \
  test_2796_app_token_stays_out_of_checkout_and_step_env

test_2798_missing_workflow_function_fails_loudly() {
  local out rc=0
  out=$(workflow_function no_such_helper_2798 2>&1) || rc=$?
  [[ "$rc" -ne 0 && "$out" == *"no_such_helper_2798() not found"* ]]
}

# #1564: a reply that jq cannot parse is a failed listing, not "no run waited".
test_2803_approve_reports_an_unparsable_run_list() {
  local dir out
  dir=$(mktemp -d)
  printf '#!/usr/bin/env bash\necho "this is not json"\n' > "$dir/gh"
  chmod +x "$dir/gh"
  out=$(env -i PATH="$dir:$PATH" GITHUB_REPOSITORY=o/r bash -c "sleep() { :; }
$(workflow_function approve_pending_runs)
approve_pending_runs 0123456789abcdef0123456789abcdef01234567 forward-merge/x" 2>&1)
  trash "$dir" 2>/dev/null || true
  [[ "$out" == *"::warning::could not list runs"* && "$out" != *"No run for"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# A reset must name what it drops, so a failed listing halts before the reset.
test_2803_failed_drop_listing_halts_before_the_reset() {
  local stubs out before after
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == log && "\$*" == *"--format=- %h %s"* ]]; then
  echo "fatal: listing broke" >&2
  exit 128
fi
exec "$(command -v git)" "\$@"
STUB
  chmod +x "$stubs/git"
  out=$(RESET_CASE_PATH_PREFIX="$stubs" run_reset_case bot-conflict "[]")
  trash "$stubs" 2>/dev/null || true
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ -n "$before" && "$before" == "$after" ]] \
    && [[ "$out" == *"::error::Cannot list the commits a reset of forward-merge/rel-to-target would drop"* ]] \
    && [[ "$out" != *"Reset stale"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# Generated cases: branch_is_cascade_only is true exactly when every
# author/committer pair is the cascade pair. Fixed seed, so a failure repeats.
test_2803_cascade_only_matches_the_author_rule() {
  local bot='41898282+github-actions[bot]@users.noreply.github.com' human='p@example.com'
  local fn i n k log expect pair rc
  fn=$(workflow_function branch_is_cascade_only) || return 1
  RANDOM=2803
  for ((i = 0; i < 60; i++)); do
    n=$((RANDOM % 5))
    log=""
    expect=0
    for ((k = 0; k < n; k++)); do
      case $((RANDOM % 4)) in
        0) pair="$bot $bot" ;;
        1) pair="$human $bot"; expect=1 ;;
        2) pair="$bot $human"; expect=1 ;;
        *) pair="$human $human"; expect=1 ;;
      esac
      log+="$pair"$'\n'
    done
    rc=0
    FAKE_LOG="$log" bash -c "git() { printf '%s' \"\$FAKE_LOG\"; }
$fn
branch_is_cascade_only" >/dev/null 2>&1 || rc=$?
    if [[ "$rc" -ne "$expect" ]]; then
      printf '  case %d: expected %d, got %d for:\n%s' "$i" "$expect" "$rc" "$log" >&2
      return 1
    fi
  done
}

# #2280: only exit 1 from merge-base means "not an ancestor"; any other code halts.
test_2803_ancestry_check_failure_halts_the_update() {
  local out
  out=$(run_stale_arm 128)
  [[ "$out" == *"::error::git merge-base failed for forward-merge/release/3.x-to-release/4.x (exit 128)"* ]] \
    && [[ "$out" == *"HALTED=ancestry check failed for forward-merge/release/3.x-to-release/4.x"* ]] \
    && [[ "$out" != *"PUSHED:"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

# A failed reset checkout halts and leaves the remote branch as it was.
test_2803_failed_reset_checkout_halts() {
  local stubs out before after
  stubs=$(mktemp -d)
  cat > "$stubs/git" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == checkout && "\$2" == -B && "\$4" != origin/* ]]; then
  echo "error: checkout broke" >&2
  exit 1
fi
exec "$(command -v git)" "\$@"
STUB
  chmod +x "$stubs/git"
  out=$(RESET_CASE_PATH_PREFIX="$stubs" run_reset_case bot-conflict "[]")
  trash "$stubs" 2>/dev/null || true
  before=$(reset_case_sha "$out" BEFORE_SHA)
  after=$(reset_case_sha "$out" AFTER_SHA)
  [[ -n "$before" && "$before" == "$after" ]] \
    && [[ "$out" == *"::error::Failed to reset forward-merge/rel-to-target to the fresh merge"* ]] \
    && [[ "$out" != *"Reset stale"* ]] && return 0
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

run_test "Issue #2803: an ancestry check failure halts the update" \
  test_2803_ancestry_check_failure_halts_the_update
run_test "Issue #2803: a failed reset checkout halts" \
  test_2803_failed_reset_checkout_halts
run_test "Issue #2798: a missing workflow function fails loudly" \
  test_2798_missing_workflow_function_fails_loudly
run_test "Issue #2803: approve reports an unparsable run list" \
  test_2803_approve_reports_an_unparsable_run_list
run_test "Issue #2803: a failed drop listing halts before the reset" \
  test_2803_failed_drop_listing_halts_before_the_reset
run_test "Issue #2803: cascade-only matches the author rule on generated cases" \
  test_2803_cascade_only_matches_the_author_rule

# ---------------------------------------------------------------------------
# Build a repo where target and source both change THIRD-PARTY-LICENSES.md and
# website/docs/third-party-licenses.md, so the merge conflicts on both. The
# given generator body is written to scripts/gen-attribution.sh, so a test
# picks whether the regeneration succeeds or fails. Leaves the caller inside
# a conflicted `git merge source` on the target branch. Echoes the path.
# ---------------------------------------------------------------------------
make_attribution_conflict_repo() {
  local generator="$1" repo
  repo=$(mktemp -d)
  (
    cd "$repo" || exit 1
    git init -q -b target .
    git config user.email t@t
    git config user.name t
    git config commit.gpgsign false
    mkdir -p website/docs scripts
    printf 'base\n' > THIRD-PARTY-LICENSES.md
    printf 'base\n' > website/docs/third-party-licenses.md
    printf '%s\n' "$generator" > scripts/gen-attribution.sh
    git add THIRD-PARTY-LICENSES.md website/docs/third-party-licenses.md scripts/gen-attribution.sh
    git commit -q -m base

    git switch -q -c source
    printf 'source\n' > THIRD-PARTY-LICENSES.md
    printf 'source\n' > website/docs/third-party-licenses.md
    git commit -q -am "source change"

    git switch -q target
    printf 'target\n' > THIRD-PARTY-LICENSES.md
    printf 'target\n' > website/docs/third-party-licenses.md
    git commit -q -am "target change"

    git merge --no-commit --no-ff source >/dev/null 2>&1 || true
  )
  printf '%s\n' "$repo"
}

# ---------------------------------------------------------------------------
# Test (Issue #2972): the generated attribution files conflict on every
# forward-merge, because each release line carries its own crate versions.
# The conflict resolves by regenerating both files from the merged manifests,
# so the cascade no longer halts there.
# ---------------------------------------------------------------------------
test_2972_attribution_conflict_is_regenerated() {
  local repo out lic site
  repo=$(make_attribution_conflict_repo 'printf "regenerated\n" > THIRD-PARTY-LICENSES.md
printf "regenerated\n" > website/docs/third-party-licenses.md')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  lic=$(cd "$repo" && sed -n 1p THIRD-PARTY-LICENSES.md)
  site=$(cd "$repo" && sed -n 1p website/docs/third-party-licenses.md)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *RESOLVED* ]] && [[ "$lic" == regenerated ]] && [[ "$site" == regenerated ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  printf '  files: %s / %s\n' "$lic" "$site" >&2
  return 1
}

# ---------------------------------------------------------------------------
# Test (Issue #2972): a failing attribution regeneration bails. A stale copy
# staged as resolved would ship wrong license text, so the cascade halts.
# ---------------------------------------------------------------------------
test_2972_attribution_regeneration_failure_bails() {
  local repo out
  repo=$(make_attribution_conflict_repo 'exit 1')

  out=$(cd "$repo" && SOURCE_REF=source TARGET=main SOURCE_DESC=release/4.x \
    bash -c "$(resolve_block)" 2>&1 || true)
  trash "$repo" 2>/dev/null || true

  if [[ "$out" == *BAILED* ]] && [[ "$out" == *"gen-attribution.sh failed"* ]]; then
    return 0
  fi
  printf '  out: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

run_test "Issue #2972: a conflicted attribution file is regenerated and resolves" \
  test_2972_attribution_conflict_is_regenerated
run_test "Issue #2972: a failing attribution regeneration bails" \
  test_2972_attribution_regeneration_failure_bails

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
