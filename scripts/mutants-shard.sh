#!/usr/bin/env bash
# Usage: scripts/mutants-shard.sh <shard-index> <shards> <diff-file>
#
# Run one shard of the in-diff mutation run for a pull request (#3569). The shard runs inside a
# time budget that is shorter than the job limit. A shard that runs out of budget is reported as
# incomplete, with an error annotation, a step summary, and an INCOMPLETE marker in mutants.out,
# and exits 10. Exit 10 fails the check, so an incomplete shard is never green (#3794). Without the
# budget, the job limit cancels the shard. A mutant that survived before the stop exits 2. A
# setup error exits 11.
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
    sed -n '2,${/^#/!q;p;}' "$0" | sed 's/^# \{0,1\}//' >&2
}

# A shard with less time left than this does not start. The build alone takes longer.
min_budget="${MUTANTS_MIN_BUDGET:-120}"
# Exit codes of cargo-mutants, from https://mutants.rs/exit-codes.html (#3837): 0 all caught,
# 1 usage error, 2 a mutant was not caught, 3 a test timed out, 4 the baseline failed, 5 the
# --in-diff diff does not match the tree, 6 the --in-diff input is not a diff, 70 internal error.
# Exit status of an incomplete shard. It is not 0, so the check cannot pass with untested lines.
# It is outside the codes above, so it never reads as a survivor or a timeout.
readonly INCOMPLETE_EXIT=10
# Exit status of a setup error, such as a bad argument or a missing diff file. It is outside the
# codes above and outside INCOMPLETE_EXIT, so a setup failure never reads as a finding, a timeout,
# or an incomplete shard (#3814).
readonly SETUP_EXIT=11

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 3 ]]; then
    echo "mutants-shard: expected <shard-index> <shards> <diff-file>" >&2
    usage
    exit "$SETUP_EXIT"
fi
shard="$1"
shards="$2"
diff_file="$3"
total="${MUTANTS_BUDGET:-2400}"
if [[ -n "${JOB_START:-}" ]]; then
    job_start="$JOB_START"
else
    job_start="$(date +%s)" || {
        echo "mutants-shard: cannot read the clock with date. Set JOB_START to a Unix time." >&2
        exit "$SETUP_EXIT"
    }
fi
for value in "$shard" "$shards" "$total" "$job_start" "$min_budget"; do
    # A leading zero is refused because bash reads it as octal in the arithmetic below.
    if ! [[ "$value" =~ ^(0|[1-9][0-9]{0,11})$ ]]; then
        echo "mutants-shard: '$value' is not a number of at most 12 digits without a leading zero" >&2
        exit "$SETUP_EXIT"
    fi
done
if ((shard >= shards)); then
    echo "mutants-shard: shard index $shard must be below the shard count $shards" >&2
    exit "$SETUP_EXIT"
fi
if [[ ! -f "$diff_file" ]]; then
    echo "mutants-shard: diff file '$diff_file' does not exist" >&2
    exit "$SETUP_EXIT"
fi
if ! command -v timeout >/dev/null; then
    echo "mutants-shard: GNU timeout is missing. Run on Linux, or install coreutils." >&2
    exit "$SETUP_EXIT"
fi

# The semgrep bash grammar cannot parse a command substitution inside arithmetic (#3845).
now="$(date +%s)" || {
    echo "mutants-shard: cannot read the clock with date." >&2
    exit "$SETUP_EXIT"
}
if ((job_start > now)); then
    echo "mutants-shard: JOB_START $job_start is later than the clock $now. Fix the value." >&2
    exit "$SETUP_EXIT"
fi
elapsed=$((now - job_start))
budget=$((total - elapsed))

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

# timeout reads a limit of 0 as no limit, so a budget below 1 never starts the shard.
if ((budget < min_budget || budget < 1)); then
    report_incomplete "mutants shard $((shard + 1))/$shards did not start: only ${budget} s of the \
${total} s budget were left after the setup steps. Its mutants were not tested. The weekly sweep \
runs the scoped set, not this diff, and it is report-only."
    exit "$INCOMPLETE_EXIT"
fi

status=0
# --test-tool nextest stops a mutant at its first failing test. --in-place reuses the cached
# target directory of the checkout, which is disposable in CI. -k sends KILL 60 s after the TERM,
# so a child that ignores TERM cannot hold the shard until the job limit.
# --test-workspace runs the tests of every crate, as the CI test job does. Without it, a mutant
# is judged by its own package's tests only, so a survivor can be a false miss (#3809).
timeout -k 60 "$budget" cargo mutants --workspace --test-workspace=true --in-place \
    --test-tool nextest --in-diff "$diff_file" --shard "$shard/$shards" || status=$?

# Exit 124 is a budget stop. GNU timeout 9.4 exits 137 instead when cargo mutants ignores the TERM
# and the KILL of -k ends it (checked on Ubuntu 24.04, #3894). The kernel's memory kill also exits
# 137. The run time tells them apart: a 137 at or after the budget is a stop, because TERM was
# sent at the budget. A 137 before the budget is a failure.
finished="$(date +%s)" || finished=0
stopped=no
if [[ "$status" -eq 124 ]]; then
    stopped=yes
elif [[ "$status" -eq 137 && "$finished" -gt 0 ]] && ((finished - now >= budget)); then
    stopped=yes
fi
if [[ "$stopped" == yes ]]; then
    stop_message="mutants shard $((shard + 1))/$shards ran out of time, and the shard had ${budget} s. \
The rest of the shard was not tested. The weekly sweep runs the scoped set, not this diff, and it \
is report-only."
    # The incomplete report is written in both cases, so the gap is visible even with survivors.
    # cargo-mutants is assumed to write missed.txt before the TERM stops it. That is not verified
    # (#3814). When the file is absent, the shard still fails as incomplete, so no survivor passes.
    if [[ -s mutants.out/missed.txt ]]; then
        echo "mutants-shard: shard $((shard + 1))/$shards ran out of time, and it had already" \
            "missed $(grep -c . mutants.out/missed.txt) mutant(s):" >&2
        cat mutants.out/missed.txt >&2
        report_incomplete "$stop_message"
        # A mutant that survived before the stop is a finding, so the shard still fails.
        exit 2
    fi
    report_incomplete "$stop_message"
    exit "$INCOMPLETE_EXIT"
fi
if [[ "$status" -eq 137 ]]; then
    echo "mutants-shard: cargo mutants was killed (exit 137). A kill is a failure, not a budget stop." >&2
elif [[ "$status" -ne 0 ]]; then
    echo "mutants-shard: cargo mutants exited $status. See the output above for the cause." >&2
fi
exit "$status"
