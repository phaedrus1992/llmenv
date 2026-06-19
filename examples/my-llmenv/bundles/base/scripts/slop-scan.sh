#!/usr/bin/env bash
# slop-scan.sh — prose quality checker for commits, PR descriptions, and docs.
#
# Usage: slop-scan.sh <file>
#        echo "some text" | slop-scan.sh -
#
# Exits 1 and prints offending lines if "slop" patterns are found.
# Exits 0 if the text is clean.
#
# HOW IT INTEGRATES:
#   Referenced from the writing-style skill and from the dev-sprint skill's
#   PR description phase. The permission in config.yaml allows Claude Code to
#   invoke it without a prompt:
#     { tool: Bash, pattern: ".../bundles/base/scripts/slop-scan.sh *" }
#
#   Skills invoke it like:
#     bash /Users/alice/git/my-llmenv/bundles/base/scripts/slop-scan.sh <file>
#
# ADD YOUR OWN PATTERNS at the bottom of SLOP_PATTERNS. The list below covers
# the most common LLM-generated boilerplate phrases.

set -euo pipefail

# Patterns that indicate "slop" — marketing speak, passive filler, LLM clichés.
# Each pattern is a grep -E extended regex, case-insensitive.
SLOP_PATTERNS=(
    "\bleverage[sd]?\b"
    "\bseamless(ly)?\b"
    "\brobust\b"
    "\bcomprehensive\b"
    "\bempow(er|ers|ering|ered)\b"
    "\bsolution\b"
    "\bin order to\b"
    "\butilize[sd]?\b"
    "\bfacilitate[sd]?\b"
    "\benhance[sd]?\b"
    "\bsignificant(ly)?\b"
    "\boptimal(ly)?\b"
    "\bstreamline[sd]?\b"
    "\bstate-of-the-art\b"
    "\bcutting-edge\b"
    "\bin this (PR|pull request|commit|change)\b"
    "\bthis (PR|pull request|commit|change) (adds|removes|updates|fixes|introduces)\b"
    "\bas part of this\b"
    "\bI (was|am) (trying|looking|working)\b"
)

# --------------------------------------------------------------------------
INPUT="${1:--}"  # default to stdin
found=0

if [[ "$INPUT" == "-" ]]; then
    content=$(cat)
    source="<stdin>"
else
    content=$(cat "$INPUT")
    source="$INPUT"
fi

for pattern in "${SLOP_PATTERNS[@]}"; do
    matches=$(echo "$content" | grep -inE "$pattern" || true)
    if [[ -n "$matches" ]]; then
        echo "slop-scan: [$source] pattern '$pattern' matched:" >&2
        echo "$matches" | head -5 >&2
        found=1
    fi
done

if [[ $found -eq 1 ]]; then
    echo "" >&2
    echo "slop-scan: rewrite the flagged lines — see writing-style skill." >&2
    exit 1
fi

exit 0
