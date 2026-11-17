#!/usr/bin/env bash
# Usage: scripts/mutants-shard.sh <shard-index> <shards> <diff-file> [--budget-seconds N]
#
# Run one shard of the in-diff mutation run for a pull request (#3569). The shard runs inside a
# time budget that is shorter than the job limit. A shard that runs out of budget is reported as
# incomplete, with a warning annotation and a step summary, and exits 0. Without the budget, the
# job limit cancels the shard, and the check fails with no finding.
#
# <shard-index> is zero-based, as `cargo mutants --shard k/n` needs. A shard that finishes exits
# with the status of cargo-mutants, so a surviving mutant still fails the check. The weekly sweep
# covers the mutants of an incomplete shard. Diagnostics go to stderr.
set -euo pipefail

usage() {
    sed -n "2,12p" "$0" | sed 's/^# \{0,1\}//' >&2
}

# The job limit is 45 minutes. The budget leaves 5 minutes for the artifact upload.
budget=2400
positional=()

while [[ $# -gt 0 ]]; do
    case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    --budget-seconds)
        budget="${2:?--budget-seconds needs a number}"
        shift 2
        ;;
    -*)
        echo "mutants-shard: unknown option '$1'" >&2
        usage
        exit 2
        ;;
    *)
        positional+=("$1")
        shift
        ;;
    esac
done

if [[ ${#positional[@]} -ne 3 ]]; then
    echo "mutants-shard: expected <shard-index> <shards> <diff-file>" >&2
    usage
    exit 2
fi
shard="${positional[0]}"
shards="${positional[1]}"
diff_file="${positional[2]}"
for value in "$shard" "$shards" "$budget"; do
    if ! [[ "$value" =~ ^[0-9]{1,6}$ ]]; then
        echo "mutants-shard: '$value' is not a number of at most 6 digits" >&2
        exit 2
    fi
done
if ((shard >= shards)); then
    echo "mutants-shard: shard index $shard must be below the shard count $shards" >&2
    exit 2
fi
if [[ ! -f "$diff_file" ]]; then
    echo "mutants-shard: diff file '$diff_file' does not exist" >&2
    exit 2
fi

status=0
# --test-tool nextest stops a mutant at its first failing test. --in-place reuses the cached
# target directory of the checkout, which is disposable in CI.
timeout "$budget" cargo mutants --workspace --in-place --test-tool nextest \
    --in-diff "$diff_file" --shard "$shard/$shards" || status=$?

if [[ "$status" -eq 124 ]]; then
    message="mutants shard $((shard + 1))/$shards ran out of its ${budget} s budget and is incomplete. \
No mutant survived before the stop, but the rest of the shard was not tested. The weekly sweep covers it."
    echo "::warning title=mutants shard incomplete::$message"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        printf '### mutants shard %s/%s: INCOMPLETE\n\n%s\n' "$((shard + 1))" "$shards" "$message" \
            >>"$GITHUB_STEP_SUMMARY"
    fi
    echo "mutants-shard: $message" >&2
    exit 0
fi
exit "$status"
