<!-- markdownlint-disable MD013 -->
# Task Tracker: Mandatory Sessions, Project Tagging, and an `llmenv` Skill — Design

## Problem

`llmenv task`'s store lives at a single, global-per-engine path —
`<state_dir>/tasks/` (e.g. `~/.cache/llmenv/claude-code/state/tasks/`), keyed
only by engine (claude-code vs opencode vs crush). It has no notion of
"project" at all, and sessions are single-active-per-store:
`start_session` errors if one is already active, or `--force` abandons it.

- Two windows working in the **same** repo currently share the session
  pointer, so starting a session in one window can force-abandon the
  other's — true multi-window concurrency is impossible without one window
  destroying the other's state.
- Two windows in **different, unrelated** repos also share the same task
  list and the same session pointer, which is unexpected but at least
  `llmenv task ls` showing everything across every project is a property
  worth keeping, not a bug to fix by partitioning the store.
- Nothing helps an agent notice "there's already a session open for this
  project — did you mean to resume it, or is it stale and safe to replace?"

An earlier iteration of this design partitioned the store per-project
(`tasks/projects/<project-key>/`). Reconsidered: **the store stays exactly
where it is today, flat and global per engine** — `task ls` keeps showing
everything. The isolation problem is solved a different way: sessions become
**mandatory** (every task belongs to one), each session is **tagged with the
project it was started in** (metadata, not a partition key), and
`session start` gets a checkpoint that surfaces an existing same-project
session instead of silently colliding with it or requiring a destructive
`--force`.

## Design

### 1. Project resolution (metadata only, not a store partition)

Same precedence considered in the earlier iteration, reused here purely to
compute a tag stored on each `Session`, not to relocate any files:

1. **Walk up from cwd looking for a `.git` entry** (directory or file —
   worktree/submodule pointers count, existence check only, the pointer
   target is never resolved). If found, that directory is the project root.
   Then check whether `.llmenv.yaml` also exists in that same directory — if
   so, use its `id` field as the human-readable name component instead of
   the directory's basename (the root itself stays the git root either way).
2. **If no `.git` found** anywhere walking up: fall back to the existing
   `.llmenv.yaml` marker discovery (`discover_project`, bounded at `$HOME`).
   Use whatever root it finds.
3. **If neither is found**: the literal cwd is the project root.

Project tag: `{name}-{hash}` — `name` is the resolved `.llmenv.yaml` `id` (if
present) or the root's basename; `hash` is the first 10 hex characters of
`SHA-256(canonicalized absolute root path)`, reusing the `sha2` crate already
in the workspace (used for cache-folder content hashing) — no new
dependency. Example: `llmenv-a3f9c21b04`. Resolved fresh on every
invocation, no caching (cwd can change between commands).

### 2. Sessions are mandatory

Every task must belong to a session — no more untagged/sessionless tasks.

`llmenv task add <title> [--session <id>]`:

