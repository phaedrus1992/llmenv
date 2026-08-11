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

```text
llmenv export [--scope ID] [--tag TAG] [--explain] [--compress]
```

Resolve the current environment and print shell `export` lines. This is what the
shell hook runs on every prompt. It also materializes the agent config directory
and emits the introspection env vars (`LLMENV_ACTIVE_*`, `LLMENV_PROJECT_ROOT`,
`LLMENV_ICM_CONTEXT`) and the adapter's pointer var (`CLAUDE_CONFIG_DIR`).

- `--tag TAG` filters to bundles carrying that tag.
- `--scope ID` is accepted but scope filtering is not yet implemented (prints a
  warning and exports all matching tags).
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

## `hook`

```text
llmenv hook <zsh|bash>
```

Print shell integration code for the given shell. Add `eval "$(llmenv hook zsh)"`
(or `bash`) to your shell profile. The emitted hook calls `llmenv export` on each
prompt.

## `status`

```text
llmenv status [bundles|tags|scopes|mcps|marketplaces|plugins]
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
llmenv context [--bundle NAME] [--why] [--json]
```

Show the resolved environment and active scopes in detail — the fuller view
behind `status`, including which contributors fired.

- `--bundle NAME` narrows the view to a single named bundle, showing its env
  vars, hooks (with event, matcher, type, and handler), MCPs, plugins, and skills.
- `--why` shows activation tracing: which scope triggered each active tag, and
  which tags caused each bundle to fire.
- `--json` emits the full context as machine-readable JSON.

## `validate`

```text
llmenv validate
```

Check the config for structural issues. Reports duplicate bundle names. Exits
non-zero if any issues are found.

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

Generate shell completion scripts for `bash`, `zsh`, or `fish`. With no flags,
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
materialize now. Invoked automatically by the Claude Code `SessionStart` hook: it
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

