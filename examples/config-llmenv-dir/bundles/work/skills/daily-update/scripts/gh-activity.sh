#!/usr/bin/env bash
# GitHub activity for the authenticated user on a single day.
# Read-only. Identity resolved by `gh` (--author=@me), so no username needed.
#
# Usage:  gh-activity.sh YYYY-MM-DD [owner]
#   owner  optional org/user to scope to (e.g. phaedrus1992). Omit for all.
set -euo pipefail

DATE="${1:?usage: gh-activity.sh YYYY-MM-DD [owner]}"
OWNER="${2:-}"

owner_flag=()
[[ -n "$OWNER" ]] && owner_flag=(--owner="$OWNER")

echo "## PRs updated $DATE"
gh search prs --author=@me "${owner_flag[@]}" --updated="$DATE" \
  --json number,title,repository,state,url -L 50 |
  jq -r '.[] | "\(.repository.name)#\(.number) [\(.state)] \(.title)  \(.url)"'

echo "## PRs merged $DATE"
gh search prs --author=@me "${owner_flag[@]}" --merged-at="$DATE" \
  --json number,title,repository -L 50 |
  jq -r '.[] | "MERGED \(.repository.name)#\(.number) \(.title)"'

echo "## Commits authored $DATE"
gh search commits --author=@me --author-date="$DATE" \
  --json repository,sha,commit -L 100 |
  jq -r '.[] | "\(.repository.name) \(.sha[0:8]) \(.commit.message | split("\n")[0])"'

echo "## Issues updated $DATE"
gh search issues --author=@me "${owner_flag[@]}" --updated="$DATE" \
  --json number,title,repository,state -L 50 |
  jq -r '.[] | "\(.repository.name)#\(.number) [\(.state)] \(.title)"'
