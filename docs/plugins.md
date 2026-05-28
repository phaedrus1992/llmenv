# Plugins & Marketplaces

llmenv can wire agent plugins into the materialized config. Plugins are sourced
from **marketplaces** and grouped into **plugin collections** that are selected
onto scopes by tag — the same model as bundles and MCP servers.

## Marketplaces

A marketplace is a named source of plugins, declared at the top level:

```yaml
marketplace:
  - name: superpowers
    source: "https://github.com/obra/superpowers.git"
  - name: local-dev
    source: "~/code/my-plugins"
```

The `source` is classified automatically:

| Source form | Classified as | Behavior |
|-------------|--------------|----------|
| `https://`, `http://`, `ssh://`, `git://`, `git+ssh://` | git | Cloned into the cache |
| `git@host:owner/repo` (scp-style) | git | Cloned into the cache |
| `/abs`, `~/path`, `./rel`, `../rel`, bare relative | path | Used in place |

Git marketplaces are cloned once into `<cache_dir>/marketplaces/<name>/`, shared
across every scope, and refreshed by [`plugin-sync`](#syncing). The resolved git
HEAD is mixed into the materialized scope's content hash, so a marketplace update
re-renders the agent config. Local-path marketplaces are content-hashed by their
current state and need no sync.

## Plugin collections

A `plugin-collection` is a named bag of plugins that activates by tag:

```yaml
plugin-collection:
  - name: dev
    tags: [me]
    plugins:
      - "superpowers:caveman"      # <marketplace>:<plugin>
      - "superpowers:brainstorm"
```

Each entry is a `<marketplace>:<plugin>` reference, where the left half names a
declared marketplace. The union of all selected collections' plugins is what gets
wired up for the active environment.

You can also list plugins directly under `capabilities.plugins` (global) or in a
bundle's `bundle.yaml` — they merge with collection-selected plugins.

## Where plugins materialize

The Claude Code adapter renders selected plugins into `settings.json`:

- `extraKnownMarketplaces` — each referenced marketplace as a `directory` source
  pointing at its local clone under `<cache_dir>/marketplaces/<name>/`.
- `enabledPlugins` — each selected plugin as `plugin@marketplace`, all enabled.

## Syncing

```bash
llmenv plugin-sync
```

Clones any missing git marketplaces and fast-forwards those already present.
Run it after adding a marketplace or to pull upstream plugin updates. Local-path
marketplaces are skipped (they're read in place).

## Inspecting

```bash
llmenv marketplace-ls    # marketplaces, marking those referenced by selected plugins
llmenv plugin-ls         # plugins, marking those the active scope selects
```

`llmenv doctor` flags plugin orphans: a collection no scope can select, a
marketplace no selectable collection references, and a plugin referencing an
undeclared marketplace.