Lifecycle/memory events (`session_start`, `session_end` are auto-registered by
the Claude Code adapter; `turn_start` is not yet wired in, see
[#499](https://github.com/phaedrus1992/llmenv/issues/499)):

- `session_start` — injects the session wake-up pack (`icm_wake_up`); also
  creates the correlated ICM transcript session and emits the baseline
  `lifecycle_start` + scope-header session-log events
- `turn_start` — injects recalled context (`icm_memory_recall`): a project-scoped
  recall for the active tags, plus one project-unfiltered recall per active tag
  keyed on `llmenv-tag:<tag>` and one per active bundle keyed on
  `llmenv-bundle:<bundle>`, so tag and bundle memory crosses project boundaries
- `session_end` — best-effort store of the active scope context
  (`icm_memory_store`); also emits the baseline `lifecycle_end` session-log event

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
- `memory diff` — show what changed since the last session.
- `memory prune [--dry-run]` — preview or apply TTL-based forgetting.

## `prune`

```text
llmenv prune [--all] [--older-than DUR] [--dry-run]
```

Clean stale cache folders.

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
llmenv task add <title> [--parent SLUG | --no-parent] [--session <id>]
llmenv task start <id> [--force]
llmenv task done <id>
llmenv task wait <id> [reason]
llmenv task ls [--format json] (--session <id> | --all) [--current-project]
llmenv task show <id> | --current | --next
llmenv task note <id> [text]
llmenv task block <id> --on <other>
llmenv task edit <id> [--title <t>] [--parent SLUG | --no-parent]
  [--block-on <id>]... [--unblock <id>]... [--add-note <text>] [--delete-note <index-or-timestamp>]
llmenv task clear <id>... | --session <id>
llmenv task session start [name] [--description <text>] [--resume <id> | --replace | --new]
llmenv task session finish [<id>]
llmenv task session show [<id>]
llmenv task session summary [<id>] [--format json]
llmenv task session ls
```

In-engine task tracker (#231): durable, cross-session "what am I working on"
state, backed by one JSON file per task. `<id>` accepts an exact slug or any
unambiguous prefix of one.

- `task add <title> [--parent SLUG | --no-parent] [--session <id>]` — create
  a task (`open` state). (added in v3.10.0) Omitting `--parent` no longer
  means "no parent": it defaults to the most recently *created* task in the
  same session, so a run of plain `task add`s forms an ordered chain by
  default — the order agents add tasks in is usually the order they intend
  to execute them. Pass `--parent SLUG` to nest under a specific task
  instead (bypassing the chain), or `--no-parent` to force a deliberate
  top-level task (the two flags conflict with each other). The chain never
  crosses sessions — a new session's first task always starts with no
  parent, regardless of what was last added in a different session. **A
  task must belong to a session** (see below): with exactly one session open
  for the current project it auto-resolves; pass `--session <id>` when two
  or more are open; errors with actionable guidance when none is open.
- `task start <id> [--force]` — claim a task, moving it to `wip`. Also the
  resume action for a `waiting` task — it accepts any non-`done` state as its
  starting point. `parent` and `blocked_on` (added in v3.8.0) are enforced
  differently: an undone **parent** only warns — organizational grouping,
  not an ordering guarantee, so starting a child while the parent is still
  open is often fine. An undone **`blocked_on`** reference (`task block`,
  below) hard-blocks — refuses to start — since that's an explicit
  dependency the user configured on purpose; pass `--force` to override. A
  `blocked_on` reference resolves as done only once the target task *and
  every one of its descendants* are done, so blocking on a parent task alone
  covers its whole child set (see `task block`, below).
- `task done <id>` — mark a task complete.
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
- `task show <id>` — full detail for one task (notes, parent, blockers).
  `task show --current` / `task show --next` (added in v3.8.0, mutually
  exclusive with each other and with `<id>`) resolve the task in progress for
  the current project instead of naming one: `--current` is the `wip` task
  (falling back to the most recently updated non-`done` task) in each open
  session for the current project; `--next` is the next actionable task after
  it, in the same parent-before-children order `task ls` displays, skipping
  `done` tasks and any task whose `blocked_on` refs aren't all `done`. A
  single open session prints the same bare JSON as `task show <id>`; two or
  more each get a `# <name> (<id>)` header, separated by a `---` rule. Errors
  if no session is open for the current project.
- `task note <id> [text]` — append a progress note; reads from stdin if
  `text` is omitted.
- `task block <id> --on <other>` — record that `id` is blocked on `other`: a
  hard ordering dependency (see `task start`, above) — prefer this over
  relying on `--parent` nesting to imply an order it doesn't actually
  enforce. For a downstream step that must wait on a whole set of sibling
  tasks (e.g. several parallel analyzer tasks under one parent step), block
  on the **parent** rather than hand-wiring a `block` edge to each sibling —
  a `blocked_on` reference isn't satisfied until the target task *and every
  one of its descendants* are done.
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
  `task show`'s `notes` array, or by its exact `at` timestamp.
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
  --replace | --new]` — start a session for the current project. Pass
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
- `task session finish [<id>]` — close out a session; auto-resolves when
  exactly one is open for the current project, otherwise pass an id. Never
  touches its tasks' session tag — a finished session (even with incomplete
  tasks) is a legitimate historical record.
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
- `task session ls` — list every currently open session (id, name, project,
  description), current-project matches first. This is the recovery path
  after a compaction: with one session open for the project there's exactly
  one match to resume.

When every task in an open session is done, the SessionStart/Stop hook
reminders (below) nudge the agent to run `task session finish` or add more
work to the session instead.

The CLI subcommands always work. The injected `llmenv` skill guidance and
the SessionStart/Stop lifecycle reminders are gated behind
`features.task_tracker.enabled` (default `false`). Each `wip` task in a
reminder is tagged with the session that started it; since a hook has no
reliable way to tell whether that session is *this* conversation's own (two
terminals in the same project is a normal pattern), the reminder never
presumes ownership — it conditions resuming/finishing a task on the agent
recognizing it as its own earlier work. Separately, once every task in an
open session is done, the reminder nudges to close out that session or add
more work to it (see above), likewise conditioned on recognizing it:

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
`llmenv task` id to use for follow-up. This is Claude-Code-specific (those tools
are Claude Code's) and off when the tracker is disabled. (#985)

Set `features.task_tracker.block_engine_task_tools: false` (added in v3.10.0,
default `true`) to keep the CLAUDE.md fragment and reminders while letting
Claude's native Task tools through unblocked — for example, when a project
genuinely uses them for multi-agent teammate coordination rather than solo step
tracking. See [`features.task_tracker:`](configuration.md#featurestask_tracker)
for the full field reference. (#980)

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
llmenv doctor [--gc] [--all] [--verbose]
```

Validate adapter wiring and configuration. By default runs checks only for the
active context (active bundles, active MCP servers, etc.). Checks:

- config parsing
- cache directory writability
- git connectivity
- orphans — scopes/tags/bundles/MCP/plugins that can never activate, a memory
  `server_host` missing from `host:`, unknown fields in project markers, and a
  network scope whose `match` has no `gateway_mac` (added in v3.8.0) — only
  `gateway_mac` is evaluated today, so `ssid`/`cidr` alone can never match
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
  `MAX_MCP_OUTPUT_TOKENS`, `ENABLE_PROMPT_CACHING_1H`, and
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` are not set; reports (info) whether
  `CLAUDE_CODE_SUBAGENT_MODEL` is set; and checks whether a context-mode MCP
  server is registered
- cached OAuth credential (added in v3.8.0) — reports whether a token is cached
  in the durable state dir, and warns when the cached token has expired. See
  [Inherited Claude Code state](configuration.md#oauth-credential-inheritance).

- `--all` runs the full orphan analysis across the entire config (all bundles and
  scopes, not just active ones).
- `--gc` runs cache garbage collection after the diagnostics. On macOS this also
  drops the keychain credential item belonging to each cache folder it deletes
  (added in v3.8.0); matched by folder path, so your default `~/.claude` login is
  never affected.
- `--verbose` prints detailed per-check reasoning alongside each pass/fail result.

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
