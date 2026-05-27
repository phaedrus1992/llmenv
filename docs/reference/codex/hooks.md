# Hooks

Codex supports a `[hooks]` table in `config.toml`, but with a **narrower model**
than Claude Code: events are a subset, and **only command handlers run** (prompt
and agent handler types are parsed but skipped).

## Events

Codex recognizes these hook events (subset of Claude Code's ~25):

- `PreToolUse`
- `PermissionRequest`
- `PostToolUse`
- `PreCompact` / `PostCompact`
- `SessionStart`
- `SubagentStart` / `SubagentStop`
- `UserPromptSubmit`
- `Stop`

## Handler types

Only **command** handlers execute. The config parser accepts other handler
shapes for forward-compatibility but silently skips them — so a generated hook
must be a command.

```toml
[hooks]
# command hooks only; the TOML alias `command_windows` is also accepted
# for a Windows-specific command line.
```

Hooks are subject to trust: untrusted projects skip project-scoped hooks; user
and system hooks still load.

## Gaps vs llmenv

llmenv already copies hook files in bundles (`hooks/*.json`, with `{{ICM_MCP}}`
substitution) — but those are **Claude Code-shaped JSON** registered via
`settings.json`. For Codex:

- Hooks are **TOML, in `config.toml`**, command-only. The Claude Code JSON hook
  artifacts are **not portable** — a `CodexAdapter` can't just byte-copy them.
- The adapter would need to either (a) generate `[hooks]` entries from a
  Codex-aware source, or (b) skip hooks for Codex initially (matching the current
  Claude Code reality where hooks are copied but inert — see issue #34).
- Event-name and handler-type differences mean any cross-agent hook abstraction
  in llmenv must down-map to Codex's command-only subset and drop unsupported
  events/handlers.
