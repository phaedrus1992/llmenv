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
llmenv export [--scope ID] [--tag TAG]
```

Resolve the current environment and print shell `export` lines. This is what the
shell hook runs on every prompt. It also materializes the agent config directory
and emits the introspection env vars (`LLMENV_ACTIVE_*`, `LLMENV_PROJECT_ROOT`,
`LLMENV_ICM_CONTEXT`) and the adapter's pointer var (`CLAUDE_CONFIG_DIR`).

- `--tag TAG` filters to bundles carrying that tag.
- `--scope ID` is accepted but scope filtering is not yet implemented (prints a
  warning and exports all matching tags).

## `hook`

```
llmenv hook <zsh|bash>
```

Print shell integration code for the given shell. Add `eval "$(llmenv hook zsh)"`
(or `bash`) to your shell profile. The emitted hook calls `llmenv export` on each
prompt.

## `status`

```
llmenv status
```

Show the current environment status: active scopes and tags, and whether the
config parses.

## `context`

```
llmenv context
```

Show the resolved environment and active scopes in detail — the fuller view
behind `status`, including which contributors fired.

## `scope-ls`

```
llmenv scope-ls
```

Alias: `llmenv scopes`

List configured scopes (network/host/user/project), marking which are active and
which are orphaned (tags no contributor consumes).

## `tag-ls`

```
llmenv tag-ls
```

Alias: `llmenv tags`

List all tags across scopes and contributors, marking active and orphaned tags.

## `bundle-ls`

```
llmenv bundle-ls
```

Alias: `llmenv bundles`

List configured bundles, marking those that fire for the current environment.

## `mcp-ls`

```
llmenv mcp-ls
```

Alias: `llmenv mcps`

List the MCP servers selected for the current environment, with each server's
resolved role and transport (stdio / http / sse). Includes the memory backend
when active.

## `marketplace-ls`

```
llmenv marketplace-ls
```

Alias: `llmenv marketplaces`

List configured plugin marketplaces, marking those referenced by selected
plugins.

## `plugin-ls`

```
llmenv plugin-ls
```

Alias: `llmenv plugins`

List configured plugins, marking those selected by the active scope and showing
their source collection.

## `plugin-sync`

```
llmenv plugin-sync
```

Sync plugin marketplaces into the cache — clone git sources that are missing,
fast-forward those already present. Local-path marketplaces are used in place and
need no sync.

## `sync`

```
llmenv sync
```

Sync the config repository with GitHub: `git add`, `commit`, and `push` the
config directory. Use this to propagate config changes to other hosts.

## `check-stale`

```
llmenv check-stale
```

Warn if the running agent's config has drifted from what llmenv would
materialize now. Invoked automatically by the Claude Code `SessionStart` hook: it
compares the content hash in the booted `CLAUDE_CONFIG_DIR` against the
freshly-computed one and prints a restart hint on drift. Safe to run manually.

## `hook-run`

```
llmenv hook-run <session_start|turn_start|session_end>
```

Engine-neutral lifecycle hooks that inject ICM memory context over MCP. Invoked by
the agent runtime (not by users directly) in response to three neutral events:

- `session_start` — injects the session wake-up pack (`icm_wake_up`)
- `turn_start` — injects recalled context (`icm_memory_recall`): a project-scoped
  recall for the active tags, plus one project-unfiltered recall per active tag
  keyed on `llmenv-tag:<tag>` and one per active bundle keyed on
  `llmenv-bundle:<bundle>`, so tag and bundle memory crosses project boundaries
- `session_end` — best-effort store of the active scope context (`icm_memory_store`)

Each hook talks to the configured ICM memory MCP over HTTP. Failures degrade
gracefully: a missing or unreachable backend logs a warning and exits cleanly
(exit code 0) so lifecycle hooks never block the agent.

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

## `doctor`

```
llmenv doctor [--gc]
```

Validate adapter wiring and configuration. Checks config parsing, cache
writability, git connectivity, and orphans (scopes/tags/bundles/MCP/plugins that
can never activate, a memory `server_host` missing from `host:`, and unknown
fields in project markers). With `--gc`, runs cache garbage collection after the
diagnostics.
