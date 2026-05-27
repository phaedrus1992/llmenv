# Plugins reference

Source: <https://code.claude.com/docs/en/plugins-reference>,
<https://code.claude.com/docs/en/plugin-marketplaces> (fetched 2026-05-27).

Plugins bundle skills, agents, hooks, MCP servers, commands, and LSP servers into
a distributable unit, installed from marketplaces.

## Plugin layout

```
my-plugin/
  plugin.json          # manifest
  commands/*.md
  agents/*.md
  skills/<name>/SKILL.md
  hooks/hooks.json
  .mcp.json
```

Components are auto-discovered. `${CLAUDE_PLUGIN_ROOT}` resolves to the plugin
root in paths.

## plugin.json

Manifest with `name` and optional component declarations, e.g. LSP servers:

```json
{
  "name": "my-plugin",
  "lspServers": {
    "go": {
      "command": "gopls",
      "args": ["serve"],
      "extensionToLanguage": { ".go": "go" }
    }
  }
}
```

Plugin agents support `name`, `description`, `model`, `effort`, `maxTurns`,
`disallowedTools`, but **not** `hooks`/`mcpServers`/`permissionMode` (security).

## Marketplaces (settings.json)

`extraKnownMarketplaces` (sources: `github`, `git`, `directory`, `hostPattern`,
inline `settings`), `enabledPlugins`, plus managed controls
`strictKnownMarketplaces`, `blockedMarketplaces`, `pluginTrustMessage`,
`strictPluginOnlyCustomization`. Each marketplace entry accepts `autoUpdate`.

Plugins are loadable from `.zip` archives and URLs (recent feature).

## Gaps vs llmenv

- **Entirely unmodeled and arguably out of scope.** llmenv *is itself* a config
  generator — it materializes the same component types plugins distribute
  (skills, agents, hooks, MCP). Whether llmenv should also generate
  `enabledPlugins` / `extraKnownMarketplaces` entries is a design question, not an
  obvious gap.
- Possible overlap to resolve in a design doc: a user might want llmenv to declare
  *which plugins are enabled* per scope (e.g. enable a rust plugin on rust
  projects). That maps to the `enabledPlugins` / marketplace settings keys and
  would ride on the `settings.json` generator.
- `strictPluginOnlyCustomization` (M) is notable: in an enterprise that sets it,
  user/project skills/agents/hooks/MCP are blocked — only plugins and managed
  settings work. Not relevant to a personal project, but it bounds where
  llmenv-style direct materialization is permitted.
