#!/usr/bin/env bash
# Tests for scripts/sync-changelog-doc.sh.
# Run: bash .github/workflows/__tests__/sync-changelog-doc.sh
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

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/sync-changelog-doc.sh"

# Each test builds a scratch copy of the script plus fixture CHANGELOG-*.md
# files, since the real script always writes website/docs/changelog.md at
# its own repo root — a scratch tree keeps tests from touching the real file.
make_scratch_repo() {
  local repo
  repo=$(mktemp -d)
  mkdir -p "$repo/scripts" "$repo/website/docs"
  cp "$SCRIPT" "$repo/scripts/sync-changelog-doc.sh"
  printf '%s\n' "$repo"
}

test_concatenates_newest_first() {
  local repo
  repo=$(make_scratch_repo)
  cat > "$repo/CHANGELOG-1.md" <<'MD'
# Changelog

Preamble text.

## [Unreleased]

- v1 entry
MD
  cat > "$repo/CHANGELOG-2.md" <<'MD'
# Changelog

Preamble text.

## [Unreleased]

- v2 entry
MD

  local out rc doc
  out=$(cd "$repo" && DRY_RUN=false bash scripts/sync-changelog-doc.sh 2>&1)
  rc=$?
  doc=$(cat "$repo/website/docs/changelog.md" 2>/dev/null || true)
  trash "$repo" 2>/dev/null || rm -rf "$repo"

  [[ $rc -eq 0 ]] \
    && [[ "$doc" == *"## Version 2.x"* ]] \
    && [[ "$doc" == *"v2 entry"* ]] \
    && [[ "$doc" == *"v1 entry"* ]] \
    && [[ "${doc%%v2 entry*}" != *"v1 entry"* ]]
}

# A `find | sort` producer feeding a `while read -d '' ... done < <(...)`
# loop can fail invisibly: process substitution's exit status isn't seen by
# the consuming loop under `set -e`, so a `sort` failure would leave the loop
# at zero (or partial) iterations, and the script would still print "Done."
# and exit 0 with an incomplete website/docs/changelog.md. Reproduced here
# with a `sort` stub that always fails; the script must exit non-zero
# instead of silently succeeding.
test_sort_failure_fails_loudly() {
  local repo
  repo=$(make_scratch_repo)
  cat > "$repo/CHANGELOG-1.md" <<'MD'
# Changelog

## [Unreleased]

- v1 entry
MD

  local fakebin
  fakebin=$(mktemp -d)
  cat > "$fakebin/sort" <<'SH'
#!/usr/bin/env bash
exit 1
SH
  chmod +x "$fakebin/sort"

  local out rc
  out=$(cd "$repo" && DRY_RUN=false PATH="$fakebin:$PATH" bash scripts/sync-changelog-doc.sh 2>&1)
  rc=$?
  trash "$repo" "$fakebin" 2>/dev/null || rm -rf "$repo" "$fakebin"

  if [[ $rc -ne 0 ]]; then
    return 0
  fi
  printf '  expected non-zero exit, got 0. output: %s\n' "${out//$'\n'/ | }" >&2
  return 1
}

run_test "sync-changelog-doc: concatenates CHANGELOG-*.md newest-version-first" \
  test_concatenates_newest_first

run_test "sync-changelog-doc: a find/sort failure fails the script instead of silently succeeding" \
  test_sort_failure_fails_loudly

# The output is built in a temp file and moved into place only on success
# (rather than truncating website/docs/changelog.md up front), so a failure
# never leaves the real file corrupted — it's left exactly as it was before
# the run.
test_sort_failure_leaves_existing_output_untouched() {
  local repo
  repo=$(make_scratch_repo)
  cat > "$repo/CHANGELOG-1.md" <<'MD'
# Changelog

## [Unreleased]

- v1 entry
MD
  printf 'previously generated content\n' > "$repo/website/docs/changelog.md"

  local fakebin
  fakebin=$(mktemp -d)
  cat > "$fakebin/sort" <<'SH'
#!/usr/bin/env bash
exit 1
SH
  chmod +x "$fakebin/sort"

  local doc
  (cd "$repo" && DRY_RUN=false PATH="$fakebin:$PATH" bash scripts/sync-changelog-doc.sh) >/dev/null 2>&1
  doc=$(cat "$repo/website/docs/changelog.md" 2>/dev/null || true)
  trash "$repo" "$fakebin" 2>/dev/null || rm -rf "$repo" "$fakebin"

  if [[ "$doc" == "previously generated content" ]]; then
    return 0
  fi
  printf '  expected the pre-existing file untouched, got: %s\n' "$doc" >&2
  return 1
}

run_test "sync-changelog-doc: a find/sort failure leaves the existing changelog.md untouched" \
  test_sort_failure_leaves_existing_output_untouched

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
