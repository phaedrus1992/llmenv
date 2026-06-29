#!/usr/bin/env bash
# Open GitHub work for the authenticated user: PRs still in flight.
# Read-only. Identity resolved by `gh` (@me = the active account), so no
# username needed. Forward-looking sibling of daily-update/gh-activity.sh —
# that script asks "what happened on day X"; this asks "what is still open".
#
# Usage:  gh-open-work.sh [owner]
#   owner  optional org to scope to (default: phaedrus1992). Pass "" for all.
set -euo pipefail

OWNER="${1-phaedrus1992}"

owner_flag=()
[[ -n "$OWNER" ]] && owner_flag=(--owner="$OWNER")

echo "## My open PRs"
gh search prs --author=@me --state=open "${owner_flag[@]}" \
  --json number,title,repository,isDraft,url,updatedAt -L 50 |
  jq -r 'sort_by(.updatedAt) | reverse | .[] |
    "\(.repository.name)#\(.number) [\(if .isDraft then "draft" else "ready" end)] \(.title)  updated:\(.updatedAt[0:10])  \(.url)"'

echo
echo "## PRs awaiting my review"
gh search prs --review-requested=@me --state=open "${owner_flag[@]}" \
  --json number,title,repository,author,url,updatedAt -L 50 |
  jq -r 'sort_by(.updatedAt) | reverse | .[] |
    "\(.repository.name)#\(.number) by @\(.author.login) \(.title)  updated:\(.updatedAt[0:10])  \(.url)"'
