#!/usr/bin/env bash
# Usage: scripts/mutants-plan.sh <base-ref> [--per-shard N] [--max-shards N]
#
# Plan the in-diff mutation run for a pull request (#2189). Print two lines on
# stdout, ready to append to $GITHUB_OUTPUT:
#
#   shards=<N>          number of shards; 0 when the diff has no mutants
#   matrix=[0,1,...]    zero-indexed shard list for a workflow matrix
#
# The shard count follows the mutant count, so a large diff does not run into
# the job timeout and a small one does not pay for idle runners. Every shard
# repeats a fixed baseline build and test run, so the count is capped.
#
# <base-ref> is a branch name such as `release/3.x`; the script diffs against
# `origin/<base-ref>`. Diagnostics go to stderr.
set -euo pipefail

usage() {
    sed -n "2,15p" "$0" | sed 's/^# \{0,1\}//' >&2
}

per_shard=6
max_shards=16
base_ref=""

while [[ $# -gt 0 ]]; do
    case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    --per-shard)
        per_shard="${2:?--per-shard needs a number}"
        shift 2
        ;;
    --max-shards)
        max_shards="${2:?--max-shards needs a number}"
        shift 2
        ;;
    -*)
        echo "mutants-plan: unknown option '$1'" >&2
        usage
        exit 2
        ;;
    *)
        base_ref="$1"
        shift
        ;;
    esac
done

if [[ -z "$base_ref" ]]; then
    echo "mutants-plan: a base ref is required, for example release/3.x" >&2
    usage
    exit 2
fi
# A branch name is attacker-controllable in CI, so check its shape before use.
if ! [[ "$base_ref" =~ ^[A-Za-z0-9._/-]+$ ]]; then
    echo "mutants-plan: base ref '$base_ref' has characters outside [A-Za-z0-9._/-]" >&2
    exit 2
fi
for value in "$per_shard" "$max_shards"; do
    if ! [[ "$value" =~ ^[1-9][0-9]{0,3}$ ]]; then
        echo "mutants-plan: '$value' is not a positive number of at most 4 digits" >&2
        exit 2
    fi
done

diff_file="$(mktemp)"
trap 'rm -f "$diff_file"' EXIT
git diff "origin/${base_ref}...HEAD" >"$diff_file"

count=0
if git diff --name-only "origin/${base_ref}...HEAD" | grep -q '\.rs$'; then
    # cargo-mutants exits non-zero when the diff holds no mutable code; that
    # means zero mutants, not a failure.
    count="$(cargo mutants --list --workspace --in-diff "$diff_file" 2>/dev/null | grep -c . || true)"
fi

shards=$(((count + per_shard - 1) / per_shard))
if ((shards > max_shards)); then
    shards="$max_shards"
fi

matrix="[]"
if ((shards > 0)); then
    matrix="[$(seq -s, 0 $((shards - 1)))]"
fi

echo "mutants-plan: $count mutants, $shards shards" >&2
echo "shards=$shards"
echo "matrix=$matrix"
