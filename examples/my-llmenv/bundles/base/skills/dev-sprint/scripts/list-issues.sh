#!/bin/bash
set -euo pipefail

# List open issues grouped by milestone, with the "No Milestone" bucket first
# (so dev-sprint Phase 1 triage sees orphans immediately).
#
# Usage:
#   list-issues.sh                       # human-readable, grouped by milestone
#   list-issues.sh --json                # machine-readable JSON array (full bodies),
#                                        #   for jq/python filtering — avoids a manual re-fetch
#   list-issues.sh --milestone "vX.Y"    # only issues in that milestone
#   list-issues.sh --no-milestone        # only issues with NO milestone (triage view)
#   list-issues.sh --full-body           # human view, but print full bodies (no 100-char cut)
#   list-issues.sh --limit N             # cap fetched issues (default 100)
#
# Flags compose, e.g.:
#   list-issues.sh --json --no-milestone
#   list-issues.sh --milestone "v0.3" --full-body

MODE="human"
FILTER_MILESTONE=""
ONLY_NO_MILESTONE=0
FULL_BODY=0
LIMIT=100

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)         MODE="json"; shift ;;
    --milestone)    FILTER_MILESTONE="${2:-}"; shift 2 ;;
    --no-milestone) ONLY_NO_MILESTONE=1; shift ;;
    --full-body)    FULL_BODY=1; shift ;;
    --limit)        LIMIT="${2:-100}"; shift 2 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *)
      echo "Error: unknown argument '$1'. Run with --help." >&2
      exit 1 ;;
  esac
done

# Use .tmp/ directory at project root for temporary files
PROJECT_ROOT="${PROJECT_ROOT:-.}"
TMPDIR="$PROJECT_ROOT/.tmp"
mkdir -p "$TMPDIR"
TMPFILE="$TMPDIR/list-issues-$$.json"
trap 'rm -f "$TMPFILE"' EXIT

# Fetch open issues. Always pull the fields callers were re-fetching manually
# (full body, labels, milestone, state, url) so no follow-up gh call is needed.
if ! gh issue list --state open \
      --json number,title,milestone,body,labels,createdAt,url \
      --limit "$LIMIT" > "$TMPFILE"; then
  echo "Error: Failed to fetch issues from GitHub. Ensure you're authenticated with 'gh auth login' and inside the right repo." >&2
  exit 1
fi

uv run python3 - "$MODE" "$FILTER_MILESTONE" "$ONLY_NO_MILESTONE" "$FULL_BODY" "$TMPFILE" << 'EOF'
import json
import sys
from collections import defaultdict

mode, filter_milestone, only_no_ms, full_body, tmpfile = (
    sys.argv[1], sys.argv[2], sys.argv[3] == "1", sys.argv[4] == "1", sys.argv[5],
)

with open(tmpfile) as f:
    try:
        data = json.load(f)
    except (json.JSONDecodeError, ValueError) as e:
        print("Error: Failed to parse GitHub issues.", file=sys.stderr)
        print(f"Details: {e}", file=sys.stderr)
        sys.exit(1)


def milestone_title(issue):
    return issue["milestone"]["title"] if issue.get("milestone") else "No Milestone"


# Apply filters
if only_no_ms:
    data = [i for i in data if not i.get("milestone")]
if filter_milestone:
    data = [i for i in data if milestone_title(i) == filter_milestone]

if mode == "json":
    # Machine-readable: emit exactly what we fetched (full bodies included) so
    # the caller can jq/python-filter without a second gh call.
    json.dump(data, sys.stdout, indent=2)
    print()
    sys.exit(0)

if not data:
    print("No matching open issues found.")
    sys.exit(0)

by_milestone = defaultdict(list)
for issue in data:
    by_milestone[milestone_title(issue)].append(issue)

# "No Milestone" FIRST (triage these into a milestone before selecting work),
# then the rest alphabetically.
milestones = sorted(by_milestone.keys(), key=lambda x: (x != "No Milestone", x))

for milestone in milestones:
    issues = sorted(
        by_milestone[milestone],
        key=lambda x: (
            not any(label["name"].lower() == "bug" for label in x["labels"]),  # bugs first
            x["createdAt"],  # older first
        ),
    )
    print(f"\n{'=' * 60}")
    print(f"MILESTONE: {milestone}")
    print(f"{'=' * 60}")
    print(f"Count: {len(issues)} issue(s)\n")
    for issue in issues:
        labels = ", ".join(label["name"] for label in issue["labels"]) if issue["labels"] else "(no labels)"
        print(f"  #{issue['number']} — {issue['title']}")
        print(f"      Labels: {labels}")
        print(f"      Created: {issue['createdAt']}")
        # full_body decides the format in one place: full prints every line
        # indented; otherwise a 100-char inline snippet (or the no-body marker).
        if full_body:
            print("      Body:")
            for line in (issue["body"] or "(no description)").splitlines():
                print(f"        {line}")
            print()
        elif issue["body"]:
            print(f"      {issue['body'][:100]}...\n")
        else:
            print("      (no description)\n")
EOF
