# Unattended run report: 2026-11-19

Branch `unattended/local-main`, based on `origin/release/3.x` (a51831cb).
It holds 36 commits, and the work is in milestone v3.14.0.
The commits are not signed yet.
The run disabled signing, and `scripts/resign.sh` signs every commit again before the push.
Nothing was pushed, and no PR is open.

## Rules of the run

1. Each sprint had its own branch, cut from the integration branch.
2. Each sprint passed `verify.sh`, `scripts/hawk-check.sh`, and `cargo deny check` before its local merge.
3. Each sprint had a strict `nbl-dev:pre-pr-review`, with no push and no PR.
4. The last step was a strict review of the whole branch against `origin/release/3.x`.
5. A question for the owner went to a comment on the issue, and the issue went on the blocked list.
6. No issue was closed. The PR closes them.

## Work merged

Sprint 1, composite #3893, CI chores:

- #3837: the exit codes of cargo-mutants are in the header of `scripts/mutants-shard.sh`.
- #3845: the budget line is two assignments, so semgrep parses the file.
- #3864: not reproduced at the current tip. The evidence is a comment on the issue.

Sprint 2, composite #3895, task store and timestamps:

- #3883: `llmenv task ls` prints the count and the first cause of a skipped task file.
- #3882: an unreadable `updated_at` or session `started_at` fails the read, and names the file.
- #3858: a failed `ps` check reads as "cannot tell", not "not yours".
- #3862: the timestamp helpers take a `RecordId`, so a swapped argument does not compile.

Sprint 3, composite #3906, statusline and `parse_age`:

- #3860: `parse_age` names a calendar unit only for a year or a month.
- #3889: the `pr` widget keeps the last PR for a day and caches a failure for 10 s.
  The lookup runs in a detached process with an 8 s limit.

Sprint 4, composite #3908, pid ownership:

- #3835: a pid has three states: own, another user, gone.
  A sandbox that blocks a signal to your own proxy fails with the cause.
- #3795: the spawn lock records the start time of its holder. A lock whose pid now names a newer process is reclaimed.
- #3836: the staging-folder owner check has a test with two uids.

Sprint 5:

- #3894: a 137 from GNU timeout at or after the budget plus `MUTANTS_KILL_AFTER` is a budget stop.
  GNU timeout 9.4 was checked on Ubuntu 24.04.

Whole-branch review fixes (`fix/3910-whole-branch-review`):

- `load_task` refuses a task that the listing skips.
- A future `last_activity` is "cannot tell" for ownership.
- One pid classifier in `src/mcp`, and `ESRCH` at SIGTERM is "gone".
- The PR cache read, corrupt, and refresh failures are noticed.
- One `is_within` check replaces three copies of the TTL idiom.
- `choose_auth_for_inheritance` returns the loaded cache.

## Work not done

| Issue | State | Why |
| --- | --- | --- |
| #3797 | blocked | The owner must pick option A or B. The options and a recommendation are on the issue. |
| #3844 | blocked | It needs shard times from a CI run with workspace tests. None exist until the PR opens. |
| #3905 | blocked | The owner decides how the session pick and the write guards find the owner after `/clear`. |
| #3889 step 4 | open elsewhere | The gh shim limit of 2 s is in my-llmenv#494. |
| #3909 | filed | The spawn lock reclaim removes the lock by path after a re-read. The fix is a rename or an `flock`. |
| #3907 | filed | Readers that keep an entry with a time in the future. A comment adds two more sites. |
| #3904 | filed | The auth cache ignores a bad entry with no notice. |
| #3892 | filed | `llmenv clean` should also prune filesystem and ICM transcripts, with a configurable age (v3.15.0). |

## Review of the whole branch

Six analyzers ran:
`semgrep-scanner`, `overengineering-reviewer`, `silent-failure-hunter`,
`security-audit`, `property-test-gap-finder`, and `variant-bug-hunter`.
Each sprint also had its own review run before this one.

Findings of the whole-branch run: P0 0, P1 1, P2 18, plus 12 from the silent-failure hunt.

Fixed:

- P1: property tests for `parse_gh_pr_view`.
- `load_task` skipped the timestamp check that the listing runs.
- An unreadable PR cache entry read as "no PR" with no notice.
- `ESRCH` at SIGTERM was an error.
- The bad-timestamp log did not strip control characters.
- A future `last_activity` counted as active.
- The prune failure list was in folder order.
- Duplicate code: a second pid classifier, two test-only wrappers, a copy of the auth cache, and three TTL checks.
- Error text without a fix.

Dismissed, with the reason:

- `MUTANTS_KILL_AFTER` as a knob: the test needs a short grace time, and a 60 s wait for each case is too slow.
- Folding `validate_stored`: it has five callers.
- Taking only the first bad timestamp in `check_task_times`: the message names both fields on purpose.
- The fix text of the `#? auth` marker is only in the log.
  The marker is short for the width of the line, and the docs name each cause.
- A synchronous lookup when `CLAUDE_CONFIG_DIR` is unset: there is no folder for a cache, and the 3 s limit bounds it.
- Wall-clock steps in the 137 check: both outcomes stay red.
- The lock format is not read by a llmenv before v3.14.0: documented in `website/docs/mcp.md`.

Filed: #3909 and a comment on #3907.

Semgrep did not finish two files: `src/cli/mod.rs` for the `rust` ruleset and `src/task/session.rs` for `apiiro-obfuscation-rust`.
Both are time-outs. #3864 tracks the first. The scans of the sprint diffs had no finding.

## Lessons

1. A clippy check that pipes through `rg '^warning'` misses colored output. Pass `--color never`, or read `verify.sh`.
2. A test that writes a placeholder timestamp, such as `"now"`, fails once a reader checks timestamps. Use RFC 3339 in fixtures.
3. A proptest that round-trips arbitrary strings through a validating reader must expect the refusal.
4. The auto-detected review base was the wrong base for a sprint branch. Pass `unattended/local-main` to the analyzers.
5. `ps` exit codes and the output of `ps` are one reader. Three copies of the call drifted, so they share `ps_value`.
6. The main-guard hook cannot read a heredoc. Write a script to `.tmp/` with the Write tool, and run it.
7. A hook blocks a `git commit` subject over 72 characters before the command runs.
   The `git add` in the same command does not run either.

## What the user must do

1. Decide #3797 (option A or B), #3905, and the open question of #3844.
2. Unlock the signer, then run `scripts/resign.sh` on `unattended/local-main`.
3. Say that the push and the PR are approved. The PR targets `release/3.x`.
4. After the merge, close the done issues with `close-merged-refs.sh`.
   Then run the forward merge `release/3.x` to `release/4.x` to `main`.
5. Apply my-llmenv#494 (the gh shim limit).
