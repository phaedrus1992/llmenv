# Issue #3196 — the status line tasks widget shows only this conversation's work

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/3196
- **Milestone:** `v3.14.0`
- **Base branch:** `release/3.x`
- **Code:** `src/materialize/status_data/tasks.rs`, `src/cli/statusline/data.rs`, `src/cli/statusline/widget.rs`, `src/cli/statusline/llmenv_widget.rs`, `src/task/session.rs`
- **Type:** bug fix, status file field added, tests, docs

## Problem

`try_tasks_data` (`src/materialize/status_data/tasks.rs:53`) sums the progress of every open session of the project.
It also picks the current task from all of them.
Since #3185, the Stop and AskUserQuestion reminders and the commit gate skip a session that another conversation owns (`Session::visible_to`, `src/task/session.rs:226`).
The status line does not, so two conversations in one project see each other's work as their own.

The status data is written to `llmenv-status.json` when llmenv materializes (`write_post_materialize`, `src/cli/mod.rs:1932`).
That write has no conversation id.
The status line process reads the file once per refresh (`StatusData::load`, `src/cli/statusline/mod.rs:159`) and reads no task files.

## Decision

Owner decision on #3196 (2026-11-03), option 1:

> The status line command reads the conversation id from its stdin JSON and filters the sessions itself with `Session::visible_to`. Acceptance: a test that two conversations in one project each count and name only their own task. The sessions that the renderer needs must reach it without a new file read on every refresh, so decide how in the implementation plan.

## How the sessions reach the renderer

The writer adds one list to the `tasks` object of `llmenv-status.json`: one entry for each open session of the project, with its owner, its progress, and its current task.
The renderer already reads that file on every refresh, so it gets the list with no new file read.
It filters the list with the same rule as `Session::visible_to`.

The data is as fresh as the file.
The file is written at materialize time today, and this change does not change when it is written.

## Design

### 1. One visibility rule, shared (`src/task/session.rs`)

Add a free function, and make `Session::visible_to` call it:

```rust
/// Whether a session with `owner` is visible to `caller`. A session with no owner is visible
/// to every caller, and a caller with no id sees every session (#3185).
pub(crate) fn owner_visible(owner: Option<&str>, caller: Option<&str>) -> bool {
    match (caller, owner) {
        (Some(c), Some(o)) => c == o,
        _ => true,
    }
}
```

`visible_to(&self, caller)` becomes `owner_visible(self.owner_session.as_deref(), caller)`.
The existing test `visible_to_hides_only_a_session_that_another_conversation_owns` (`src/task/session.rs:1384`) must keep passing.

The field `owner_session` (`src/task/session.rs:53`) is private.
Add a read accessor on `Session`, and keep the field private:

```rust
/// The engine conversation that owns this session, if one is recorded.
pub(crate) fn owner_session(&self) -> Option<&str> {
    self.owner_session.as_deref()
}
```

### 2. The status file field (`src/cli/statusline/data.rs`)

Add a type next to `TasksData` (around line 82).
Give it the same derives as `TasksData`.
Do not derive `Default`: `SessionProgress` (line 91) does not implement it.

```rust
/// One open session of the project, as the status line needs it (#3196).
pub(crate) struct SessionTasks {
    /// The conversation that owns the session (`Session::owner_session`). `None` for a session with no owner.
    pub owner: Option<String>,
    /// Done and total tasks of this session.
    pub progress: SessionProgress,
    /// The title of this session's task in progress, if any.
    pub current: Option<String>,
}
```

Add one field to `TasksData`:

```rust
/// Each open session of the project. Empty in a file from an older llmenv.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub sessions: Vec<SessionTasks>,
```

Keep the existing fields `session`, `current`, and `unreadable`, and keep filling them as today.
A status line that has no conversation id uses them.

The schema name `llmenv-status-v1` and `v: 1` do not change, because the new field is optional.

