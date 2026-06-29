#!/usr/bin/env bash
# Resolve the base branch for a sprint/issue from its milestone.
#
# Rules:
#   - Versioned milestone ("X.Y — theme", "vX.Y"): use release/X.Y.x if it exists.
#   - "Large Features" milestone (title matches /large/i): use the default branch (main).
#   - Bug Fixes / Small Enhancements (no version, not large): use the latest release/X.x.
#   - No release branches at all: fall back to the default branch.
#
# Prints the resolved base branch on stdout (single line) for `$(...)` capture;
# diagnostics go to stderr.
#
# Usage:
#   resolve-base-branch.sh --milestone "Small Enhancements"
#   resolve-base-branch.sh --milestone "1.0 — Core correctness"
#   resolve-base-branch.sh --issue 306        # looks up the issue's milestone via gh
#   resolve-base-branch.sh                     # no input -> default branch
#
# Exit codes: 0 on success (base branch printed). Non-zero only on hard failure
# (gh lookup requested but failed). A milestone with no release branch is NOT a
# failure — it deliberately falls back to the default branch.
set -euo pipefail

MILESTONE=""
ISSUE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --milestone)
      MILESTONE="${2:-}"
      shift 2
      ;;
    --issue)
      ISSUE="${2:-}"
      shift 2
      ;;
    -h | --help)
      grep '^#' "$0" | grep -v '^#!' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "resolve-base-branch.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

default_branch() {
  git remote show origin | sed -n 's/.*HEAD branch: //p'
}

# Resolve the origin default branch, fail hard if it can't be determined, log
# the fallback reason ($1) to stderr, then print the branch to stdout.
emit_default_branch() {
  local base
  base="$(default_branch)"
  if [[ -z "$base" ]]; then
    echo "resolve-base-branch.sh: could not determine origin default branch" >&2
    exit 1
  fi
  echo "resolve-base-branch.sh: $1 -> default branch '$base'" >&2
  printf '%s\n' "$base"
}

# If an issue number was given, resolve its milestone title via gh.
if [[ -n "$ISSUE" ]]; then
  if ! MILESTONE="$(gh issue view "$ISSUE" --json milestone --jq '.milestone.title // ""')"; then
    echo "resolve-base-branch.sh: failed to look up milestone for issue #$ISSUE" >&2
    exit 1
  fi
fi

git fetch origin --quiet \
  || echo "resolve-base-branch.sh: git fetch failed (offline?); using cached refs" >&2

# Extract the first X.Y version token from the milestone title.
VER=""
if [[ -n "$MILESTONE" ]]; then
  VER="$(printf '%s' "$MILESTONE" | grep -oE '[0-9]+\.[0-9]+' | head -1 || true)"
fi

latest_release_branch() {
  git ls-remote --heads origin 'refs/heads/release/*.x' \
    | sed 's|.*refs/heads/||' \
    | sort -t/ -k2 -V \
    | tail -1
}

if [[ -n "$VER" ]] && git ls-remote --exit-code --heads origin "release/${VER}.x" >/dev/null 2>&1; then
  echo "resolve-base-branch.sh: milestone '$MILESTONE' -> release/${VER}.x" >&2
  printf '%s\n' "release/${VER}.x"
elif printf '%s' "$MILESTONE" | grep -qiE 'large'; then
  # Large Features milestones always target the default branch (main).
  emit_default_branch "large-features milestone '$MILESTONE'"
else
  # Bug Fixes / Small Enhancements: use the latest release/X.x branch.
  latest="$(latest_release_branch)"
  if [[ -n "$latest" ]]; then
    echo "resolve-base-branch.sh: milestone '$MILESTONE' -> latest release branch '$latest'" >&2
    printf '%s\n' "$latest"
  else
    emit_default_branch "no release branches on remote"
  fi
fi
