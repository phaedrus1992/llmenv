#!/usr/bin/env bash
# Open GitHub work for the authenticated user: PRs still in flight.
# Read-only. Identity resolved by `gh` (@me = the active account), so no
# username needed. Forward-looking sibling of daily-update/gh-activity.sh —
# that script asks "what happened on day X"; this asks "what is still open".
#
# Usage:  gh-open-work.sh [owner]
#   owner  optional org to scope results to. Default: empty = all your PRs
#          across every org. Set this to your own GitHub org/username to narrow
#          (don't hardcode someone else's — you'd silently query the wrong account).
set -euo pipefail

OWNER="${1:-}"

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
