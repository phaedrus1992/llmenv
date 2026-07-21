<!-- markdownlint-disable MD013 -->
# Task Tracker: Project Scoping, Concurrent Sessions, and an `llmenv` Skill — Design

## Problem

`llmenv task`'s store lives at a single, global-per-engine path —
`<state_dir>/tasks/` (e.g. `~/.cache/llmenv/claude-code/state/tasks/`), keyed
only by engine (claude-code vs opencode vs crush). It has no notion of
"project" at all:

- Two windows working in the **same** repo share one task list — expected,
  but currently share the **session** pointer too, so starting a session in
  one window can force-abandon the other's.
- Two windows working in **different, unrelated** repos on the same machine
  also share the same task list — almost certainly not what a user expects
  from a "what am I working on" tracker.
- Sessions are single-active-per-store: `start_session` errors if one is
  already active, or `--force` abandons it. This makes true multi-window
  concurrency impossible without one window destroying the other's state.

Additionally, the only agent guidance for the task tracker (and the other
agent-facing built-ins: memory/ICM, context-mode, codebase-memory-mcp) is a
CLAUDE.md fragment injected when `features.task_tracker.enabled`. There's no
single place an agent can go to learn how llmenv's built-ins work together.

## Design

### 1. Project resolution & store layout

Precedence, evaluated fresh on every `llmenv task` invocation (no caching —
cwd can change between commands):

1. **Walk up from cwd looking for a `.git` entry** — existence check only, a
   plain directory (normal repo) or a file (worktree/submodule pointer) both
   count; the pointer target is never resolved, since the directory
   containing it is the project root we want either way. If found, that
   directory is the project root. Then check whether `.llmenv.yaml` **also
   exists in that same directory** — if so, use its `id` field to override
   the human-readable name component (not the root itself; the root stays
   the git root).
2. **If no `.git` found** anywhere walking up: fall back to the existing
   `.llmenv.yaml` marker discovery (`discover_project`, bounded at `$HOME`).
   Use whatever root it finds.
3. **If neither is found**: the literal cwd is the project root.

Project key: `{name}-{hash}`, where `name` is the resolved `.llmenv.yaml` `id`
(if present) or the root directory's basename, and `hash` is the first 10 hex
characters of `SHA-256(canonicalized absolute root path)` — reusing the `sha2`
crate already in the workspace (used for cache-folder content hashing), no
new dependency. Example: `llmenv-a3f9c21b04`. Bounded length, filesystem-safe,
collision-resistant enough for practical purposes, and human-legible enough
to recognize at a glance in `~/.cache/llmenv/*/state/tasks/projects/`.

Store layout changes from the current flat `<state_dir>/tasks/` to
`<state_dir>/tasks/projects/<project-key>/`, with everything currently under
`tasks/` (task files, `sessions/`, the store `.lock` file) moving one level
deeper, per-project.

**Existing global tasks are not migrated.** They stay inert under the old
flat path. The task tracker is documented as ephemeral, cross-session scratch
state, not a permanent record — this is an acceptable, low-cost trade-off
against building and maintaining a one-time migration path.

### 2. Concurrent sessions

`start_session` no longer enforces "one active session" at all — it always
creates a new session and returns its id. There is no more `active_session`
pointer file; the single global `--force`/abandon-the-active-one flow it
enabled goes away with it, since there's nothing to reclaim from anymore. A
session is "open" as long as neither `finished_at` nor `abandoned_at` is set,
same as today — the only change is that **more than one can be open at
once**.

**Task tagging.** `llmenv task add <title> [--session <id>]`:

- `--session <id>` explicit: tag the new task with that session (erroring if
  it doesn't exist or isn't open).
- Omitted: if exactly one session is currently open, auto-tag with it
  (today's implicit behavior, preserved for the common single-window case).
  If zero or 2+ sessions are open, the task is created untagged (plain
  open/`wip`), same as "no session" today — no guessing across multiple
  candidates.

This is the mechanism that actually lets two windows in the same project
coexist: each window's agent captures the session id `task session start`
returns and passes `--session <id>` explicitly on every `task add` for that
batch of work, rather than relying on a single shared "active" pointer. The
auto-tag fallback exists purely as a convenience for the common case (one
window, one session) — it's preferred, not required, exactly as asked.

**Session commands**, all taking an optional `<id>` argument that defaults
the same way as `add`'s `--session` (exactly one open → use it; otherwise
error, listing open session ids, asking to disambiguate):

- `llmenv task session start [name]` — always creates, returns the new id.
- `llmenv task session finish [<id>]` — normal completion; done tasks stay
  tagged (historical record), same as today.
- `llmenv task session finish [<id>] --abandon` — the old `--force`
  behavior, now explicit and targeted instead of implicit: untags every
  incomplete task in that session with an orphan note, leaves done tasks
  tagged. Needed because a session can still go stale (agent crash, window
  closed mid-work) even without a single-active-session invariant forcing
  the issue.
