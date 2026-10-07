# Commands

Every command accepts `--color <auto|always|never>` (default `auto`). Run
`llmenv <command> --help` for the authoritative flag list. Global flags:
`-h/--help`, `-V/--version`.

## `init`

```text
llmenv init [PATH] [--repo URL]
```

Initialize llmenv configuration. Writes a template `config.yaml` into the config
directory (or `PATH` if given). With `--repo URL`, clones an existing config
repository instead of writing a template. No-op if a config already exists.

## `export`

Deprecated (as of v3.10.0): superseded by `llmenv launch <engine>`
([#1056](https://github.com/phaedrus1992/llmenv/issues/1056)), a supervised,
ambient replacement landing in v4.0.0. `export`/the shell-hook flow keeps
working through v4.0.0 — this is advance notice, not a removal.

```text
llmenv export [--scope ID] [--tag TAG] [--explain] [--compress]
```

Resolve the current environment and print shell `export` lines. This is what the
shell hook runs on every prompt. It also materializes the agent config directory
and emits the introspection env vars (`LLMENV_ACTIVE_*`, `LLMENV_PROJECT_ROOT`,
`LLMENV_ICM_CONTEXT`) and the adapter's pointer var (`CLAUDE_CONFIG_DIR`).

- `--tag TAG` filters to bundles carrying that tag.
- `--scope ID` narrows the export to that scope's tags (plus OS/extra tags)
  when the scope is active in the current environment. If the requested scope
  isn't active, a warning is printed and all matching tags are exported
  instead.
- `--explain` annotates each exported variable with a `# source:` comment line
  showing whether it comes from the adapter (with the firing bundle names) or
  from llmenv introspection.
- `--compress` strips trailing whitespace and collapses repeated blank lines in
  the materialized `CLAUDE.md` / `AGENTS.md` to reduce token cost.

## `regenerate`

```text
llmenv regenerate
```

Regenerate the materialized config without emitting shell `export` lines. Use
after editing `config.yaml` or bundle files when the current shell already has
the right env vars.

### When one engine can't be rendered

(added in v3.11.0)

Each installed engine is regenerated independently, so a config that one engine
rejects doesn't stop the others — that engine simply keeps its previous config.

`regenerate` **exits non-zero** whenever any adapter failed, naming them, even
though the rest succeeded. Before v3.11.0 it exited 0 as long as one adapter
worked, so a rejected permission rule scrolled past as a warning above a `✓`
line and looked like success
([#1346](https://github.com/phaedrus1992/llmenv/issues/1346)).

`export` is the exception: it runs on every prompt through the shell hook, so a
partial failure there stays exit 0 — failing would break your prompt for as
long as the config is bad, and the vars the other engines produced are still
correct. It prints one summary line naming the engines whose output is missing,
and `llmenv regenerate` will show the full error.

## `hook`

```text
llmenv hook <zsh|bash>
```

Print shell integration code for the given shell. Add `eval "$(llmenv hook zsh)"`
(or `bash`) to your shell profile. The emitted hook calls `llmenv export` on each
prompt.

## `status`

```text
llmenv status [bundles|tags|scopes|mcps|marketplaces|plugins|read-once|all]
```

Show the current environment status: active scopes and tags, and whether the
config parses. With a subcommand, show a detailed listing for that category:

- `status bundles` — list configured bundles, marking those that fire for the
  current environment.
- `status tags` — list all tags across scopes and contributors, marking active
  and orphaned tags.
- `status scopes` — list configured scopes (network/host/user/content/project),
  marking which are active and which are orphaned. `content` scopes joined
  this listing in v3.10.0 — they were previously omitted entirely (#845).
- `status mcps` — list MCP servers selected for the current environment, with
  each server's resolved role and transport (stdio / http / sse).
- `status marketplaces` — list configured plugin marketplaces, marking those
  referenced by selected plugins.
- `status plugins` — list configured plugins, marking those selected by the
  active scope and showing their source collection.
- `status read-once` — show the read-once file dedup cache entries.
- `status all` — show every section above.

## `statusline`

```text
llmenv statusline
```

Render an ANSI-styled status line. Reads the engine's session JSON from
stdin, config from `config.yaml`'s `statusline:` section (see
[Configuration reference](configuration.md#statusline)), and llmenv's own
stats from the materialized `llmenv-status.json`, then prints one line per
configured row to stdout.

Not meant to be invoked manually — it's wired automatically as the engine's
statusline hook (Claude Code seeds it into `settings.json` on first
materialization; Crush has no statusline hook to wire it into yet). Never
fails on missing/malformed input: unknown widgets, a missing data file, or
unparseable stdin all degrade to an empty render for that widget rather than
an error.

### Broken config renders an error row

(added in v3.8.0)

A `config.yaml` that can't be loaded or parsed is the one failure that does
*not* degrade to empty. Instead of rendering nothing, the statusline prints a
single row naming the problem and the remedy:

```text
⚠️ llmenv: config error — run 'llmenv doctor'
```

The command still exits 0, so the engine keeps rendering the status line. The
row deliberately omits the underlying parse error — it's multi-line and
arbitrarily long, where a status line is one short row. Run
[`llmenv doctor`](#doctor) to see the actual error and its location.

Previously a config parse error exited non-zero with empty stdout, so the
statusline silently vanished from every open terminal with the real error
going only to a stderr the engine discards — leaving no signal that the
config was broken.

## `context`

```text
llmenv context [--bundle NAME] [--why]
```

Show the resolved environment and active scopes in detail — the fuller view
behind `status`, including which contributors fired.

- `--bundle NAME` narrows the view to a single named bundle, showing its env
  vars, hooks (with event, matcher, type, and handler), MCPs, plugins, and skills.
- `--why` shows activation tracing: which scope triggered each active tag, and
  which tags caused each bundle to fire.

## `validate`

```text
llmenv validate
```

Check the config for structural issues. Reports duplicate bundle names, a
project marker whose `enable_bundles` or `disable_bundles` names an unknown
bundle, an unknown engine id in `disabled_engines`, and a `native_*` key that
names an unknown engine. Exits non-zero if any of these is found.

## `edit`

```text
llmenv edit [BUNDLE-NAME]
```

Open `config.yaml` (or, if `BUNDLE-NAME` is given, the matching
`bundles/<name>.yaml` file) in `$EDITOR`. Falls back to `$VISUAL`, then `vi`.

## `completions`

```text
llmenv completions [SHELL] [--install] [--dir DIR] [--force]
```

Generate shell completion scripts for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. With no flags,
prints the script to stdout — pipe it to a file your shell loads at startup:

```sh
# zsh — add to your .zshrc or drop into $fpath
llmenv completions zsh > ~/.zfunc/_llmenv

# bash — add to your .bashrc
llmenv completions bash > ~/.local/share/bash-completion/completions/llmenv

# fish
llmenv completions fish > ~/.config/fish/completions/llmenv.fish
```

(added in v3.8.0) `--install` writes the script to the shell's standard
completion directory instead, so you don't need to know the path yourself:

```sh
llmenv completions --install              # detect $SHELL, install to the standard location
llmenv completions zsh --install          # install for a specific shell
llmenv completions --install --dir DIR    # install to a custom directory
llmenv completions --install --force      # overwrite an existing completion file
```

Standard locations: `$BASH_COMPLETION_USER_DIR/completions/` (falling back to
`~/.local/share/bash-completion/completions/`) for bash, `$ZSH_CUSTOM/completions/`
(falling back to `~/.zsh/completions/`) for zsh, and `~/.config/fish/completions/`
for fish. Refuses to overwrite an existing file unless `--force` is passed.
Restart your shell (or `exec $SHELL`) afterward — for zsh, add the printed
`fpath+=(...)` line to `~/.zshrc` first, before `compinit`.

## `plugin-sync`

```text
llmenv plugin-sync
```

Sync plugin marketplaces into the cache — clone git sources that are missing,
fast-forward those already present. Local-path marketplaces are used in place and
need no sync.

(changed in v3.12.1) A local-path marketplace whose directory is missing on this host is skipped with a warning.
The sync goes on to the marketplaces declared after it.
Before v3.12.1 the sync stopped at the first missing path.

(changed in v3.12.0) A plugin whose marketplace entry pins a `ref` or a `sha` is cloned again on each sync, so a changed
pin takes effect.
Before v3.12.0 the sync kept the old checkout and reported success.
A plugin with no pin is pulled.
The sync also reads the `github` and `git-subdir` plugin sources, and skips a malformed entry with a warning.
See [Plugins](plugins.md#plugin-sources-in-a-marketplace-manifest).

## `sync`

```text
llmenv sync [--dry-run]
```

Sync the config repository with GitHub: `git add`, `commit`, and `push` the
config directory. Use this to propagate config changes to other hosts.

- `--dry-run` previews pending changes (`git status --short`) without committing
  or pushing.

## `check-stale`

```text
llmenv check-stale [--auto-fix]
```

Warn if the running agent's config has drifted from what llmenv would
materialize now. Run automatically at Claude Code session start by
`llmenv hook-run session_start` (a separate `SessionStart` hook before v3.11.0): it
compares the content hash in the booted `CLAUDE_CONFIG_DIR` against the
freshly-computed one and prints a restart hint on drift. Safe to run manually.

- `--auto-fix` re-materializes the config automatically on drift instead of only
  printing a warning.

## `hook-run`

```text
llmenv hook-run <event>
```

Engine-neutral lifecycle hooks that inject ICM memory context over MCP and
drive [`session_log:`](configuration.md#session_log). Invoked by the agent
runtime (not by users directly).

Lifecycle/memory events (`session_start`, `session_end`, and `post_model_switch` are always registered
by the Claude Code adapter; `turn_start` needs a memory backend, and the adaptive
recall events also need `adaptive_recall` on):

- `session_start` — injects the session wake-up pack (`icm_wake_up`); with
  `adaptive_recall` (added in v3.12.0), also injects the scope-tagged memories
  and resets the per-session recall state after a compaction or `/clear`; also
  creates the correlated ICM transcript session and emits the baseline
  `lifecycle_start` + scope-header session-log events. Before v3.12.0, Claude
  Code fetched the wake-up pack but never showed it to the model. (changed in
  v3.12.0) In Claude Code and opencode the block starts with
  `[ICM MEMORY CONTEXT (session start)]`, so it reads as different from the
  per-prompt `[ICM MEMORY CONTEXT (auto-injected)]` recall. A resumed or forked
  session (`source` `resume` or `fork`) makes no `icm_wake_up` call, because
  the conversation already holds the earlier pack. opencode receives the block
  in its first message; Crush runs no `SessionStart` hook.
- `turn_start` — with `adaptive_recall` (changed in v3.12.0), injects memories
  that match the prompt and recent session activity, plus related topics, and
  skips memories already sent in this context; without it, injects the
  scope-tagged recall on every prompt: a project-scoped recall for the active
  tags, plus one project-unfiltered recall per active tag keyed on
  `llmenv-tag:<tag>` and one per active bundle keyed on `llmenv-bundle:<bundle>`
- `post_tool_batch` (added in v3.12.0) — records the tools, files, and commands
  of a batch in the per-session recall state; no output
- `post_tool_use_failure` (added in v3.12.0) — records the error and injects
  memories about it
- `subagent_start` (added in v3.12.0) — injects memories that match the
  subagent's task, which `subagent_task` records
- `subagent_task` (added in v3.12.0) — a `PreToolUse` hook on the `Agent` tool
  that queues the subagent's task text; no output
- `post_model_switch` (added in v3.12.0) — a Claude Code `PostModelSwitch` hook. It records the new
  model and the switch in the session's agent-config document (see
  [Agent config](#agent-config)); no output
- `session_end` — best-effort store of the active scope context
  (`icm_memory_store`); also emits the baseline `lifecycle_end` session-log event

`session_start` also does this work (added in v3.12.0):

- It checks that each managed MCP server answers an MCP `initialize` within 5 seconds, restarts a stopped ICM proxy on
  the host that serves memory, and puts a `MCP health check failed` notice in the session context for each server that
  stays down.
  See [Session-start health check](mcp.md#session-start-health-check-added-in-v3120).
- It runs the background jobs that left a checkpoint file, up to 3 attempts each.
  See [Background work](troubleshooting.md#background-work-that-did-not-finish).
- It writes the session's [agent-config document](#agent-config).

Other events:

- `pre_tool_use` — runs the read-once dedup on `Read`.
  With the task tracker on, it also redirects the engine task tools to `llmenv task` and denies the first `git commit`
  or `gh pr create` with no task in progress (see [Task nudges](#task-nudges-added-in-v3120)).
  With `codebase-memory-mcp` wired, it denies an `index_repository` call that sets a project name, sets `persistence:
  true`, or reaches outside the allowed roots.
- `stop` — registered when session logging, `features.task_tracker`, or slippage `self_critique` is on.
  It prints the task reminders and the slippage self-critique.

Verbose events (auto-registered only when `session_log.verbose: true`):
`user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `notification`, `stop`,
`subagent_stop`, `pre_compact` — each captures the corresponding Claude Code
hook payload (prompt text, tool name + input/response, notification message,
etc.) as a session-log event.

Each hook talks to the configured ICM MCP over HTTP. Failures degrade
gracefully: a missing or unreachable backend logs a warning and exits cleanly
(exit code 0) so lifecycle hooks never block the agent. The session-log file
sink is independent of MCP reachability — it still writes even when ICM is
down. Per-event transcript records dispatch via a short-lived detached child
(`llmenv session-log-record`, internal plumbing) so `hook-run` itself never
blocks on the network round trip.

## `memory`

```text
llmenv memory stats|list|diff|prune [--dry-run]
```

Inspect ICM memory state for the active scope.

- `memory stats` — record counts by tag/bundle/type, last-written.
- `memory list` — list stored memories for the active scope.
  (changed in v3.12.0) It asks ICM for the memories of the project that the current folder belongs to.
  llmenv names the project like ICM does: the `origin` remote's repository name, else the main repository's folder name,
  else the folder name.
  When llmenv cannot tell the project, it warns `cannot tell the project of this folder, so memories of all projects are
  used` and lists every project.
  Before v3.12.0 ICM filtered by the working directory of the ICM server, which is unrelated to your project when ICM
  runs on another host.
- `memory diff` — show what changed since the last session.
  (changed in v3.12.0) It compares the same project-scoped recall as `memory list` and gives the same warning.
- `memory prune [--dry-run]` — preview or apply forgetting by memory importance.
  It forgets `low` and `medium` importance memories and keeps `high` and `critical` ones.
  It reads at most 100 memories for each run.
  (changed in v3.12.0) It reads only the memories of the project that the current folder belongs to.
  When llmenv cannot tell the project, the command refuses to run and forgets nothing.
  Before v3.12.0 the command had no project filter, so ICM answered with the records of its own working folder.
  `--dry-run` prints the counts and forgets nothing.
  (changed in v3.12.0) The command refuses to run, and forgets nothing, while the active `features.memory` entry sets `retention`.
  ICM's recall output has no record age or type, so llmenv cannot apply the per-type durations.
  Remove `retention` from that entry to use the importance-based prune.
  A `retention` on an entry that is not active does not block the prune.

## `prune`

```text
llmenv prune [--all] [--older-than DUR] [--dry-run]
```

Clean stale cache folders. Exits non-zero if any plugin cache entry could not
be removed (added in v3.11.0) — the per-entry failures are printed above the
summary.

- (no flags) — remove folders from previous binary versions and orphaned `*.tmp`
  staging dirs.
- `--all` — remove **every** cache folder unconditionally (next `export`
  re-materializes).
- `--older-than DUR` — remove only current-version folders older than `DUR`
  (e.g. `14d`, `1w`).
- `--dry-run` — preview deletions without removing (works with `--all` and
  `--older-than`).
- `--plugin-cache` — also remove the shared plugin cache directory.

## `read-once`

```text
llmenv read-once clear
```

Manage the read-once file dedup cache (#318). `read-once clear` clears all
cached read-once entries — use after reorganizing bundle content to force
re-ingestion on the next turn.

## `task`

```text
llmenv task add <title> [--child-of SLUG | --parallel] [--after SLUG] [--parent SLUG] [--session <id>]
  [--detail <text> | --detail-file <path>]
llmenv task start <id> [--force] [--reopen]
llmenv task done <id> [--force]
llmenv task reopen <id>...
llmenv task wait <id> [reason]
llmenv task ls [--format json] (--session <id> | --all) [--current-project]
llmenv task show <id> | --current | --next
llmenv task note <id> [text]
llmenv task block <id> --on <other>
llmenv task edit <id> [--title <t>] [--parent SLUG | --no-parent]
  [--block-on <id>]... [--unblock <id>]... [--add-note <text>] [--delete-note <index-or-timestamp>]
  [--detail <text> | --detail-file <path>]
llmenv task clear <id>... | --session <id>
llmenv task session start [name] [--description <text>] [--resume <id> | --replace | --new]
  [--task <title>]...
  [--context <text> | --context-file <path>] [--issue <n>]... [--branch <b>] [--base <b>]
  [--memory-topic <t>]... [--doc <path>]...
llmenv task session edit [<id>] [same flags as session start]
llmenv task session note [text] [--id <id>]
llmenv task session finish [<id>] [--abandon-open]
llmenv task session show [<id>]
llmenv task session summary [<id>] [--format json]
llmenv task session ls
```

In-engine task tracker (#231): durable, cross-session "what am I working on"
state, backed by one JSON file per task. `<id>` accepts an exact slug or any
unambiguous prefix of one.
The caller's open session is searched first.
A bare slug then reaches that session's task, even when a finished session left a task with the same slug.
Only when the open session has no match does the search cover the whole project.

- `task add <title> [--child-of SLUG | --parallel] [--after SLUG] [--parent SLUG] [--session <id>]` — create
  a task (`open` state). (changed in v3.12.0) A new task joins the **queue** of its session (tasks run in creation
  order): it cannot start
  until the task ahead of it is `done` or `waiting`, and until no other queued task is in progress.
  `--child-of SLUG` makes it a **sub-task** instead. Sub-tasks run in parallel, starting one puts every `open`
  ancestor in progress (a queued ancestor must be allowed to start, and a `done` parent refuses). The parent
  cannot be marked `done` before every sub-task is. A sub-task needs an
  unfinished parent in its own session. `--parallel` takes a top-level task out of the queue, so it runs beside the
  head. `--after SLUG` records that another task must be done first, which `task start` enforces like
  `task block`. `--parent SLUG` only links the task for display. The two flags `--child-of` and `--parallel` conflict,
  and `--child-of` conflicts with `--parent`. Before v3.12.0 a plain `task add` chained onto the previous
  task, and `--no-parent` opted out. A task no longer chains, and `--no-parent` is accepted, warns, and does
  nothing. Tasks stored before v3.12.0 keep their parent
  as a display link and count as top-level tasks.
  **A task must belong to a session** (see below): with exactly one session open
  for the current project it auto-resolves; with two or more open it picks
  the one this conversation started or resumed (changed in v3.12.0; see
  "Session ownership" below), else asks for `--session <id>`; errors with
  actionable guidance when none is open. `--detail <text>` or
  `--detail-file <path>` (added in v3.12.0) stores what a cold reader needs to do
  the task: files, acceptance criteria, and gotchas. The two flags conflict.
  An unreadable `--detail-file` fails before llmenv adds the task.
- `task start <id> [--force] [--reopen]` — claim a task, moving it to `wip`. Also the
  resume action for a `waiting` task — it accepts any non-`done` state as its
  starting point. An undone **`blocked_on`** reference (`task block`,
  below) refuses to start, since that's an explicit dependency. (changed in v3.12.0) An `open`
  queued task also refuses to start while the task ahead of it is not `done` or `waiting`, or while
  another queued task is in progress; the error names that task. Sub-tasks and `--parallel` tasks are not
  in the queue. Pass `--force` to override. A
  `blocked_on` reference resolves as done only once the target task *and
  every one of its descendants* are done, so blocking on a parent task alone
  covers its whole child set (see `task block`, below). `--reopen` (added
  in v3.12.0) moves a `done` task back to `open` with a note, then starts
  it; without it, `start` refuses a `done` task.
- `task done <id> [--force]` — mark a task complete. (changed in v3.12.0) Refuses a parent whose sub-tasks are
  not all `done`, and lists them; `--force` closes it anyway and prints a note.
  Refuses a task that was never started (`open` straight to `done`) and exits
  non-zero, because that jump means no work was tracked. Run `task start`
  first. Pass `--force` when the work is done without tracking; it prints a
  note that the start was skipped. A task in `wip` or `waiting` completes
  as before, and `done` on a `done` task stays a no-op. The native-tool
  redirect (`TaskUpdate`, `TodoWrite`) never forces: it returns the refusal as
  the tool result.
- `task reopen <id>...` — undo `task done`. (added in v3.12.0) Moves each named `done` task back to `open`.
  The task keeps its notes, parent, and `blocked_on` links, and gets a note that records the reopen.
  The call changes nothing and exits non-zero if any named task is not `done`, and the error names each such task.
  It also refuses a sub-task whose parent is `done`, unless you name the parent in the same call.
  To reopen one task and start it in one step, use `task start <id> --reopen`.
- `task wait <id> [reason]` — mark a task `waiting` on something outside the
  agent's control (a human review, a decision, external system access)
  instead of `wip`. `reason` is recorded as a note; reads from stdin if
  omitted. Distinct from `wip` in how the lifecycle reminders (below) treat
  it: a `wip` task is surfaced on every Stop and pushed toward action, while a
  `waiting` task is silent on Stop — it appears only in the SessionStart
  reminder, as a plain FYI with no "take action" framing, since the correct
  behavior is to wait for the reason to clear, not keep retrying (and
  re-injecting the FYI every turn would just nag about a state meant to be
  quiet).
- `task ls [--format json] (--session <id> | --all) [--state <s>]...
  [--hide-done] [--current-project]` — list tasks. **Requires `--session <id>`
  or `--all`** (added in v3.8.0) — no silent default to every session's
  tasks; pass `--all` to deliberately see everything. The human output groups
  tasks by session (current-project sessions first), indents subtasks under
  their parent, prefixes each row with a state glyph + label
  (`open`/`wip`/`waiting`/`done`), and annotates blocked tasks with their
  `blocked_on` refs; color follows TTY / `NO_COLOR` / `CLICOLOR_FORCE`.
  `--format json` is the stable machine format.
  `--state <open|wip|waiting|done>` (repeatable) keeps only those states; `--hide-done`
  (alias `--active`) drops completed tasks; `--current-project` (added in
  v3.8.0) further narrows to tasks whose session is tagged to the current
  project — any session ever tagged to it, open or closed, so a finished
  session's tasks still show — but doesn't substitute for `--session`/`--all`,
  since it narrows by project, not by session. Tasks with no session are
  excluded under `--current-project`. Filters compose with each other, and
  apply to the JSON output too when passed.
- `task show <id>` — full detail for one task (notes, parent, blockers, and the
  `detail` text when the task has one).
  `task show --current` / `task show --next` (added in v3.8.0, mutually
  exclusive with each other and with `<id>`) resolve the task in progress for
  the current project instead of naming one: `--current` is the `wip` task
  (falling back to the most recently updated non-`done` task) in each open
  session for the current project; `--next` is the next actionable task after
  it, in the same parent-before-children order `task ls` displays, skipping
  `done` tasks and any task whose `blocked_on` refs aren't all `done`. A
  single open session prints the same bare JSON as `task show <id>`; two or
  more each get a `# <name> (<id>)` header, separated by a `---` rule. Errors
  if no session is open for the current project. `--current` (changed in
  v3.12.0) also prints each session's resume context on stderr, so the JSON on
  stdout stays machine-readable.
- `task note <id> [text]` — append a progress note; reads from stdin if
  `text` is omitted.
- `task block <id> --on <other>` — record that `id` is blocked on `other`: a
  hard ordering dependency (see `task start`, above) — prefer this over
  relying on `--parent` nesting to imply an order it doesn't actually
  enforce. For a downstream step that must wait on a whole set of sibling
  tasks (e.g. several parallel analyzer tasks under one parent step), block
  on the **parent** rather than hand-wiring a `block` edge to each sibling —
  a `blocked_on` reference isn't satisfied until the target task *and every
  one of its descendants* are done. (changed in v3.12.0) The blocked task's
  own subtree doesn't count, so a sub-task blocked on a sibling can start once the sibling is done.
- `task edit <id> [--title <t>] [--parent SLUG | --no-parent] [--block-on
  <id>]... [--unblock <id>]... [--add-note <text>] [--delete-note
  <index-or-timestamp>]` — mutate an existing task. (added in v3.10.0) Every
  flag is optional and independent; an `edit` with none of them is a no-op
  that still bumps the task's `updated_at`. `--parent`/`--no-parent` re-parent
  or detach the task (same conflict as `task add`'s flags) and reject a change
  that would make the task its own ancestor. `--block-on`/`--unblock`
  (repeatable) add or remove `blocked_on` dependencies, idempotently — adding
  an already-present id or removing an absent one is a no-op, not an error.
  `--add-note` appends a note (reads from stdin if given as an empty string,
  e.g. `--add-note ''`); `--delete-note` removes one by its 0-based index in
  `task show`'s `notes` array, or by its exact `at` timestamp. `--detail
  <text>` or `--detail-file <path>` (added in v3.12.0) replaces the task's
  detail. An empty `--detail ''` clears it.
- `task clear <id>...` / `task clear --session <id>` — delete task(s)
  outright, for a batch that's being deliberately abandoned rather than just
  detached from a session (that's what `session start --replace` does,
  below). Exactly one of explicit ids or `--session` is required.

### Task sessions (#905)

**Sessions are mandatory**: every task belongs to one, and a session is
tagged with the project it was started in (resolved from the git root, else
a `.llmenv.yaml` marker, else the cwd). The task/session store stays global
per engine — `task ls --all` can show everything — but `task add`'s auto-resolve and
`session start`'s checkpoint scope to the current project's open sessions, so
two windows in the same project can't silently collide. Any number of
sessions may be open at once. The SessionStart/Stop `wip`/`waiting` lifecycle
reminders (below) are likewise scoped to the current project's sessions, so a
task from a different project sharing this store never nags the wrong
project's hook.

- `task session start [name] [--description <text>] [--resume <id> |
  --replace | --new] [--task <title>]...` — start a session for the current project. `--task`
  (added in v3.12.0) adds a task as the session starts and can repeat; the tasks join the queue in the
  order given, so a session never exists with no tasks. Pass
  `--description` to attach free-text context (e.g. "dev-sprint issue 493"),
  shown in `session ls` and the checkpoint; it's separate from `name` and
  never feeds id generation. **Name the session after the high-level work**
  (e.g. `oauth-token-refresh`, `v3.6.1-task-tracker-fixes`), not a placeholder
  — an omitted or auto-numbered name (`session-2`, `session-3`) defeats the
  point of `session ls` as the recovery path after a compaction. If one or
  more sessions are already open for this project, the command **errors and
  lists them** (id, name, description, idle time), requiring one of:
  - `--resume <id>` — adopt an existing open session instead of creating a
    new one (e.g. after a context compaction wiped the agent's memory of it);
    no new id is generated.
  - `--replace` — abandon every open session for this project (untagging
    their still-incomplete tasks with an orphan note; already-`done` tasks
    keep their tag as a historical record), then start fresh.
  - `--new` — create a new session anyway, leaving the existing one(s) open
    — true concurrency for two windows genuinely working in parallel.

  Tasks created with `task add` while a session is open are tagged with it
  permanently, so a task's session membership reflects when it was created.

  **Session ownership** (added in v3.12.0): `session start` records the
  engine conversation (`CLAUDE_CODE_SESSION_ID`) and engine process
  (`CLAUDE_PID`) that started the session; `--resume` moves it to the
  resuming conversation. When two or more sessions are open, `task add`,
  `session finish`, `session show`, and `session summary` without an id pick
  the one this conversation owns. The checkpoint error marks a session as
  yours when this conversation started it, or when this same engine process
  started it under an earlier conversation id — the state after a `/clear`
  or a compaction — so `--resume` is the safe choice instead of `--new`.
  Outside an engine (no such variables) nothing is recorded, and resolution
  works as before.
  (changed in v3.12.0) A session id must be a plain name.
  A `--resume`, `finish`, `edit`, or `--id` value with a path part, such as `../x`, is rejected.
- `task session finish [<id>] [--abandon-open]` — close out a session;
  auto-resolves when exactly one is open for the current project, or to the one
  this conversation owns, otherwise pass an id. (changed in v3.12.0) Refuses
  and exits non-zero while any task in the session is `open`, `wip`, or
  `waiting`, and lists each one. Finish those tasks, drop one with `task
  clear`, or pass `--abandon-open` to untag them (each gets a note) and finish
  anyway; the output lists what it dropped. A task that is `done` keeps its
  session tag as a historical record.
- `task session show [<id>]` — print a session's progress; auto-resolves
  like `finish`.
- `task session summary [<id>] [--format json]` — (added in v3.10.0) roll up
  a session's tasks, notes, and states into one artifact — e.g. for a memory
  write or a status report at the end of a session. Auto-resolves like
  `finish`. The human format prints a header (name or id, description,
  done/total) followed by each task's state glyph and notes, in the same
  parent-before-children order `task ls` groups a session's tasks in.
  `--format json` is the stable, memory-ingestion-friendly form: session
  metadata plus an array of tasks (slug/title/state/parent/blocked_on/notes).
  (changed in v3.12.0) Both forms also carry the session's resume context, as
  a `resume` object in JSON, and each task's `detail` text.
- `task session ls` — list every currently open session (id, name, project,
  description), current-project matches first. This is the recovery path
  after a compaction: with one session open for the project there's exactly
  one match to resume.

When every task in an open session is done, the SessionStart/Stop hook
reminders (below) nudge the agent to run `task session finish` or add more
work to the session instead. A session that still has an open task is never
offered `session finish`; the reminder says to start the next task.

The CLI subcommands always work. The injected `llmenv` skill guidance and
the SessionStart/Stop lifecycle reminders are gated behind
`features.task_tracker.enabled` (default `false`). Each `wip` task in a
reminder is tagged with the session that started it; since a hook has no
reliable way to tell whether that session is *this* conversation's own (two
terminals in the same project is a normal pattern), the reminder never
presumes ownership — it conditions resuming/finishing a task on the agent
recognizing it as its own earlier work. The Stop hook is the exception when
the agent has a conversation id (see
[Stop reminder rules](#stop-reminder-rules)). Separately, once every task in an
open session is done, the reminder nudges to close out that session or add
more work to it (see above), likewise conditioned on recognizing it. (added
in v3.12.0) On Stop, an open session that holds `open` tasks but no `wip` or
`waiting` task gets one line naming its next task to `task start` — an agent
that adds steps and never starts them otherwise gets no reminder at all:

```yaml
features:
  task_tracker:
    enabled: true
```

With the tracker enabled, llmenv also **redirects Claude Code's built-in task
tools** (`TaskCreate`/`TaskList`/`TaskUpdate`) into this tracker via an
auto-injected `PreToolUse` hook, so a skill or agent that reaches for the native
tools still lands durable tasks here rather than Claude's ephemeral per-session
state. `TaskCreate` records a task (auto-starting a session when none is open),
`TaskList` returns the tracker's view, and `TaskUpdate` maps its status to
start/done/delete. The native tool is suppressed and the agent is told the
`llmenv task` id to use for follow-up. The native tool is suppressed and the
agent is told the `llmenv task` id to use for follow-up; the redirect is off
when the tracker is disabled. (#985)

opencode's built-in todo list is redirected the same way (added in v3.11.0).
Its one tool, `todowrite`, replaces the whole list on every call, so llmenv
reconciles rather than applying a single operation: todos are matched to tracked
tasks **by title** (opencode's todo ids are per-session and mean nothing to the
tracker), a title that isn't tracked yet is added, `in_progress` starts a task,
and `completed` finishes it. Resending an unchanged list is a no-op, which
matters because opencode resends everything on every edit.

A tracked task that disappears from the array is **left open**. opencode sends
no tombstone, so "finished", "abandoned", and "the model rewrote the list and
forgot one" are indistinguishable — closing on that signal would silently lose
work. The reply says how many tasks were dropped so you can close them with
`llmenv task done <id>` if they really are finished. opencode has no `todoread`
tool (reading happens through session state and the UI, not a tool call), so
there is nothing to intercept on the read side. (#1304)

Set `features.task_tracker.block_engine_task_tools: false` (added in v3.10.0,
default `true`) to keep the CLAUDE.md fragment and reminders while letting
Claude's native Task tools through unblocked — for example, when a project
genuinely uses them for multi-agent teammate coordination rather than solo step
tracking. See [`features.task_tracker:`](configuration.md#featurestask_tracker)
for the full field reference. (#980)

### Core task rules (added in v3.12.0)

While the tracker is on, SessionStart injects a short statement of the tracking rules with the exact commands: open a
session when the work has more than one part, give a session its tasks, do one queued task at a time, and park a task that
waits for the user. The text lives in llmenv itself, so it needs no bundle or personal instruction. It also says that the
engine task tools are redirected to `llmenv task` while `block_engine_task_tools` is on, and that it overrides an
instruction that says they are blocked. `features.task_tracker.nudges: false` removes the text.
`llmenv doctor` warns about an instruction line that says the task tools are blocked or forbids `llmenv task`.

### Stop reminder rules

(added in v3.13.0)

The Stop reminder ends the turn with the state of the task tracker.
A reminder that returns the same text on every Stop makes the agent answer it again and again.
Three rules prevent that loop.

- **A Stop that a Stop hook caused gets no reminder.**
  Claude Code sets `stop_hook_active: true` in the Stop payload for such a stop.
  llmenv then returns no text for every Stop reminder source, including the slippage self-critique.
- **An unchanged reminder is shown once.**
  llmenv keeps a hash of the last reminder it emitted for the conversation, under `<state dir>/stop_dedupe/`.
  An identical reminder is not emitted again.
  A different reminder, an empty reminder, or a new user prompt makes the next reminder due.
  A damaged state file costs at most one extra reminder.
- **A reminder names only the sessions of the agent that stops.**
  When the Stop payload has a conversation id, the reminder covers only the sessions that this conversation
  started or resumed with `llmenv task session start`.
  A session that another conversation owns is not named.
  A session with no recorded owner is still named.
  After `/clear` the conversation id changes, so run `llmenv task session start --resume <id>` to take the session back.
  When the payload has no conversation id, the reminder covers every session of the project, as before.

The SessionStart reminder is not narrowed.
It still lists every open session of the project, because a new conversation finds its own earlier work there.

### Task nudges (added in v3.12.0)

Changed in v3.13.0: only file edits and writes count toward the nudge, and a queued task gets a start reminder.

The tracker reminds the agent while work happens, and not only at the start and the end of a session.
Each reminder names the exact `llmenv task` commands to run.
`features.task_tracker.nudges: false` turns off the reminders in the first, second, and fourth items,
and `enforce_commit: false` turns off the fifth.
Nothing turns off the third item.

- After a workflow skill starts (`dev-sprint`, `ship-issue`, and the others in `workflow_skills`), a project with no
  session or no unfinished task gets one reminder for each session to start a session with its first tasks.
- After `nudge_after` (default 8) file edits or writes with no unfinished task, the agent gets a nudge.
  Later nudges come every `nudge_every` (default 20) edits. Shell commands do not count. The count resets once a task exists.
- When a file edit runs with tasks queued and none in progress, the agent gets one reminder to run
  `llmenv task start <slug>` for the next open task. It does not repeat until a task starts.
- When a session has open tasks and none is in progress, the Stop reminder names the next task,
  whether or not the turn ends with a question.
  It offers `llmenv task start <slug>` to begin the step, and `llmenv task wait <slug> "<reason>"` for a step that needs
  the user or an outside event first. The first reminder carries both commands.
- An open session with no task at all is named in the Stop and SessionStart reminders.
  It is an error state: add a task, or finish the session.
- When the agent asks the user a question (the `AskUserQuestion` tool, or a turn that ends with `?`) while a task is
  in progress, the reminder tells it to run `llmenv task wait <slug> "<reason>"`, and `llmenv task start <slug>` after
  the answer. A waiting task is reported as "waiting on the user".
- The first `git commit` or `gh pr create` of a session with no task in progress is denied once, with the commands to
  run. A `llmenv task done` or `llmenv task wait` earlier in the same command does not count as a task in
  progress. The same command runs on the next try.
  The deny marker clears the next time a commit or pull request runs while a task is in progress, so a later gap with no
  task denies once more.
  A task that is only `open` or `waiting` does not count as in progress.

Two more reminders do not depend on `nudges`.
A reminder for a parent task in progress shows how many of its sub-tasks are done, in progress, waiting, and not started.
The Stop reminder names the next sub-task to start when a parent is in progress and its sub-tasks are all `open` or `done`.

The tracker looks at the whole project: a task in progress in any open session of the project counts as tracked work.
The deny is once for each session, not once for each commit, and a failed state write lets the command through.
The hooks register on Claude Code. When session logging already routes every tool call to `hook-run`, the tracker adds no
second entry. See [`features.task_tracker:`](configuration.md#featurestask_tracker) for the fields. (#2456)

### Resume context (added in v3.12.0)

A session can record what a fresh agent needs to pick the work up after `/clear`.
This record is the resume context.
It holds free-text notes, issue numbers, the git branch and the base branch, ICM memory topics, and plan documents.

Set it when you start the session:

```text
llmenv task session start sprint --context "pick up at step 4" \
  --issue 2337 --doc docs/design/x.md --memory-topic decisions-llmenv \
  --branch feat/2337-foo --base release/3.x
```

- `--context <text>` or `--context-file <path>` sets the notes. The two flags conflict.
- `--issue <n>`, `--memory-topic <topic>`, and `--doc <path>` each repeat.
- `--branch` and `--base` set the branches.

`session start` fills in what it can detect.
It reads the branch from git.
It reads an issue number from a branch name such as `feat/2337-foo` or `fix/2358`.
It never overwrites a value you set.
A resumed session (`--resume`) keeps what it has, and only the flags you pass change it.

Change the context after the session starts:

- `task session edit [<id>]` takes the same flags.
  A single-value flag replaces its value.
  A repeatable flag adds the entries the session lacks.
  The command picks the session the same way as `finish`.
- `task session note [text] [--id <id>]` adds a line to the notes. It reads stdin when you omit `text`.

The context appears in these places:

- `task session show` and `task session summary` print it. In JSON, it is the `resume` object.
- The SessionStart reminder lists the context of each open session in the current project.
  It names the command for each reference: `gh issue view N`, and `icm_memory_recall` with the stored topics.
  The reminder does not claim that the session is yours.
  It labels the notes as data, not instructions, and it cuts each session at 2,000 characters.
  The SessionStart and Stop reminders need `features.task_tracker.enabled`.
- `task show --current` prints it on stderr.

When a session has no notes, no issue, no doc, and no memory topic, `session start` prints one line that names the flags.
The Stop reminder repeats that line for a session that has unfinished tasks.
A branch alone does not count, because it does not say what the work is.

llmenv removes control characters from this text before it prints the text.
A state file from before v3.12.0 loads without these fields.

### Agent config

(added in v3.12.0)

Every Claude Code `SessionStart` writes a small JSON document at `<state dir>/agent_config/<session id>.json`.
It records what the session runs as: the engine, the model, the effort level, the working directory,
the project, the active tags and bundles, the booted config hash, and the llmenv and engine versions.
Only the owner can read it.
llmenv removes documents older than seven days.

- A `PostModelSwitch` hook (Claude Code 2.1.251 and later) updates `model`.
  It also adds an entry to `model_history`, which keeps the newest 20 switches.
- On `resume` and `compact`, the SessionStart context starts with one line:
  `[llmenv session] engine claude_code, model claude-opus-5, effort high, project llmenv, tags a, b, config 0123456789ab`.
  A `startup`, `clear`, or `fork` session gets no line, because it has no earlier context to lose.
- `effort` comes from the hook payload, so it reads `unset` when Claude Code sends none.
- When the session's owner has a document, `task session summary` prints
  `running as <engine> <model>, effort <level>` under the title.
  The JSON form carries an `agent` object.

## `login`

```text
llmenv login [--global]
```

Capture Claude Code auth credentials and store them in the llmenv auth cache.
Runs `claude auth login` in a temporary directory, extracts the resulting
`oauthAccount`, and saves it so new materialized folders inherit it automatically.

The OAuth token is captured too, not just the account identity (added in
v3.8.0) — so an inheriting folder is actually logged in rather than merely
knowing which account you use. See
[Inherited Claude Code state](configuration.md#oauth-credential-inheritance).

- (no flags) — if `CLAUDE_CONFIG_DIR` is set and managed by llmenv, updates both
  that folder's auth and the global cache. Otherwise falls back to global-only
  (same as `--global`) and prints a note directing you to run `llmenv export` first.
- `--global` — store credentials in the user-level Claude config (`~/.claude/`)
  rather than the project cache. Use this when `CLAUDE_CONFIG_DIR` is not set or
  not managed by llmenv.

`llmenv init` includes auth setup; use `llmenv login` to authenticate separately
or to re-authenticate.

## `setup`

```text
llmenv setup [PATH] [--repo URL] [--no-launch] [--rescan]
```

Interactive setup wizard for new llmenv users. Walks through auth setup (login
fresh via `claude auth login`, import from `~/.claude`, or skip) and settings
import (choose which keys to seed from your global `settings.json` into the
materialized config). Writes a template `config.yaml` and an agent orientation
guide, then optionally hands off to the AI engine for further configuration.

- `--no-launch` skips the AI engine handoff at the end.
- `--rescan` re-scans existing configs without overwriting files.

## `config-context`

```text
llmenv config-context
```

Print source config paths as agent context (used by the auto-registered
`SessionStart` hook). Prints the paths of `config.yaml` and the `bundles/`
directory so the agent knows where to direct config edits. Invoked automatically — not normally run by users.

## `config-guard`

```text
llmenv config-guard
```

Warn when the agent tries to write a managed cache path (used by the
auto-registered `PreToolUse` hook with matcher `Write|Edit|MultiEdit`). Checks
whether the target path is inside the llmenv cache and prints a redirection hint
pointing at the source config. Always exits 0 (fail-soft — the write is not
blocked). Invoked automatically — not normally run by users.

## `throttle`

```text
llmenv throttle <pre-tool|prompt>
```

Poll the usage backend and sleep an adaptive delay, to stay under rate limits.
The auto-registered `PreToolUse` (`pre-tool`) and `UserPromptSubmit` (`prompt`) hooks run it when a `features.throttle`
block is set.
Invoked automatically — not normally run by users.
See [`features.throttle:`](configuration.md#featuresthrottle).

## `upgrade`

```text
llmenv upgrade [--check] [--track beta|release]
```

Upgrade llmenv to the latest version from GitHub releases. Downloads the
platform-appropriate pre-built binary, performs a safe install cycle
(backup → write temp → sync → rename → verify → remove backup), and
restores the original binary on failure.

- `--check` compares the current version against the latest release and
  prints the result. Exits 1 if an update is available.
- `--track beta` uses the first non-draft GitHub release instead of the
  latest stable release. The track can be configured persistently via
  `features.upgrade.track` in `config.yaml`:

  ```yaml
  features:
    upgrade:
      track: beta    # "release" (default) or "beta"
  ```

Supported platforms: macOS (aarch64, x86_64), Linux (aarch64, x86_64).

## `doctor`

```text
llmenv doctor [--gc] [--all] [--probe-mcp] [--restart-memory-proxy]
```

(added in v3.12.0) `--restart-memory-proxy` skips the checks. It stops the local memory proxy that the
pidfile names and starts it again; see [Troubleshooting](troubleshooting.md#memory-backend-issues).
It signals the pid in the pidfile only when that process is an `mcp-proxy`, and it exits non-zero when the proxy does
not stop within 5 seconds, when the pid belongs to another program, or when the new proxy does not start.
On a host that does not serve memory it prints an info line and does nothing.

Validate adapter wiring and configuration. By default runs checks only for the
active context (active bundles, active MCP servers, etc.). Checks:

- config parsing
- cache directory writability
- git connectivity
- orphans — scopes/tags/bundles/MCP/plugins that can never activate, a memory
  `server_host` missing from `host:`, unknown fields in project markers, and a
  network scope whose `match` sets none of `gateway_mac`, `ssid`, or `cidr` (added in v3.8.0;
  before v3.12.0 it also flagged a scope with only `ssid` or `cidr`, which were not evaluated)
- network scopes (added in v3.12.0) — when a scope matches on `ssid`, doctor reads the Wi-Fi
  SSID and warns when the platform hides it (macOS 15 prints `<redacted>`), because such a
  scope never matches there. See [Network match fields](configuration.md#network-match-fields)
- cleartext MCP URLs (added in v3.12.0) — warns when the hostname of an `http://` MCP URL
  resolves to a public address. See [MCP](mcp.md)
- lifecycle hooks (added in v3.11.0) — lists which lifecycle events
  (`session_start`, `session_end`, `post_model_switch`, `turn_start`, `post_tool_batch`,
  `post_tool_use_failure`, `subagent_start`, `stop`) are wired for
  `claude_code` in the active scope, and for any that aren't, what would enable
  them. `session_start`/`session_end`/`post_model_switch` are always registered; `turn_start` needs
  a memory backend, and the three adaptive recall events (added in v3.12.0)
  also need `adaptive_recall` on;
  `stop` needs session logging or `features.task_tracker`.
  `turn_start`'s gate is read straight from the generator; the others are
  derived separately and held in step by a test that renders `settings.json`
  for each combination and fails if the report disagrees.
- instruction size (added in v3.12.0) — measures the text Claude Code loads into
  every session: `CLAUDE.md` plus every rule file without a `paths:` filter.
  Doctor prints the total, the `CLAUDE.md` size, and the five largest
  contributors by bundle. It warns when one file is over 40,000 characters or
  the total is over 80,000. The 40,000 figure is the floor of Claude Code's
  per-file notice. Claude Code does not document the combined limit, so 80,000
  is an estimate and the warning says so. Rules with a `paths:` list load only
  on matching files and are counted by number, not size. Doctor measures the
  merged config, which is what the next `llmenv regenerate` writes. Fix a large
  file in its source bundle, not in the generated copy.
- MCP text limits (added in v3.12.0) — Claude Code cuts each MCP tool description and each
  server's `initialize` instructions to 2,048 characters, and the cut is silent.
  Doctor asks each HTTP server for its instructions and tool list (`initialize` and `tools/list`
  only, never a tool call) and warns for each item over the limit.
  `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` changes the limit; doctor reads it from the
  environment or `native.claude_code.env`.
  A server that does not answer in 5 seconds is reported as "not measured", not as a warning.
  Doctor does not start stdio servers unless you pass `--probe-mcp`, because starting one can
  have side effects. SSE servers are not probed.
  The fix belongs to the server's owner, because llmenv does not change a server's text.
- dependent-tool versions (added in v3.11.0) — reports the installed version of
  the external tools llmenv wires in but doesn't ship (`icm`,
  `codebase-memory-mcp`) and how to update each. `icm upgrade --apply` installs
  its own update; `codebase-memory-mcp update` only prints the install command
  for your machine, so llmenv reports it rather than claiming it updates
  anything. Offline by design: no "an update is available" claim is made, since
  checking would mean a network round trip per tool on every run. Tools that
  aren't installed are skipped — the tool-availability checks above already
  report those. (added in v3.11.2) A `codebase-memory-mcp` older than 0.11.0
  gets a warning: llmenv's guidance names tools that release added
  (`get_file_outline`, `compare_graphs`), and the first index after upgrading
  rebuilds each project once.
- dead `native_<feature>.<engine>` keys (added in v3.8.0) — warns when a key in
  `native_permissions`, `native_hooks`, `native_plugins`, `native_mcp`,
  `native_model_providers`, or `native` names no registered engine (a typo), or
  names an engine whose adapter never reads that map (e.g.
  `native_model_providers.claude_code`, `native_hooks.opencode`). Either way the
  block parses and merges but is never rendered. Checked against the merged
  config, so bundle-contributed keys are covered. `llmenv export` and
  `llmenv regenerate` warn about the same thing, as does
  `llmenv check-stale --auto-fix` (since v3.10.0 — it re-materializes too, but
  didn't run this check before then, #1075), and `llmenv validate` fails on an
  unknown engine id. See
  [Engines](engines.md#engine-keys-are-validated).
- Claude-only permission patterns under opencode (added in v3.8.0) — warns when a
  `capabilities.permissions` pattern uses Claude Code's colon-prefix syntax
  (a trailing `:*` command prefix like `git commit:*`, or a `domain:`/`url:`
  field filter) while opencode is also installed and enabled. opencode matches a
  pattern as a plain glob, so the rule never applies there. A dead `deny` is
  called out specially: it fails open, so the thing it was written to block
  isn't blocked. Use a space-separated pattern (`git commit *`) for a rule both
  engines honour, or move the Claude-only form to
  `native_permissions.claude_code`. `llmenv export` and `llmenv regenerate`
  report this too, as does `llmenv check-stale --auto-fix` since v3.10.0
  (#1075).
- legacy shell tools without their recommended replacement (added in v3.8.0) —
  warns when `capabilities.permissions.allow` grants `grep`/`find` without also
  granting `rg`/`fd`, the replacements this project's own bundled rules
  recommend — a nudge toward `capabilities.permissions.preset: safe-readonly`
  even without adopting it. `doctor`-only: unlike the two checks above, an
  `allow`d legacy tool with no replacement is working config, not something
  silently dropped, so `export`/`regenerate` (sourced on every shell prompt)
  don't report it. See
  [Configuration](configuration.md#capabilities).
- glob-shaped hook matchers — warns when a `hook.matcher` looks like a
  file-extension glob (e.g. `*.rs`, `.py`) instead of a tool-name pattern;
  Claude Code matches `hook.matcher` against tool name only, never file path,
  so such a matcher silently never fires. Use a `scope.content` glob to gate
  the hook's bundle by file type instead.
- token-efficiency settings — warns when `BASH_MAX_OUTPUT_LENGTH`,
  `MAX_MCP_OUTPUT_TOKENS`, and `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` are not set.
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` must be a whole number from 1 to 100, and doctor warns
  above 70 because PreCompact hooks then have too little room to run (changed in v3.12.0).
  The autocompact check reads `native.claude_code` `autoCompactEnabled` and `autoCompactWindow` too
  (changed in v3.12.0): with `autoCompactEnabled: false` it reports info and
  recommends nothing, and with a window set it names the window the percentage
  applies to. A `bashOutputMaxChars` setting replaces the `BASH_MAX_OUTPUT_LENGTH` check, because
  Claude Code ignores the variable while the setting is set. The prompt-cache check reads
  `CLAUDE_CODE_PROMPT_CACHE_TTL` first, then `ENABLE_PROMPT_CACHING_1H`, and prints info, not a
  warning, when neither is set, because subscription plans get the 1-hour TTL without a variable
  (changed in v3.12.0; details in
  [Troubleshooting](troubleshooting.md#doctor-warns-about-retired-claude-code-settings)). It reports (info) whether
  `CLAUDE_CODE_SUBAGENT_MODEL` is set; and checks whether a context-mode MCP
  server is registered
- retired Claude Code settings (added in v3.12.0) — reads the rendered `settings.json` and `.claude.json`
  in the folder that `CLAUDE_CONFIG_DIR` names, and prints a `Retired Claude Code settings:` section for
  each retired settings key, environment variable, permission tool, or MCP server type.
  It skips the check, and says so, when `CLAUDE_CONFIG_DIR` is not an llmenv folder.
  It only warns. See
  [Troubleshooting](troubleshooting.md#doctor-warns-about-retired-claude-code-settings).
- task tracker instructions (added in v3.12.0) — with `features.task_tracker.enabled`, prints a
  `Task tracker instructions:` section. It warns about each paragraph of `CLAUDE.md` or of a rule
  that says the engine task tools are blocked, or that forbids `llmenv task`, and names the bundle or rule file.
  A paragraph that says the tools are redirected is not a hit. The check is a word match, so it can miss
  an unusual wording. See [Core task rules](#core-task-rules-added-in-v3120).
- MCP servers (added in v3.12.0) — sends each managed server (the memory server and
  `codebase-memory-mcp`) an MCP `initialize` with a 5 second limit, and prints one line for each.
  A server that is down gets the reason and the fix. Nothing prints when the scope has no managed server.
  See [Session-start health check](mcp.md#session-start-health-check-added-in-v3120).
- ICM server version (added in v3.12.0) — on the host that serves memory, reads the version of the local
  `icm` binary and warns below 0.10.60 and below 0.10.64. On a memory client the version is unknown, and
  doctor says to run `icm --version` on the server host. See
  [Troubleshooting](troubleshooting.md#memory-backend-issues).
- codebase-memory index (added in v3.12.0) — with one active `features.codebase_memory` entry, reports the
  result of the last index of this project: the finish time, a warning with the log path when the index
  failed, and a warning with the numbers and the `mem_budget_mb` to set when it stopped at the memory
  budget. It then lists the roots that `codebase-memory-mcp` may index, and warns about a root that
  llmenv wants and the server lacks, or about an `allowed_roots` entry that cannot expand or is not a folder.
  With two or more active entries it says that no index runs. See
  [MCP & Memory](mcp.md#codebase-memory-codebase_memory).
- background work (added in v3.12.0) — prints a `Background work:` section with one line for each
  checkpoint of a detached job that did not finish. A stale checkpoint is a warning. See
  [Troubleshooting](troubleshooting.md#background-work-that-did-not-finish).
- cached OAuth credential (added in v3.8.0) — reports whether a token is cached
  in the durable state dir, and warns when the cached token has expired. See
  [Inherited Claude Code state](configuration.md#oauth-credential-inheritance).

- `--all` runs the full orphan analysis across the entire config (all bundles and
  scopes, not just active ones).
- `--gc` runs cache garbage collection after the diagnostics. On macOS this also
  drops the keychain credential item belonging to each cache folder it deletes
  (added in v3.8.0); matched by folder path, so your default `~/.claude` login is
  never affected.
- `--probe-mcp` (added in v3.12.0) also starts the stdio MCP servers, to measure their text for the
  MCP text limits check. Starting a server can have side effects, so doctor does it only on request.
- `--restart-memory-proxy` (added in v3.12.0) replaces the checks; see above.

## Deprecated commands

The following top-level listing commands are hidden shims that print a
deprecation warning and delegate to `status <subcommand>`. Use the
`status` equivalents directly:

| Deprecated | Replacement |
| --- | --- |
| `llmenv scope-ls` | `llmenv status scopes` |
| `llmenv tag-ls` | `llmenv status tags` |
| `llmenv bundle-ls` | `llmenv status bundles` |
| `llmenv mcp-ls` | `llmenv status mcps` |
| `llmenv marketplace-ls` | `llmenv status marketplaces` |
| `llmenv plugin-ls` | `llmenv status plugins` |