`TasksData` has no `Default`, so every `TasksData { .. }` struct literal needs the new field.
There are about 13 of them: search with `rg -n "TasksData \{" src`.
Add `sessions: Vec::new()` to each one, except where step 3 fills the list.
Two literals are in production code in `src/materialize/status_data/tasks.rs`: the one in `collect_tasks`, and the one in its error branch.

### 3. The writer (`src/materialize/status_data/tasks.rs`)

In `try_tasks_data(state_dir, project)`:

1. Keep the current aggregate code unchanged.
2. Sort the open sessions by session id, so the file is the same for the same state.
3. For each open session, push one `SessionTasks`:
   - `owner`: `session.owner_session().map(str::to_owned)`.
   - `progress`: the session's own result from `try_session_progress`. Reuse the value that the aggregate loop already computes, so each session is read once.
   - `current`: the current task title of this session alone. Call `current_wip_title(state_dir, ...)` with a list that holds only this session's id.
4. A read error anywhere keeps today's behavior: the function returns `Err`, and `collect_tasks` sets `unreadable: true`.

### 4. The stdin field (`src/cli/statusline/widget.rs`)

Add a field to `EngineData` (line 27):

```rust
/// The engine conversation id. Claude Code sends it as `session_id` in the status line input.
#[serde(default)]
pub session_id: Option<String>,
```

### 5. The filter, applied before rendering (`src/cli/statusline/llmenv_widget.rs` and `src/cli/statusline/mod.rs`)

The widget renderer `render_llmenv_widget(name, data, cfg, icons, use_color)` never sees `EngineData`, and it already has five positional parameters, the project cap.
So the filter runs once in `run_statusline` (`src/cli/statusline/mod.rs:150`), after it has both the parsed `EngineData` (line 158) and the loaded `StatusData` (line 159), and before it renders any widget (line 209).
`render_llmenv_widget` and `render_tasks` keep their signatures.

Add a pure function to `src/cli/statusline/llmenv_widget.rs`:

```rust
/// The progress and current task that `caller` may see. Sums the visible sessions in `tasks.sessions`.
/// A file with no `sessions` list (an older llmenv, or no open session) falls back to the aggregate fields.
fn visible_tasks(tasks: &TasksData, caller: Option<&str>) -> (Option<SessionProgress>, Option<String>)
```

Rules:

1. If `tasks.sessions` is empty, return `(tasks.session.clone(), tasks.current.clone())`.
2. Otherwise keep the entries where `owner_visible(entry.owner.as_deref(), caller)` is true.
3. If no entry is kept, return `(None, None)`. The widget then prints nothing, as it does today for a project with no open session.
4. Sum `done` and `total` over the kept entries into one `SessionProgress`.
5. The current title is the `current` of the first kept entry that has one. The entries are in session-id order (step 3.2), so the choice is stable.

Add a second function that writes the result into the data:

```rust
/// Narrow `tasks` to what `caller` may see: replace `session` and `current` with the result of
/// [`visible_tasks`]. Leaves `unreadable` as it is.
pub(crate) fn scope_tasks_to_caller(tasks: &mut TasksData, caller: Option<&str>) {
    let (session, current) = visible_tasks(tasks, caller);
    tasks.session = session;
    tasks.current = current;
}
```

In `run_statusline`, after line 159, call `scope_tasks_to_caller` on the loaded tasks data with `engine.session_id.as_deref()`.
If `StatusData.tasks` is an `Option`, call it only when it is `Some`.
Do not read the conversation id from the environment.

`render_tasks` (line 159 of `llmenv_widget.rs`) does not change.
It reads `session` and `current` as today, which now hold the filtered values.
The `unreadable` check, the default format `☑ {done}/{total}`, the `{current}` placeholder, and its sanitizing stay as they are.

## Tests

Writer test in the tests of `src/materialize/status_data.rs`, next to `collect_tasks_two_open_sessions_sums_across_project_only` (around line 967):