- `llmenv task session show [<id>]` — as today, same default resolution.
- `llmenv task session ls` — **new**: lists every currently-open session
  (id, name, started_at). Needed now that "the" active session is no longer
  a coherent concept — an agent (or a human) needs a way to see what's open
  before picking one to target.

### 3. Statusline `tasks` widget

Investigated whether an engine exposes a session identity to the Bash tool's
shell (which would let the widget — and `add_task`'s fallback — know exactly
which session belongs to which window, no ambiguity at all). It doesn't:
`OPENCODE_SESSION_ID` is only visible inside opencode's own hook-bridge Node
process building the `llmenv hook-run` stdin payload, not inherited into the
Bash tool's subprocess environment. No confirmed equivalent exists for Claude
Code or Crush either. This is a different code path from hook stdin JSON
(which does carry a session id, but only to hook-invoked processes, not
arbitrary agent-run commands) — worth revisiting if an engine ever exposes
one, but not something to design the core mechanism around today.

Given that, the widget's display rule: exactly one session open → show its
`done/total` (today's behavior, unchanged). Zero or 2+ open → fall back to
the bare open+`wip` count, same as the "no session" case today. Summing
progress across unrelated concurrent sessions was considered and rejected —
a number that blends two windows' unrelated work isn't more meaningful than
the plain count.

### 4. `llmenv` skill

A new first-party skill, following the `skills/setup-llmenv/` precedent
(source lives in the llmenv repo, embedded via `include_str!`, materialized
into the target config directory alongside bundle-authored skills — the
existing cross-engine skill-materialization path already handles Claude
Code, opencode, and Crush uniformly, no adapter-specific work needed).

Unlike `setup-llmenv` (a one-off wizard written only when `llmenv setup`
runs), this skill materializes on every `export`/`regenerate`, mirroring how
the current CLAUDE.md task-tracker fragment is injected — and **replaces**
that fragment entirely. Skill discoverability (via its frontmatter
`description`, surfaced by the engine's own skill-listing mechanism) does the
job the always-injected fragment used to do, without permanently spending
CLAUDE.md tokens on it.

Structure, keeping token cost minimal by default (per-request):

```text
skills/llmenv/
  SKILL.md              # thin router: static description, conditional body
  references/
    task-tracker.md      # add/start/done/note/session lifecycle, --session
                          # flag, project scoping, concurrent sessions
    memory.md             # icm_* MCP tool usage: when to store/recall
    context-mode.md      # token-efficiency built-in usage
    codebase-memory.md   # codebase-memory-mcp tool usage
```

`SKILL.md`'s body is composed at materialize time (mirroring today's
conditional fragment logic, generalized from one feature to four): it links
to a reference file only if that feature is actually enabled for this scope
(`features.task_tracker`, `features.memory`, `features.context_mode`,
`features.codebase_memory`). Reference files themselves are static content —
only the files for enabled features get materialized at all, matching
`SKILL.md`'s own links, so a user never sees a reference to a feature they
don't have on. If **none** of the four features are enabled, the skill isn't
materialized at all (matching today's fragment-gating behavior, generalized
from one flag to "any of these four").

The point of the reference-file split: `SKILL.md` itself stays a short
index — enough to route the agent to the right file, nothing more — so the
token cost of having the skill installed at all is minimal until the agent
actually needs to go deep on one specific built-in.

## Testing

- **Property tests**: project-key derivation is deterministic (same root →
  same key every time), bounded length, and produces only filesystem-safe
  characters across arbitrary root paths. Resolution-precedence tests cover
  all three branches (git-root-with-marker, git-root-without-marker,
  no-git-marker-fallback, no-git-no-marker-cwd-fallback) plus the boundary
  cases already covered for `.llmenv.yaml` discovery (bounded at `$HOME`).
- **Concurrent-session unit tests**: multiple `start_session` calls never
  error and each returns a distinct id; explicit `--session` tags correctly
  (and errors on an unknown/closed id); the exactly-one-open auto-tag
  fallback; the zero-or-2+-open untagged fallback; `--abandon` vs a normal
  `finish` (done tasks preserved either way, incomplete tasks untagged only
  under `--abandon`); `session ls` lists exactly the open sessions.
- **Integration tests** (`tests/task_cli.rs`): the new `--session` flag and
  `session ls`/`--abandon` end to end through the actual CLI.
- **Statusline widget tests**: exactly-one-open still shows `done/total`;
  zero-open and 2+-open both fall back to the bare count.
- **Adapter tests** (one per engine: Claude Code, opencode, Crush): skill
  materializes only the reference files for enabled features, `SKILL.md`'s
  body links only those, and the skill is entirely absent when none of the
  four features are enabled.

## Non-goals

- Migrating existing global (pre-project-scoping) tasks — explicitly out of
  scope; they're left inert.
- Inferring a specific engine window/session identity for statusline
  aggregation or task auto-tagging — investigated, not currently possible
  across engines; revisit if that changes.
- Expanding the skill beyond the four listed built-ins (statusline was
  considered and explicitly excluded — it's a passive, human-facing display,
  not something the agent calls during normal work).
