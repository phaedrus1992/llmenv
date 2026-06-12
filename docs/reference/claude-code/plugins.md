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

## How llmenv models plugins (#59)

llmenv declares marketplaces and plugin collections at the top level of
`config.yaml`, selected onto a scope by tag intersection — the same model as
`[[bundle]]` and `[[mcp]]`:

```yaml
marketplace:
  - name: superpowers
    source: "https://github.com/obra/superpowers"   # git URL → cloned
  - name: dev-commons
    source: "~/git/dev-commons"                      # local path → used in place

plugin-collection:
  - name: rust-tools
    when: [rust]
    plugins: ["superpowers:tdd", "dev-commons:rust-tooling"]  # marketplace:plugin
```

A collection fires when any of its `when` tags intersect the active scope tags. The
union of fired collections' plugins (deduplicated) is materialized.

**Marketplace caching.** Git sources are cloned once into
`<cache_dir>/marketplaces/<name>/` and shared across scopes; `llmenv plugin sync`
fast-forwards them. The resolved git HEAD is mixed into the materialized scope
hash so a marketplace update re-renders the scope. Local-path sources are used in
place (no clone). `llmenv export` never hits the network — it uses whatever is
already cached.

**Rendering.** The Claude Code adapter writes into `settings.json`:
- `extraKnownMarketplaces` — keyed by marketplace name, `source: directory`
  pointing at llmenv's local clone, so Claude loads the synced checkout instead
  of re-fetching. Unsynced marketplaces are skipped.
- `enabledPlugins` — keyed `<plugin>@<marketplace>`, all `true`. llmenv never
  authors a `false` (disabled) entry.

The internal model (`ResolvedPlugin { marketplace, plugin }` +
`ResolvedMarketplace { name, source, install_location, head }`) is engine-agnostic
so a future Codex adapter can render the same data into its own format.

- `strictPluginOnlyCustomization` (M) is notable: in an enterprise that sets it,
  user/project skills/agents/hooks/MCP are blocked — only plugins and managed
  settings work. Not relevant to a personal project, but it bounds where
  llmenv-style direct materialization is permitted.
