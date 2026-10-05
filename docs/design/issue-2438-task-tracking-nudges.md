# Issue #2438 — make agents track multi-step work while it happens

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2438
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature, split into three changes
- **Related:** #985, #980 (redirect), #2338 (Stop reminder), #2339 (resume context), #2416 (refuse `done` on an unstarted task)

## Problem

An agent ran a multi-step sprint with no task tracking until the end.
The only reminders fire at SessionStart and Stop.
The task model cannot express "this step is made of these steps", so a rule such as "do not finish the parent before its parts" has nothing to attach to.
The issue body holds the full account and the owner's requirements.
This document fixes the design, the split, and the order.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `Task` has `parent: Option<String>`, `blocked_on`, `session`, `state` (`open`, `wip`, `waiting`, `done`). `parent` has no meaning beyond display | `src/task/mod.rs` |
| `add` without `--parent` uses `ParentSpec::Auto`: the new task becomes a child of the most recently created task of the session (#929) | `add_task_for_session_with` |
| `start_task` checks `blocked_on` only. `parent_soft_block_warning` prints a note for any undone parent, so the chain warns on nearly every start | `start_task`, `parent_soft_block_warning` |
| `complete_task` refuses an `open` task unless `--force` (#2416). It ignores children | `complete_task` |
| `idle_sessions` returns a session only when it has an `open` task and no `wip` or `waiting` task. A session with zero tasks is silent | `idle_sessions` |
| SessionStart and Stop reminders: `session_start_reminder`, `stop_hook_reminder` | `src/task/mod.rs` |
| `TaskCreate`, `TaskList`, `TaskUpdate` are redirected to `llmenv task` by a PreToolUse hook | `src/hook_run/task_tools.rs` |
| `features.task_tracker` has `enabled` and `block_engine_task_tools`. There is no key for nudges | `crates/llmenv-config/src/schema.rs` |
| The example base bundle says the engine task tools are "blocked" | `examples/config-llmenv-dir/bundles/base/AGENTS.md` |

## The split

1. **Change A — relations and completion rules.** The `relation` field, `--child-of`, `--parallel`, `--after`, the queue start rule, the parent completion rule, `session start --task`, and reminder grouping. Issue: #2455.
2. **Change B — reminders and nudges (#2456).** The `Skill` reminder, the mid-work nudge, the zero-task session report, the deny-once on `git commit` and `gh pr create`, the waiting reminder, and the two config switches.
3. **Change C — core instruction text and checks (#2457).** The text that SessionStart injects, the doctor check for contradicting text, the example `AGENTS.md` wording, and the release check for the #2416 behavior.

A comes first: B and C refer to the rules that A adds.

## Decisions

1. **Two relations.** `relation` is `queued` (default) or `child`.
   A stored task without the field loads as `queued`.
   Only `--child-of` writes `child`.
   `parent` stays and keeps the tree display for both.
2. **The queue is computed, not stored.**
   The queue of a session is its `queued` tasks that are not `parallel`, ordered by `created_at`, then by slug.
   The predecessor of a task is the previous task in that order.
   A stored predecessor would need repair when a task is deleted or reordered.
3. **Queue start rule.** `llmenv task start <t>` on an `open` task refuses while the predecessor is not `done` or `waiting`, or while another queued, non-parallel task of the session is `wip`.
   `--force` bypasses it.
   A task that is `waiting` resumes without the check: it already passed the rule once.
4. **Children.** `add --child-of <p>` sets `parent = p`, `relation = child`.
   Children are parallel by default.
   `--after <sibling>` writes a `blocked_on` edge, which `start` already enforces.
   Starting a child moves an `open` parent to `wip`.
   `--child-of` conflicts with `--parent`, `--no-parent`, and `--parallel`.
   A child cannot be added to a `done` parent.
5. **No implicit chain.** `ParentSpec::Auto` no longer sets a parent.
   `--parent` stays as a display link with `relation = queued`.
   `parent_soft_block_warning` is removed: the hard rules replace it, and it warned on every start.
6. **Parent completion rule.** `done <parent>` refuses while any descendant (a task of `relation = child` that reaches the parent through `parent` links) is not `done`.
   The error lists them.
   `--force` is the only bypass, and the CLI prints a note like the existing "never started" note.
7. **Derived parent state.** A parent is `wip` while any child is `wip`.
   A parent is `waiting` only when every open child is `waiting`.
   Both are computed by the reminders from the children, and not stored, except that starting a child persists `open` → `wip` on the parent.
8. **`session start --task <title>`** (repeatable) creates the session and its first tasks in one locked call, so creation and registration cannot be separated.
9. **Nudge counting (B).** A PostToolUse hook for `Bash`, `Edit`, `Write` increments a per-session counter in the state dir.
   A nudge fires at the Nth call (default 8) when the project has no open session, or an open session with zero tasks, and then every Mth call (default 20).
   The counter resets when a task exists.
10. **Deny-once (B).** A PreToolUse hook on `Bash` commands that start with `git commit` or `gh pr create` denies the first call when the project has no `wip` task, with the exact commands.
    A marker file for the session allows the retry.
    Matching uses the command words, not a regex over the whole line, so `git commit` inside a quoted string does not match.
11. **Waiting reminder (B).** After `AskUserQuestion`, and at Stop when the last turn ends in a question while a task is `wip`, the reminder names `llmenv task wait <slug> "<reason>"` and the later `llmenv task start <slug>`.
    A `waiting` task is reported as "waiting on the user" and is never idle.
12. **Core text (C).** `session_start_reminder` always carries a fixed statement of the four behaviors and the commands when `features.task_tracker.enabled` is true.
    The text lives in the llmenv source, with no dependency on a bundle.
13. **Switches.** `features.task_tracker.nudges` (default true) turns off the injected text and the nudges.
    `features.task_tracker.enforce_commit` (default true) turns off the deny-once.
    Both can be set per project through the existing tag-scoped `task_tracker` entries.
14. **Doctor (C).** `llmenv doctor` warns when an instruction file in scope says the engine task tools are "blocked", or forbids `llmenv task`.
15. **Release check (C).** A test runs `complete_task` on an unstarted task and expects the #2416 refusal. A CI step compares `llmenv --version` of the built binary with the source, so a stale alpha cannot hide it.

## Implemented as

- `ParentSpec::Auto` is replaced by `ParentSpec::Detached` (no parent).
  Queue, parallel, and sub-task placement is the `Placement` enum (`Queue`, `Parallel`, `Child`) in `src/task/relation.rs`.
- The nudge counter in `src/hook_run/task_nudge.rs` counts `Bash`, `Edit`, `Write` and `MultiEdit`.
  Counters live in `state_dir/task_nudge/{session_id}.json`.
- `features.task_tracker` has three more optional keys: `workflow_skills` (the skills that trigger the `Skill` reminder; default `dev-sprint`, `ship-issue`, `pre-pr-review`, `executing-plans`, `writing-plans`), `nudge_after` (default 8) and `nudge_every` (default 20).
- The doctor check for contradicting text is `src/cli/doctor/task_text.rs`.
- The example base bundle no longer says the engine task tools are "blocked", so that row of "Verified facts" is out of date.
- Decision 15 is built: the `complete_task` refusal has tests, and `scripts/check-binary-version.sh` runs in the `test` job to compare `llmenv --version` of the built binary with the source.

## Tests

- A: parent with three children, all started, all `wip`; `done` on the parent refuses with two done and one `wip`, then succeeds; two queued tasks, `start` of the second refuses until the first is `done` or `waiting`, `--parallel` removes the refusal; the chain no longer warns; the `pre-pr-review` shape (one parent, six children, a summary task queued after the parent) runs end to end.
- B: for each of the four behaviors, a test in `tests/` drives the hook with a sequence of tool calls and checks the injected reminder or the deny; the first `git commit` with no task in progress is denied, the retry is allowed, and the switch disables the deny.
- C: SessionStart with an empty user config and the tracker enabled outputs the four behaviors and the commands; doctor flags a file with the word "blocked".

## Acceptance

The acceptance list of #2438 holds for the three changes together.

## Out of scope

- The wording of the personal config (tracked in phaedrus1992/my-llmenv#74).
- A task relation across sessions.
