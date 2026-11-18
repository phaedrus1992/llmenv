#!/usr/bin/env bash
# Usage: scripts/mutants-shard.sh <shard-index> <shards> <diff-file>
#
# Run one shard of the in-diff mutation run for a pull request (#3569). The shard runs inside a
# time budget that is shorter than the job limit. A shard that runs out of budget is reported as
# incomplete, with an error annotation, a step summary, and an INCOMPLETE marker in mutants.out,
# and exits 3. Exit 3 fails the check, so an incomplete shard is never green (#3794). Without the
# budget, the job limit cancels the shard. A mutant that survived before the stop exits 2.
#
# <shard-index> is zero-based, as `cargo mutants --shard k/n` needs. A shard that finishes exits
# with the status of cargo-mutants, so a surviving mutant still fails the check. The weekly sweep
# runs the scoped set from .cargo/mutants.toml, not this diff, and it is report-only. It does not
# cover the mutants that an incomplete shard skipped.
#
# Environment:
#   JOB_START           Unix time at the start of the job. The setup steps before this script
#                       count against the budget. Unset, the budget starts with the script.
#   MUTANTS_BUDGET      Seconds from the job start to the stop. Default 2400 (the job limit is
#                       45 minutes, and 5 minutes stay free for the artifact upload).
#   MUTANTS_MIN_BUDGET  Seconds below which a shard does not start. Default 120.
# Diagnostics go to stderr.
set -euo pipefail

usage() {
    sed -n "2,17p" "$0" | sed 's/^# \{0,1\}//' >&2
}

# A shard with less time left than this does not start. The build alone takes longer.
min_budget="${MUTANTS_MIN_BUDGET:-120}"
# Exit status of an incomplete shard. It is not 0, so the check cannot pass with untested lines.
readonly INCOMPLETE_EXIT=3

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 3 ]]; then
    echo "mutants-shard: expected <shard-index> <shards> <diff-file>" >&2
    usage
    exit 2
fi
shard="$1"
shards="$2"
diff_file="$3"
total="${MUTANTS_BUDGET:-2400}"
job_start="${JOB_START:-$(date +%s)}"
for value in "$shard" "$shards" "$total" "$job_start" "$min_budget"; do
    if ! [[ "$value" =~ ^[0-9]{1,12}$ ]]; then
        echo "mutants-shard: '$value' is not a number of at most 12 digits" >&2
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
if ! command -v timeout >/dev/null; then
    echo "mutants-shard: GNU timeout is missing. Run on Linux, or install coreutils." >&2
    exit 2
fi

budget=$((total - ($(date +%s) - job_start)))

report_incomplete() {
    local message="$1"
    mkdir -p mutants.out
    printf 'INCOMPLETE shard %s/%s\n%s\n' "$((shard + 1))" "$shards" "$message" >mutants.out/INCOMPLETE
    echo "::error title=mutants shard incomplete::$message"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        printf '### mutants shard %s/%s: INCOMPLETE\n\n%s\n' "$((shard + 1))" "$shards" "$message" \
            >>"$GITHUB_STEP_SUMMARY"
    fi
    echo "mutants-shard: $message" >&2
}

if ((budget < min_budget)); then
    report_incomplete "mutants shard $((shard + 1))/$shards did not start: only ${budget} s of the \
${total} s budget were left after the setup steps. Its mutants were not tested. The weekly sweep \
runs the scoped set, not this diff, and it is report-only."
    exit "$INCOMPLETE_EXIT"
fi

status=0
# --test-tool nextest stops a mutant at its first failing test. --in-place reuses the cached
# target directory of the checkout, which is disposable in CI. -k sends KILL 60 s after the TERM,
# so a child that ignores TERM cannot hold the shard until the job limit.
timeout -k 60 "$budget" cargo mutants --workspace --in-place --test-tool nextest \
    --in-diff "$diff_file" --shard "$shard/$shards" || status=$?

# 124 is the only budget stop. A kill (137) is a failure, not a stop: the kernel's memory kill
# and a KILL after an ignored TERM both exit 137, and neither may pass as a stop.
if [[ "$status" -eq 124 ]]; then
    # A mutant that survived before the stop is a finding, so the shard still fails.
    if [[ -s mutants.out/missed.txt ]]; then
        echo "mutants-shard: shard $((shard + 1))/$shards ran out of budget, and it had already" \
            "missed $(grep -c . mutants.out/missed.txt) mutant(s):" >&2
        cat mutants.out/missed.txt >&2
        exit 2
    fi
    report_incomplete "mutants shard $((shard + 1))/$shards ran out of its ${budget} s budget and \
is incomplete. No survivor was recorded before the stop, and the rest of the shard was not tested. \
The weekly sweep runs the scoped set, not this diff, and it is report-only."
    exit "$INCOMPLETE_EXIT"
fi
if [[ "$status" -ne 0 ]]; then
    echo "mutants-shard: cargo mutants exited $status. A kill is a failure, not a budget stop." >&2
fi
exit "$status"
