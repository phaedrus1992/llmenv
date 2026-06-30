#!/bin/bash
set -euo pipefail

# Create a composite sprint GitHub issue.
#
# Usage:
#   create-composite.sh "<title>" "<body>" "<milestone>" ["<labels>"]
#   echo "<body>" | create-composite.sh "<title>" - "<milestone>" ["<labels>"]
#
# Milestone is required. Labels are OPTIONAL and may be a comma-separated list
# (e.g. "bug,area-core,P1") — upstream ship-issue / pr-review no longer enforce
# labels, and label sets vary per repo, so this script does not enforce a fixed
# allowlist. Each label is validated against the repo; existing ones are
# attached, missing ones are skipped with a warning, so a cosmetic mismatch
# never blocks the sprint.
#
# On success prints the created issue NUMBER on its own final line (so callers
# can capture it directly) after the gh URL.

if [[ $# -lt 3 ]]; then
  echo "Usage: create-composite.sh \"<title>\" \"<body>\" \"<milestone>\" [\"<labels>\"]"
  echo "Or:    echo \"<body>\" | create-composite.sh \"<title>\" - \"<milestone>\" [\"<labels>\"]"
  echo ""
  echo "Milestone required. Labels optional, comma-separated, validated against the repo's actual labels."
  echo ""
  echo "Example:"
  echo "  create-composite.sh \"Sprint: Core mechanics\" \"<body>\" \"v0.3\" \"enhancement,area-core\""
  echo "  echo \"<body>\" | create-composite.sh \"Sprint: Core mechanics\" - \"v0.3\""
  exit 1
fi

TITLE="$1"
BODY_ARG="$2"
MILESTONE="$3"
LABELS_ARG="${4:-}"

# Milestone is the one hard requirement.
if [[ -z "$MILESTONE" ]]; then
  echo "Error: Milestone is required for all issues" >&2
  exit 1
fi

# Read body from stdin if arg is "-", otherwise use arg directly.
if [[ "$BODY_ARG" == "-" ]]; then
  BODY=$(cat)
else
  BODY="$BODY_ARG"
fi

declare -a GH_ARGS=(
  "issue" "create"
  "--title" "$TITLE"
  "--body" "$BODY"
  "--milestone" "$MILESTONE"
)

# Attach each requested label that actually exists in this repo. Don't reject
# unknown labels — just warn and skip them. Labels are comma-separated.
if [[ -n "$LABELS_ARG" ]]; then
  # Fetch repo labels once so we can warn-and-skip unknown ones. If the fetch
  # itself fails (network/auth), don't silently drop every label with a
  # misleading "not found" — warn once and attach them unvalidated, letting
  # `gh issue create` surface any genuinely bad label.
  if REPO_LABELS="$(gh label list --limit 200 --json name --jq '.[].name')"; then
    labels_ok=1
  else
    echo "Warning: could not fetch repo labels (gh label list failed); attaching requested labels unvalidated." >&2
    labels_ok=0
  fi
  IFS=',' read -ra REQUESTED <<< "$LABELS_ARG"
  for raw in "${REQUESTED[@]}"; do
    # Trim surrounding whitespace.
    label="${raw#"${raw%%[![:space:]]*}"}"
    label="${label%"${label##*[![:space:]]}"}"
    [[ -z "$label" ]] && continue
    if [[ "$labels_ok" -eq 0 ]] || grep -Fxq "$label" <<< "$REPO_LABELS"; then
      GH_ARGS+=("--label" "$label")
    else
      echo "Warning: label '$label' not found in this repo; skipping it." >&2
      echo "         (Create it first with: gh label create \"$label\")" >&2
    fi
  done
fi

# Create the issue, echo gh's output (the URL), then print the bare issue number
# on the final line for easy capture by the calling skill.
URL="$(gh "${GH_ARGS[@]}")"
echo "$URL"
echo "$URL" | grep -oE '[0-9]+$' || true
