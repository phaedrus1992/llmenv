# Task Tracker

Durable, cross-session task state — use it instead of relying on in-session
TODOs.

- `llmenv task session start "<name>" [--description "<text>"]` — required
  before your first `task add`. Add `--description` for a session ls hint
  (issue number, topic) when you have one.
- Name it after the high-level work, not a placeholder — `session ls` is the
  recovery path after a compaction, and an unnamed or auto-numbered session
  (`session-2`, `session-3`) tells a future read of that list nothing. Good:
  `oauth-token-refresh`, `v3.6.1-task-tracker-fixes`. Bad: leaving `<name>`
  empty, or `session-4`.
- If a session is already open for this project, `session start` errors and
  lists them: `--resume <id>` (this is yours, e.g. after a compaction),
  `--replace` (stale, untag its tasks and start fresh), or `--new` (genuinely
  parallel work).
- `llmenv task add "<title>" [--session <id>]` — auto-tags to your one open
  session; pass `--session` if 2+ are open. Errors rather than guessing.
- `llmenv task start|done <slug>` — claim it, or mark it finished.
- `llmenv task note|wait <slug> ["<text>"]` — log a progress note, or mark
  blocked on external/human input; the text/reason reads from stdin if
  omitted.
- `llmenv task add "<title>" --parent <slug>` / `llmenv task block <slug>
  --on <other>` — link tasks liberally, not just for big epics: `parent`
  orders sub-tasks under a parent in `task ls`; `block` records a real
  dependency and shows up as blocked in `task ls`.
- `llmenv task note` is the durable *why*, not just *what* — record
  milestones, design decisions (with rejected alternatives), and failures (so
  a retry after compaction doesn't repeat them). SessionStart/Stop reminders
  and memory writes draw on these notes.
- Lost your session id after a compaction? `llmenv task session ls` — one
  match for this project in the common case; use it.
- "What am I on / what's next?" — `llmenv task show --current` resolves your
  `wip` task for this project without naming it; `llmenv task show --next`
  resolves the next actionable task after it (skipping `done`/`waiting` and
  anything still `blocked_on`). `llmenv task ls --current-project` narrows a
  full listing to this project's tasks.
- `llmenv task session finish [<id>]` / `session show [<id>]` to close out —
  `finish` auto-resolves if exactly one session is open.
