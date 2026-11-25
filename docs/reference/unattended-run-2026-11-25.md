# Unattended run report: 2026-11-25

Branch `unattended/local-main`, based on `origin/release/3.x` (21daa7d3).
It holds 32 commits and 2 sprint merge commits, and the work is in milestone v3.14.2.
The commits are not signed yet.
The run disabled signing, and `scripts/resign.sh` signs every commit again before the push.
Nothing was pushed, and no PR is open.

## Rules of the run

1. Each sprint had its own branch, cut from the integration branch.
2. Each sprint passed the repo gate before its local merge.
   The gate is `verify`, `scripts/hawk-check.sh`, `cargo deny check`, and the pre-push hooks.
3. Each sprint had a strict `nbl-dev:pre-pr-review`, with no push and no PR.
4. The last step was a strict review of the whole branch against `origin/release/3.x`.
5. A question for the owner went to a comment on the issue, and the issue went on the blocked list.
6. No issue was closed. The PR closes them.

## Work merged

Sprint 1, composite #4036, ICM recall latency and the Stop hook:

- #4023: the recall calls of a session start, a prompt, and a subagent start share a time budget.
- #4022: a call that the budget cuts before it starts does not count in the recall breaker.
- #3897: `features.memory.failure_recall` skips the recall on a tool failure.
- #2909: the hook processes of one memory server share one MCP session id.
- #2473: the idle Stop reminder leads with a task wait when the last message asks the user.
- #4037: a budget cut is neither a success nor a failure on every recall path.

Sprint 2, composite #4039, bug fixes for the hook, doctor, auth, and the index job:

- #3914: the detached index job takes a lock per project, and a second job stops.
- #3912: the doctor shows an old failing recall ring as info.
- #3907: a stored time in the future counts as expired in every TTL check.
- #3904: a credential file or a dead-folder marker that llmenv ignores gives a notice.
- #3357: the notices of a hook output stay under the Claude Code cut on every output path.

## Work not done

| Issue | State | Why |
| --- | --- | --- |
| #4035 | Blocked | A fix needs the live still host and a latency measurement under load. Options are on the issue. |
| #3911 | Blocked | The usage sample window needs an owner decision. Options are on the issue. |
| #3633 | Blocked | The icm reply format needs the icm source, which is not cached. Options are on the issue. |

## Review of the whole branch

Two whole-branch passes ran, each with four analyzers:
overengineering-reviewer, property-test-gap-finder, security-audit, and variant-bug-hunter.
The semgrep scanner and the repo gate ran per sprint.

| Pass | P0 | P1 | P2 |
| --- | --- | --- | --- |
| 1 | 0 | 1 | 9 |
| 2 | 0 | 1 | 12 |

Fixed:

- The notices cap was missing on some hook output paths. Fixed in `aa27bc48` and `5d04077a`.
- A budget cut counted as a failure on some paths and not on others (#4037). Fixed in `afca71c0`.
- Time helpers were duplicated in the session cache, the recall breaker, and the index lock. They use one helper now.
- A failed holder write left a stale process id in the index lock file, and the holder read followed a link. Both are fixed.
- A transcript marker from the future was never stale. It is stale now.
- The statusline dropped the cause of a failed codebase-memory count. It logs the cause now.
- Property tests cover `hung_notice`, the doctor `age` text, the TTL window, and the session cache.

Dismissed, with the reason:

- The CLI memory commands do not use the session cache. They are not hook paths, and the finding named no change.

Filed as issues by the review:

- #4040: the detached store, record, and consolidation jobs run twice on one checkpoint with no lock.
- #4041: the detached consolidation recall bypasses the recall breaker.
- #4042: the session cache id is keyed by the URL only.
- #4043: the doctor and task displays show 0 for a future timestamp.

Mutation runs covered the idle reminder, the recall deadline, and the session cache. No mutant survived in the idle run.

## Lessons

1. `tokio::time::timeout_at` polls the inner future first, so a call that has already finished is never cut.
2. An expired budget before a call starts must return a cut without a call, and the breaker must not see it.
3. A shared time helper removes a whole class of clock bugs. Write the helper first.
4. A cargo-mutants run needs `--no-config` and an output folder that exists.
5. A commit gate reads the task state before the command, so start the task in its own call.

## What the user must do

1. Read the version tags in `website/docs/`.
   #3896 and #3899 docs use v3.15.0, and this sprint uses v3.14.2.
   Reconcile them at the release.
2. Decide the options on #4035, #3911, and #3633.
3. Unlock the signer, so the run can sign the commits again.
4. Give the go-ahead for the push and the one PR.