- `--session <id>` explicit: tag with that session (error if it doesn't
  exist or isn't open).
- Omitted: if exactly one open session is tagged to the current project,
  auto-use it (today's implicit convenience, now scoped to the project tag
  instead of "the one open session in the whole global store"). If zero or
  2+ are open for this project, `task add` **errors**, telling the agent to
  run `llmenv task session start` first (or pass `--session` explicitly) —
  it does not silently auto-create a session, since that would make
  `session start`'s resume/replace/ignore checkpoint (below) pointless: it
  would just never fire.

### 3. `session start`'s resume / replace / ignore checkpoint

`llmenv task session start [name] [--description <text>]`:

1. Resolve the current project tag.
2. Look for existing **open** sessions tagged with it.
3. **None found** → create the new session normally, no friction.
4. **One or more found**, and no `--resume`/`--replace`/`--new` flag given →
   **error**, listing each existing session's id, name, description, and
   idle duration (derived from `last_activity`), requiring one of:
   - `--resume <id>` — adopt the existing session instead of creating a new
     one; touches its `last_activity`. No new session id is generated.
   - `--replace` — abandon every existing open session tagged to this
     project (same untag-incomplete-tasks + orphan-note behavior the
     earlier iteration called `--abandon`; done tasks stay tagged as a
     historical record), then create a fresh session.
   - `--new` — create a new session anyway, leaving the existing one(s)
     open. This is the genuine-concurrency path: two windows in the same
     project, both legitimately active at once.

### 4. `Session` schema additions

| Field | Type | Purpose |
| ------- | ------ | --------- |
| `project` | `String` | The resolved project tag from §1. Informational — used to filter/sort in `session ls`, `task add`'s auto-resolve, `session start`'s checkpoint, and the statusline widget. Never used to partition storage. |
| `description` | `Option<String>` | New, separate from `name`. Free text set via `--description <text>` at `session start` (e.g. "dev-sprint issue 493", "brainstorm a feature about task lists"). `name` stays short and feeds slug/id generation as it does today; `description` is display-only, giving a human or agent enough context to make the resume/replace/ignore call without needing to inspect every task inside the session. |
| `last_activity` | `String` (RFC3339) | Updated whenever any task tagged to this session changes (`add`/`start`/`done`/`note`) or the session itself is resumed. Surfaced as an idle duration in `session ls` and the checkpoint error, so staleness is a judgment call an agent or human can make at a glance — no hard-coded auto-expiry threshold. |

### 5. `llmenv task session ls` (new)

Lists every currently open session — global, since the store is global —
showing id, name, description, project tag, and idle duration. Sorted with
sessions matching the current project's tag first, then the rest by
recency. This is the recovery path after a context compaction: if the agent
doesn't remember its own session id, it re-derives "which session is mine"
from this list rather than needing to have memorized anything.

### 6. Surviving context compaction

Worked through directly: since `session start`'s checkpoint blocks silently
creating a second same-project session, the common case (one agent, one
window, one project) converges on **exactly one** open session per project.
After a compaction wipes the agent's memory of its own session id, it can
run `session ls`, see exactly one match for its project, and use that — no
memorization needed at all.

The only case that genuinely requires the agent to have durably remembered
its *specific* session id is true concurrency: 2+ open sessions for the same
project (the `--new`/ignore path). There is no engine-level mechanism today
that would let llmenv guarantee that identity survives a compaction — no
engine exposes a stable per-window identity to the Bash tool's subprocess
environment (checked: `OPENCODE_SESSION_ID` is only visible inside
opencode's own hook-bridge process building `llmenv hook-run`'s stdin
payload, not inherited into Bash-tool subprocesses; no confirmed equivalent
for Claude Code or Crush). This is accepted as a documented limitation,
mitigated only by skill guidance telling the agent to keep referencing its
session id explicitly once concurrency is in play — not solved at the
protocol level.

### 7. Statusline `tasks` widget

Since every task now requires a session, the old "bare open+`wip` count"
fallback no longer exists (that count was always untagged tasks). Filtering
to sessions tagged with the current project (same resolution as §1):

- Exactly one open → show its `done/total` (unchanged from before).
- Zero open for this project → render empty — no active work is being
  tracked here.
- 2+ open for this project (genuine concurrency) → sum `done`/`total`
  across just this project's open sessions. Now that sessions carry a
  project tag, this sum is meaningful (they're at least related work),
  unlike summing across every open session globally would have been.

### 8. `llmenv` skill

Unchanged in structure from the earlier iteration: `skills/llmenv/SKILL.md`
(thin router, static description, conditional body) plus
`skills/llmenv/references/{task-tracker,memory,context-mode,codebase-memory}.md`,
following the `skills/setup-llmenv/` precedent (embedded via `include_str!`,
materialized on every `export`/`regenerate` via the existing cross-engine
skill-materialization path, replacing the current CLAUDE.md task-tracker
fragment entirely, and skipped altogether if none of the four features are
enabled).

The `task-tracker.md` reference file content changes to teach the new
model: sessions are mandatory before `task add` works; the
resume/replace/ignore flow at `session start`; when to pass `--description`
(whenever the agent has enough context to make one meaningful — a
dev-sprint issue number, a brainstorming topic, etc.); and the compaction
caveat from §6, so the agent knows to re-run `session ls` after a
compaction rather than assuming it remembers its own id.

## Testing

- **Property tests**: project-tag derivation is deterministic, bounded
  length, filesystem-safe-character output, across arbitrary root paths.
  Resolution-precedence tests cover all three branches from §1.
- **Mandatory-session enforcement**: `task add` with zero/2+ open
  project-tagged sessions and no `--session` errors with actionable
  guidance; exactly-one auto-resolves; explicit `--session` always works
  (and rejects an unknown/closed id).
- **`session start` checkpoint**: no existing session → creates cleanly;
  existing session(s) + no flag → errors listing them; `--resume`,
  `--replace`, `--new` each produce the documented outcome (including that
  `--replace` preserves done tasks' tags while untagging + noting
  incomplete ones, matching the old abandon behavior).
- **`description`/`last_activity`**: set and round-trip correctly;
  `last_activity` updates on every task mutation tied to the session and on
  `--resume`.
- **`session ls`**: lists exactly the open sessions, current-project
  matches sorted first.
- **Statusline widget**: exactly-one-for-project shows `done/total`;
  zero-for-project renders empty; 2+-for-project sums correctly and doesn't
  leak in sessions from unrelated projects.
- **Integration tests** (`tests/task_cli.rs`): the full mandatory-session +
  checkpoint flow end to end through the actual CLI.
- **Adapter tests** (Claude Code, opencode, Crush): skill materializes only
  the reference files for enabled features; absent entirely when none are
  enabled.

## Non-goals

- Partitioning the task/session store by project — reconsidered and
  dropped; `task ls`/the store stay global, isolation comes from mandatory
  sessions + project tagging instead.
- Migrating anything — nothing about the store's location or shape changes,
  so there's nothing to migrate.
- Guaranteeing session-identity survival across compaction for genuinely
  concurrent same-project sessions — investigated, not solvable without an
  engine-exposed session identity that doesn't currently exist; documented
  as a known limitation, mitigated only by skill guidance.
- Expanding the skill beyond the four listed built-ins (statusline was
  considered and excluded — it's a passive, human-facing display, not
  something the agent calls during normal work).