1. `collect_tasks_lists_each_open_session_with_its_owner`
   - Make two open sessions in one project, owned by `conv-a` and `conv-b`. The field `owner_session` is private, so do not set it directly. Start each session with `start_session_as` and a `StartRequest` whose `owner` is an `EngineIdentity` with `session_id: Some("conv-a")` (then `"conv-b"`), and `StartDecision::New`. Read the exact fields of `StartRequest` and `EngineIdentity` in `src/task/session.rs`, and fill the other fields as the existing `collect_tasks_*` tests do.
   - Give `conv-a` two tasks: one done, and one in progress named `task A`. Its progress is 1/2.
   - Give `conv-b` two tasks: one open, and one in progress named `task B`. Its progress is 0/2.
   - Call `collect_tasks_from_state_dir`.
   - Assert `sessions` has two entries in session-id order, each with its owner, its own `done`/`total`, and its own `current`.

Renderer tests in the tests of `src/cli/statusline/llmenv_widget.rs`, next to `renders_tasks_done_over_total_when_session_open` (line 565).
Build `TasksData` values by hand.
In each test, call `scope_tasks_to_caller(&mut tasks, caller)`, then render the tasks widget the same way the existing `render_tasks_*` tests do.
Use the format `{done}/{total} {current}`, so each assertion checks the count and the name.

2. `two_conversations_each_see_only_their_own_task` (the acceptance test)
   - `sessions`: owner `conv-a` with 1/2 and current `task A`; owner `conv-b` with 0/1 and current `task B`.
   - With caller `conv-a`, assert the output is `1/2 task A`.
   - With caller `conv-b`, assert the output is `0/1 task B`.
3. `a_session_with_no_owner_is_shown_to_every_caller`
   - `sessions`: owner `None` with 1/1, and owner `conv-b` with 0/1.
   - With caller `conv-a`, assert `1/1`.
4. `no_caller_id_sums_every_session`
   - The same two sessions as test 2, with caller `None`. Assert `1/3` and the current title `task A`.
5. `a_file_without_sessions_uses_the_aggregate`
   - `sessions` empty, `session: Some(2/5)`, `current: Some("old")`, with caller `conv-a`. Assert `2/5 old`.
6. `a_caller_with_no_visible_session_renders_nothing`
   - `sessions`: one session owned by `conv-b`, with caller `conv-a`. Assert the widget output is empty.

Stdin test in the tests of `src/cli/statusline/widget.rs` (or wherever `EngineData` parsing is tested):

7. `engine_data_reads_the_session_id`
   - Parse `{"session_id":"abc"}`, and assert `session_id == Some("abc")`.
   - Parse `{}`, and assert `session_id == None`.

All existing tasks widget and `collect_tasks_*` tests must keep passing.

## Docs

Find the `tasks` widget in the status line docs under `website/docs/` (search for `☑` or `tasks` in the status line page).
Add this sentence to its description:

```markdown
In a project with several conversations, the widget counts only the sessions that the current conversation owns, plus sessions with no owner (changed in v3.14.0).
```

## Changelog

Add one entry under `## [Unreleased]` → `### Fixed` in `CHANGELOG-3.md`:

```markdown
- Count and name only this conversation's task sessions in the status line `tasks` widget, plus sessions with no owner. Two conversations in one project no longer see each other's work. [fix:statusline] [#3196]
```

Take the tag from the commit that adds `scope_tasks_to_caller`.
Then run `scripts/sync-changelog-doc.sh` and commit `website/docs/changelog.md` with it.

## Out of scope

- How often `llmenv-status.json` is written. The widget can lag between materializations, as it does today.
- A task-store index by session. The writer runs at materialize time, so its extra reads do not affect the status line refresh.

## Spec-writer calls

- The aggregate fields stay, so a status line with no conversation id, or a file from an older llmenv, still renders.
- The first visible session in session-id order gives the current title. The issue does not say which title wins when two visible sessions each have one.
- `owner_visible` is a free function so the renderer applies the exact rule of `visible_to` without a `Session` value.
- The filter runs once in `run_statusline`, not inside the widget, so no widget function grows past the five-parameter cap.
