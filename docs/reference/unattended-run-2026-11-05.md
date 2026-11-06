# Unattended run report: 2026-11-05

Branch `unattended/local-main`, based on `release/3.x`.
The integration branch stands in for `release/3.x` during the run.
Scope: the open issues of milestone v3.14.0.
The branch holds 148 commits above `release/3.x`: 22 sprint merges, the sprint commits, and the wrap-up review fixes.
The signer locked during the run, so some commits are unsigned.
`resign.sh` signs all commits again before the push.

## Rules of the run

- One branch for each sprint, created before its first commit.
  Each branch merged into `unattended/local-main` with `sprint-merge.sh`.
- No push, no PR, and no closed issue during the run. This PR closes the finished issues.
- Each sprint ran the full local review before its merge: `nbl-dev:pre-pr-review` in strict mode, with Semgrep.
- Each sprint ran the full gate before its merge.
- A module budget hit was paid with an extraction in the same change, never with a raised cap.
- An issue that needs an owner decision got a comment with the options and a recommendation, and was marked blocked.
- Every deferral is a filed issue.
- No feature in this run changes a number the user relies on, so no feature ships behind an off switch.

## Work merged

### Task tracker

- #3478 and #3479: the TaskUpdate redirect checks the owner of a task.
  The ownership check and the change run under one re-entrant store lock.
  `session edit`, `session note`, and `session finish` refuse a session of another conversation.
  `--other-session` names the owner and overrides the refusal.
- #3477, #3474, #3471, #3468, #3465, #3462: one store read for each command, hook run, and redirect.
  Strict reads keep an unreadable store from reading as an empty one.
  A finished job checkpoint that stays gives a notice.
- #3475, #3469: property tests for the tracking fields, and a type for the strict read.
- #3472, #3466, #3463: the Stop hook, the reminders, and `task reopen` read each store once.

### Hooks, notices, and state

- #3457 and #3447: one manifest parse, and fewer state reads.
- #3449 and #3445: log-only failures now log with their cause, and state files are cut by age.
- #3398 and #3399: detached jobs leave a notice when they fail, and silent hook paths say why.
- #3441, #3087, #3386: `gc` names the cause of a failure and shares its readers.
- #3442, #3270, #3269, #3130, #3122: `gh` exit status, scope disables, and state cut-offs.
- #3433, #3432, #3394, #3277, #3440: git exit codes, the age rule gate, and the `gc` sweep.
- #3439, #3103, #3187: the resume notice and the ownerless-session reminder.
- #3438, #3101, #3421: `onFailure` guards and a JSON memory list.
- #3434, #3423, #3419, #3428, #3390, #3389: `gc` races, the recall budget, and hook fixes.

### Features, config, and tests

- #3418, #3371, #3368, #3369: JSON recall from ICM, dedup by id, and an ICM 0.11.5 floor.
- #3102: `llmenv prefill` report.
- #3431, #3393, #3041, #3280, #3064, #3055: property test coverage.
- #3429, #3060, #3392: hook program errors and the rendered check.
- #3426, #3322, #3323, #3288: `validate.rs` split, and changelog tags.
- #3422, #3376, #3350, #3374: one sweep clock, and the prune I/O error.

The full list of 76 finished issues is in `.git/unattended/done.txt` before the PR, and in the PR body after it.

## Work not done

| Issue | State | Why |
| --- | --- | --- |
| #3391 | Blocked | Owner picks close, default-path check, or dirfd for a symlinked parent of the session log. |
| #3370 | Blocked | The ICM queries share one filter set, so a merge changes recall. Owner picks. |
| #3227 | Blocked | The fix belongs in `netboxlabs/applied-ai`, not in llmenv. |
| #3132 | Blocked | GitHub ruleset setting. Only the owner can change it. |
| #3110 | Blocked | Owner picks the payload or the log for the cause of an unreadable size. |
| #3105 | Blocked | Owner picks proxy reliability, a stdio session entry, or in-process ICM. Analysis is on the issue. |
| #3071 | Blocked | Owner picks one fallback rule for zone failures across status widgets. |
| #3485, #3482 | Open | The user asked to wrap up after sprint #3478. The commands read the store twice. |
| #3486, #3487, #3480, #3481 | Open | Ownership gaps of the sprint review. #3487 needs an owner decision. |
| #3444, #3483 | Open | Flaky tests: log capture under parallel runs, and the keychain worker-lost test. |
| #3092, #3085 | Open | Test coverage work. |
| #3330, #3083, #3226 | Open | Scope work on an unreadable working directory, and review follow-ups. |

## Review of the whole branch

The review ran in strict mode over `origin/release/3.x...HEAD` (246 files).
It ran Semgrep once, and three analyzers in four area passes.
The areas are the task tracker, hooks and adapters, materialize and memory, and the rest.
The over-engineering and property-test analyzers ran once over the whole branch.

- Findings: 2 P1 and 34 P2 from the analyzers, and about 35 more from the silent-failure passes.
- Fixed in the review commit:
  the bundle origin in the hook dedup guard, a false opencode warning for a plain command,
  three notices that were log-only, a settings read that a repo could block,
  a `gc` and a `prune` report that disagreed, a NotFound race in the sweep,
  a partial task delete with no cause, a lost `task done` output, and the transcript migration marker.
  It also adds property tests for the prune importance rule, the expiry rule, and the transcript folder name.
- Filed as issues: #3480 (items 3 to 6), #3486, #3487, #3505 (task), #3506 (hooks), #3507 (notices),
  #3508 (state reads), #3509 (cache and doctor), and #3510 (semver crate).
- Dismissed with a reason: `format_age` stays because a property test pins it.
  The three `current_exe` hits and the `temp_dir` hit of Semgrep name the running binary and the temp roots.
  They are not trust decisions.
  `canonicalize` in the store lock stays.
  The macOS `/var` and `/private/var` spellings of one folder would deadlock one thread.
- Semgrep coverage is incomplete.
  The `rust` ruleset and three other Rust rulesets timed out on 16 files and failed to parse them.
  Examples are `src/cli/mod.rs`, `src/task/mod.rs`, `src/hook_run/mod.rs`, and `src/adapter/claude_code/mod.rs`.
  The area passes read those files instead.

## Lessons

- `rust-budget.sh --changed` reads only staged changes and exits 0 on growth.
  Stage the files before the check, and check the whole range at wrap-up.
- `bash-command-guard` blocks any command text that holds the word "clone", also `.clone()` in a Python heredoc.
  Use the Edit tool.
- Do not edit the tree while a background gate runs.
- A failed signed commit needs one retry, then a pause for the user.
- The store lock must be re-entrant per thread and per store, or a command cannot check and change under one lock.
- The review of one sprint finds gaps in the area of the next sprint.
  Ownership gaps in session commands came out of the review of #3478.

## What the user must do

1. Unlock Bitwarden, so the signer works.
2. Decide the seven blocked issues. The options and the recommendations are on each issue.
3. Decide #3487 (`--resume` claim and `--replace`), which is item 5 of #3480.
4. Read the PR, and merge it when the checks pass.
5. Merge the two new `release/3.x` commits (jiff 0.2.39) into the branch if the push says the base moved.
