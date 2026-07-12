# Commands

Every command accepts `--color <auto|always|never>` (default `auto`). Run
`llmenv <command> --help` for the authoritative flag list. Global flags:
`-h/--help`, `-V/--version`.

## `init`

```
llmenv init [PATH] [--repo URL]
```

Initialize llmenv configuration. Writes a template `config.yaml` into the config
directory (or `PATH` if given). With `--repo URL`, clones an existing config
repository instead of writing a template. No-op if a config already exists.

## `export`

```
llmenv export [--scope ID] [--tag TAG] [--explain]
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

## `regenerate`

```
llmenv regenerate
```

Regenerate the materialized config without emitting shell `export` lines. Use
after editing `config.yaml` or bundle files when the current shell already has
the right env vars.

## `hook`

```
llmenv hook <zsh|bash>
```

Print shell integration code for the given shell. Add `eval "$(llmenv hook zsh)"`
(or `bash`) to your shell profile. The emitted hook calls `llmenv export` on each
prompt.

## `status`

```
llmenv status [bundles|tags|scopes|mcps|marketplaces|plugins]
```

Show the current environment status: active scopes and tags, and whether the
config parses. With a subcommand, show a detailed listing for that category:

- `status bundles` — list configured bundles, marking those that fire for the
  current environment.
- `status tags` — list all tags across scopes and contributors, marking active
  and orphaned tags.
- `status scopes` — list configured scopes (network/host/user/project), marking
  which are active and which are orphaned.
- `status mcps` — list MCP servers selected for the current environment, with
  each server's resolved role and transport (stdio / http / sse).
- `status marketplaces` — list configured plugin marketplaces, marking those
  referenced by selected plugins.
- `status plugins` — list configured plugins, marking those selected by the
  active scope and showing their source collection.

## `context`

```
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

```
llmenv validate
```

Check the config for structural issues. Reports duplicate bundle names. Exits
non-zero if any issues are found.

## `edit`

```
llmenv edit [BUNDLE-NAME]
```

Open `config.yaml` (or, if `BUNDLE-NAME` is given, the matching
`bundles/<name>.yaml` file) in `$EDITOR`. Falls back to `$VISUAL`, then `vi`.

## `completions`

```
llmenv completions <bash|zsh|fish>
```

Generate shell completion scripts. Pipe the output to a file your shell loads at
startup:

```sh
# zsh — add to your .zshrc or drop into $fpath
llmenv completions zsh > ~/.zfunc/_llmenv

# bash — add to your .bashrc
llmenv completions bash > ~/.local/share/bash-completion/completions/llmenv

# fish
llmenv completions fish > ~/.config/fish/completions/llmenv.fish
```

## `plugin-sync`

```
llmenv plugin-sync
```

Sync plugin marketplaces into the cache — clone git sources that are missing,
fast-forward those already present. Local-path marketplaces are used in place and
need no sync.

## `sync`

```
llmenv sync [--dry-run]
```

Sync the config repository with GitHub: `git add`, `commit`, and `push` the
config directory. Use this to propagate config changes to other hosts.

- `--dry-run` previews pending changes (`git status --short`) without committing
  or pushing.

## `check-stale`

```
llmenv check-stale [--auto-fix]
```

Warn if the running agent's config has drifted from what llmenv would
materialize now. Invoked automatically by the Claude Code `SessionStart` hook: it
compares the content hash in the booted `CLAUDE_CONFIG_DIR` against the
freshly-computed one and prints a restart hint on drift. Safe to run manually.

- `--auto-fix` re-materializes the config automatically on drift instead of only
  printing a warning.

## `hook-run`

```
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

## `prune`

```
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

## `login`

```
llmenv login [--global]
```

Capture Claude Code auth credentials and store them in the llmenv auth cache.
Runs `claude auth login` in a temporary directory, extracts the resulting
`oauthAccount`, and saves it so new materialized folders inherit it automatically.

- (no flags) — if `CLAUDE_CONFIG_DIR` is set and managed by llmenv, updates both
  that folder's auth and the global cache. Otherwise falls back to global-only
  (same as `--global`) and prints a note directing you to run `llmenv export` first.
- `--global` — store credentials in the user-level Claude config (`~/.claude/`)
  rather than the project cache. Use this when `CLAUDE_CONFIG_DIR` is not set or
  not managed by llmenv.

`llmenv init` includes auth setup; use `llmenv login` to authenticate separately
or to re-authenticate.

## `config-context`

```
llmenv config-context
```

Print source config paths as agent context (used by the auto-registered
`SessionStart` hook). Prints the paths of `config.yaml` and the `bundles/`
directory so the agent knows where to direct config edits. Invoked automatically — not normally run by users.

## `config-guard`

```
llmenv config-guard
```

Warn when the agent tries to write a managed cache path (used by the
auto-registered `PreToolUse` hook with matcher `Write|Edit|MultiEdit`). Checks
whether the target path is inside the llmenv cache and prints a redirection hint
pointing at the source config. Always exits 0 (fail-soft — the write is not
blocked). Invoked automatically — not normally run by users.

## `upgrade`

```
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

```
llmenv doctor [--gc] [--all] [--verbose]
```

Validate adapter wiring and configuration. By default runs checks only for the
active context (active bundles, active MCP servers, etc.). Checks:

- config parsing
- cache directory writability
- git connectivity
- orphans — scopes/tags/bundles/MCP/plugins that can never activate, a memory
  `server_host` missing from `host:`, and unknown fields in project markers
- token-efficiency settings — warns when `BASH_MAX_OUTPUT_LENGTH`,
  `MAX_MCP_OUTPUT_TOKENS`, `ENABLE_PROMPT_CACHING_1H`, and
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` are not set; reports (info) whether
  `CLAUDE_CODE_SUBAGENT_MODEL` is set; and checks whether a context-mode MCP
  server is registered

- `--all` runs the full orphan analysis across the entire config (all bundles and
  scopes, not just active ones).
- `--gc` runs cache garbage collection after the diagnostics.
- `--verbose` prints detailed per-check reasoning alongside each pass/fail result.

## Deprecated commands (removed in 2.1)

The following top-level listing commands are still accepted in 2.0.x as hidden
shims but will be removed in 2.1. Use the `status <subcommand>` equivalents
instead:

| Deprecated | Replacement |
|---|---|
| `llmenv scope-ls` | `llmenv status scopes` |
| `llmenv tag-ls` | `llmenv status tags` |
| `llmenv bundle-ls` | `llmenv status bundles` |
| `llmenv mcp-ls` | `llmenv status mcps` |
| `llmenv marketplace-ls` | `llmenv status marketplaces` |
| `llmenv plugin-ls` | `llmenv status plugins` |
