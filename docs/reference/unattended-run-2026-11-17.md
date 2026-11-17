# Unattended run: 2026-11-17

Branch `unattended/local-main`, base `release/3.x`, milestone v3.14.0.
The branch holds 76 commits and 18 merge commits on top of `origin/release/3.x`.
The commits are unsigned until `scripts/resign.sh` runs.
Nothing was pushed during the run.

## Rules of the run

- One branch for each sprint, merged locally into the integration branch after the full gate.
- The gate is verify (build, clippy, tests), hawk, and cargo deny, run as if for a push.
- The only review stage is `nbl-dev:pre-pr-review` in strict mode. Every finding was fixed or filed as an issue.
- A question for the owner became a comment on the issue and a block in the open-issues state.
- No signing workaround and no push.

## Work merged

Task store reads and ownership:

- One store read for each command (#3485, #3482, #3742, #3743, #3747, #3748).
- Ownership in every writer and session choice (#3587, #3617, #3619, #3590, #3602, #3603, #3751, #3753).
- Audit notes after the change, and a journal that survives a crash (#3755, #3762, #3781, #3782, #3783, #3784).

Task reminders and next-task logic:

- One startability check for the reminders and `task start`
  (#3757, #3763, #3764, #3770, #3772, #3773, #3776, #3777, #3779, #3780).
- Blocker walk and ancestry gaps (#3618, #3756).
- Partial-failure reports (#3749, #3750, #3759, #3761).

Statusline and hooks:

- One fallback rule for zone failures, and the cause of an unreadable size (#3071, #3110, #3744).
- Hook event name fallbacks and notices (#3752, #3760, #3766, #3769).
- Read-once also compares the file size (#3226).
- Bounded reads do not follow links (#3785).

Tests, CI, and chores:

- Log-capture races, a flaky keychain test, and test batches (#3444, #3483, #3771).
- Mutation shards with a time budget (#3569), and job timeouts (#3778).
- `semver` crate, CRAP splits, and review follow-ups (#3510, #3595, #3226).
- Bug batches (#3754, #3758, #3767, #3768, #3746).

## Work not done

| Issue | State | Why |
| --- | --- | --- |
| #3765 | blocked | Owner decision: exit status when an audit note fails after a saved change. |
| #3745 | blocked | Owner decision: the #3071 marker rule for non-zone widget failures. |
| #3592 | blocked | The fix lives in the applied-ai plugin. |
| #3584 | blocked | The issue has no body. Questions posted on the trust model for a `command` source. |
| #3487 | blocked | Owner decision: how a second conversation may take over an owned session. |
| #3480 | blocked | Item 5 waits on #3487. Items 1 to 4 and 6 are fixed. |
| #3330, #3083 | blocked | Owner decision: unknown state for an unreadable working directory. |
| #3132 | blocked | Needs a GitHub ruleset change. |
| #3092, #3085 | blocked | Owner decision: a pinned vitest toolchain for `plugin.js`. |

## Review of the whole branch

Each sprint ran semgrep and the silent-failure hunter.
By diff content it also ran these analyzers:
the overengineering reviewer, the security audit, the property-test finder, the variant hunter, and crap-score.
Every finding was fixed, dismissed with a reason on the issue or in the commit, or filed.
Filed from reviews: #3776 to #3778, #3780, #3782, #3783, #3785.
Two findings were dismissed with a reason.
The label for a cross-session parent stays, because the start refuses that case whatever the other session does.
The advice to start a parked task after an answer stays, because it is conditional on a future state.
The semgrep tob-rust ruleset timed out on `src/task/mod.rs` in two sprints. The other seven rulesets covered it.

## Lessons

- A property test of a cache decision found a real bug: read-once ignored the file size.
- `custom_flags` replaces earlier flags, so a second call can drop `O_NOFOLLOW`.
- A process id is a weak lock. A file lock that the system releases at death needs no id.
- The commit gate needs a task in progress started in its own Bash call.
- `sprint-merge.sh` must run on the integration branch, and it refuses a dirty tree.
  Do not mark an issue done before the merge prints its SHA.
- `.tmp/gate2.sh` can leave `gate.rc` at 0 after a failed run. Read `.tmp/verify.log`.

## What the user must do

1. Decide the blocked issues in the table above.
2. Unlock the signer when asked, so `scripts/resign.sh` can sign the branch.
3. Check the PR and its `closingIssuesReferences` before the merge.
