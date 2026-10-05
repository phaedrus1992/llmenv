# Issue #2416 — refuse `task done` on a never-started task and `session finish` with open tasks

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2416
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix with a behavior change (two commands refuse where they used to warn or succeed)
- **Related:** #2338 (the never-started warning this replaces), #2339 (`--detail` on tasks, already shipped)

This is a spec, not a plan.

## Problem

An agent closed a sprint with a stub implementation and a green task list, and the tracker let it.
Three gaps made that cheap:

1. `llmenv task done <slug>` on a task that was never started only prints a note and exits 0.
2. `llmenv task session finish` succeeds with open, in-progress, or waiting tasks and prints `(5/11 done)`.
3. The Stop reminder for an idle session offers `llmenv task session finish <id>` as a way out while tasks are still open.

A `done` without a `start` means no work was tracked.
A session with open tasks is not finished.
The tracker should say so with a non-zero exit, and offer an explicit override for the rare real case.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `TaskState` is `Open` (default), `Wip`, `Waiting`, `Done`; `as_str` gives `open`, `wip`, `waiting`, `done` | `src/task/mod.rs` |
| `complete_task(state_dir, input)` sets `Done` from any state under `with_store_lock` and returns `Completed { task, prior }`; `Completed::never_started_warning()` builds the #2338 note | `src/task/mod.rs` |
| `start_task(state_dir, input, force: bool)` refuses on an unmet `blocked_on` unless `force`, with the text `(pass --force to start anyway)` | `src/task/mod.rs` |
| `finish_session(state_dir, id)` takes the store lock, bails if the session is not open, stamps `finished_at`; `session_progress` returns `(done, total)` | `src/task/session.rs` |
| `abandon_session` (the `--replace` path) untags incomplete tasks and adds an "orphaned" note to each | `src/task/session.rs` |
| Stop text: `stop_hook_reminder` joins `wip_reminder`, `session_finish_reminders`, `idle_session_reminders`, and `session::missing_context_reminders`; the offending sentence is in `idle_reminder_lines`, driven by `idle_sessions()` → `IdleSession { session, next, open_count }`; `finish_reminder_lines` holds the "all N tasks are done" text | `src/task/mod.rs` |
| Hook-run caller: `resolve_stop_reminder(state_dir, session_id, config)` | `src/hook_run/mod.rs` |
| Clap: `TaskCommand::Done { id }` and `TaskSessionCommand::Finish { id }`; handlers `run_task_command` and `run_task_session_command` are thin formatting layers over `crate::task` | `src/cli/mod.rs` |
| Second caller of `complete_task`: the native-tool redirect. `todowrite` handles status `completed` (and already detects "added and completed in one call"); `update` handles `Some("completed")` | `src/hook_run/task_tools.rs` |
| `done_task` is a `#[cfg(test)]` wrapper around `complete_task`; many unit tests complete a task straight from `open` to build fixtures | `src/task/mod.rs`, `src/task/session.rs` |
| Integration tests use `assert_cmd` through `support::isolated_llmenv_cmd`; existing cases `done_without_start_warns_and_reopen_restarts_it`, `session_finish_by_id_closes_it_out`, `session_finish_auto_resolves_when_exactly_one_open` | `tests/task_cli.rs` |
| Docs: `## task` in `website/docs/commands.md` (synopsis block plus per-command bullets, `### Task sessions (#905)`, `### Resume context (added in v3.12.0)`); agent-facing reference `skills/llmenv/references/task-tracker.md` | docs |
| `Task.detail: Option<String>` and `--detail` / `--detail-file` exist (#2339) | `src/task/mod.rs`, `src/cli/mod.rs` |

## Decisions

1. **`done` on an `Open` task is an error, not a warning.**
   The store function gets a `force: bool` parameter, the same shape as `start_task`.
   Without `force`, an `Open` task returns `Err` with the fix: start it, or pass `--force` when the work really is done without tracking.
   `Wip` and `Waiting` complete as today.
   `Done` stays idempotent.
2. **The native-tool redirect never forces.**
   `todowrite` and `update` pass `force = false`.
   A refusal becomes the tool result text so the agent reads the fix.
   The one existing exception stays: a task added and completed in the same `TodoWrite` call is not a skipped start (that path already exists in `todowrite`).
3. **`session finish` refuses with unfinished tasks.**
   Any task in the session with state `Open`, `Wip`, or `Waiting` blocks the finish.
   The error lists each one as `<state> <slug> <title>` and names both ways out: finish the tasks, or `--abandon-open`.
4. **`--abandon-open` reuses the abandon path.**
   It does what `abandon_session` already does for `--replace`: untag the unfinished tasks from the session and add a note `abandoned when session <id> finished`.
   Do not write a second untag loop; extract the shared part of `abandon_session` if needed.
5. **The Stop reminder offers `session finish` only when nothing is open.**
   `idle_reminder_lines` stops naming `session finish` when `open_count > 0`.
   With open tasks it says to start the next task or finish the open ones.
   `finish_reminder_lines` (all tasks done) keeps naming `session finish`.
6. **`task clear` is the per-task alternative** and needs no change; the `session finish` error names it as the way to drop one task.
7. **Test fixtures move to a start-then-done helper.**
   `done_task` in tests keeps completing from `open` by passing `force = true`, so existing fixtures compile with a one-line change.
   New tests for this issue use the real path.

## Design

### Store layer (`src/task/mod.rs`, `src/task/session.rs`)

- `complete_task(state_dir, input, force: bool) -> Result<Completed>`.
  When `prior == Open && !force`: `bail!` with
  `'<slug>' was never started (open). Run llmenv task start <slug> and finish the work, or pass --force if it is done.`
  `Completed::never_started_warning()` is removed; the forced path returns a `Completed` whose `prior` is `Open`, and the CLI prints a one-line note that the start was skipped.
- `finish_session(state_dir, id, abandon_open: bool) -> Result<FinishOutcome>`.
  `FinishOutcome` carries `done`, `total`, and `abandoned: Vec<TaskSummary>` so the CLI can print what was dropped.
  With unfinished tasks and `!abandon_open`: `bail!` with the list and the two fixes.
- A new pure function `unfinished_tasks(tasks: &[Task]) -> Vec<&Task>` filters on state; both `finish_session` and the Stop reminder use it.

Keep each function under the project's 100-line limit; the listing and the error text belong in a small formatter.

### CLI (`src/cli/mod.rs`)

- `TaskCommand::Done { id, force: bool }` with `#[arg(long)]`.
- `TaskSessionCommand::Finish { id, abandon_open: bool }` with `#[arg(long)]`.
- Error output goes through the existing error path (non-zero exit, message on stderr).
- Success output for `finish` with abandoned tasks: `Finished session '<id>' (<done>/<total> done, <n> abandoned)` followed by one line per abandoned task.

### Native-tool redirect (`src/hook_run/task_tools.rs`)

- Pass `false` to `complete_task`.
- Map the error into the redirect's reply text so the agent sees the fix and the tool call is not silently dropped.
- Keep the "added and completed in the same call" allowance.

### Stop reminder (`src/task/mod.rs`)

- `idle_reminder_lines`: when `open_count > 0`, the line reads
  `Session '<id>' has <n> open task(s); start the next one with llmenv task start <slug>, or finish them before llmenv task session finish <id>.`
  When `open_count == 0` the current text stays.
- `session_finish_reminders` is unchanged.

### Docs

- `website/docs/commands.md`, `## task`: `task done <id> [--force]`, `task session finish [<id>] [--abandon-open]`, the new refusal behavior, tagged `(changed in v3.12.0)`.
- `skills/llmenv/references/task-tracker.md`: same two flags, and the rule "start before done".
- Changelog under `Changed`: `task done` refuses a never-started task and `task session finish` refuses with open tasks; `--force` and `--abandon-open` override.

## Implemented as

- `complete_task(state_dir, input, force)` also refuses to close a parent whose sub-tasks are not done unless `force` is set (#2455).
  `Completed` carries `skipped_start_note()` and `undone_children_note()` for the forced path; `never_started_warning()` is gone.
- The native-tool redirect forces only a todo that was added and completed in the same `TodoWrite` call (`existing.is_none()`).
  Every other completion, and every `update` to `completed`, passes `force = false`.
- `unfinished_tasks` and `unfinished_error` are private to `src/task/session.rs`.
  `FinishOutcome` is `{ session, done, total, abandoned: Vec<Task> }`, and `total` counts abandoned tasks.

## Tests

1. Unit, `complete_task`: `Open` without force errors and the message names `task start` and `--force`; `Open` with force completes and reports `prior == Open`; `Wip`, `Waiting`, `Done` unchanged.
2. Unit, `finish_session`: one `Open`, one `Wip`, one `Waiting` task each block the finish and appear in the error; `abandon_open` untags them, adds the note, and returns them in `FinishOutcome`; a session with only `Done` tasks finishes as before.
3. Unit, `idle_reminder_lines`: with `open_count > 0` the text does not contain `session finish`; with `open_count == 0` it does.
4. Redirect: `TaskUpdate` to `completed` on an `open` task returns the refusal text and leaves the task `open`; `TodoWrite` add-and-complete in one call still completes.
5. Integration (`tests/task_cli.rs`): `done` on an open task exits non-zero; `done --force` exits 0; `session finish` with an open task exits non-zero and lists it; `session finish --abandon-open` exits 0 and the task is no longer in the session.
6. Property test: for any mix of task states, `finish_session` without `abandon_open` succeeds if and only if every task is `Done`.

## Acceptance criteria

1. `llmenv task done <slug>` on an `open` task exits non-zero with the fix; `--force` completes it.
2. `llmenv task session finish` with any `open`, `wip`, or `waiting` task exits non-zero and lists them; `--abandon-open` finishes and reports what it dropped.
3. The Stop reminder names `session finish` only when no task is open.
4. The native-tool redirect cannot bypass the refusal.
5. Changelog entry under `Changed`; `commands.md` and the task-tracker reference updated with the version tag.

## Out of scope

- The instruction side (global CLAUDE.md, ship-issue, executing-plans, pre-pr-review skills). That lives in the my-llmenv config repo and is filed there.
- A new acceptance-criteria field. `--detail` from #2339 already covers it; the docs just say to use it.
- Changing what `task clear` does.
