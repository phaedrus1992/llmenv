<!-- markdownlint-disable MD013 -->

# Plugins & Marketplaces

llmenv can wire agent plugins into the materialized config. Plugins are sourced
from **marketplaces** and grouped into **plugin collections** that are selected
onto scopes by tag — the same model as bundles and MCP servers.

## Marketplaces

(added in v1.0.0)

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
| ------------- | -------------- | ---------- |
| `https://`, `ssh://`, `git+ssh://` | git | Cloned into the cache |
| `http://`, `git://`, `file:`, `<helper>::<address>`, `ext::...` | git | Rejected when the clone starts (see below) |
| `git@host:owner/repo` (scp-style) | git | Cloned into the cache |
| `/abs`, `~/path`, `./rel`, `../rel`, bare relative | path | Used in place |

`http://` and `git://` are plaintext and unauthenticated.
A `<helper>::` source runs a `git-remote-<helper>` program.
llmenv rejects all of these, and a source that starts with `-`, with an error that names the source.
(changed in v3.12.0) `git://` and the `<helper>::` form are now rejected for plugin sources in a marketplace manifest, as `ext::` and `http://` were before.

A git marketplace `source` can end in `#<ref>` to pin a branch, tag, or commit.
A pinned source is never pulled.
`plugin-sync` re-clones it, so a changed `#<ref>` takes effect on the next sync.
(changed in v3.12.0) The re-clone goes into a staging folder first.
llmenv swaps it into place only after the clone succeeds, and it puts the old clone back if the swap fails.
A failed sync leaves the working clone as it was.

Git marketplaces are cloned once into `<cache_dir>/marketplaces/<name>/`, shared
across every scope, and refreshed by [`plugin-sync`](#syncing). The resolved git
HEAD is mixed into the materialized scope's content hash, so a marketplace update
re-renders the agent config. Local-path marketplaces are content-hashed by their
current state and need no sync.

### Plugin sources in a marketplace manifest

(added in v3.12.0)

A plugin entry in a marketplace's `.claude-plugin/marketplace.json` has a `source`.
llmenv reads these forms:

| `source` | Result |
| --- | --- |
| `"./path"` | A plugin inside the marketplace clone. |
| `"https://host/repo.git"` | A separate clone under `plugin-payloads/`. |
| `{"source": "url", "url": ...}` | The same as a plain URL. |
| `{"source": "github", "repo": "owner/name"}` | A clone of `https://github.com/owner/name.git`. |
| `{"source": "git-subdir", "url": ..., "path": ...}` | A clone of the repo. The plugin is the folder `path`. |
| `{"source": "npm", "package": ..., "version": ...}` | Left to the engine, which installs it from npm. |

`url`, `github`, and `git-subdir` sources accept a `ref` and a `sha`.
A `git-subdir` `url` is a clone URL or `owner/name`.
A `sha` must be a full commit id (40 hex digits, or 64 in a SHA-256 repo).
A `sha` or `ref` that is not a string is an error, not an unpinned clone.
llmenv fetches that one commit, so the plugin does not move when the branch does.
When an object has a `sha` and a `ref`, the `sha` wins.
A `ref` selects a branch or tag, and no pin uses the default branch.
When an object has both `url` and `repo`, `url` wins.
`plugin-sync` re-clones a pinned plugin, so a changed `ref` or `sha` takes effect on the next sync.
An unpinned plugin is pulled.

A malformed entry is skipped with a warning that names the entry, the value, and the fix.
This covers a `repo` that is not `owner/name`, a `sha` that is not a full commit id,
a `ref` that is empty or unsafe, and a `path` that is not a relative folder inside the repo.
The `archive` and `command` kinds are skipped with a warning that names the kind, because llmenv does not fetch them.
An object with neither `url` nor a github `repo` is skipped with a warning.

## Plugin collections

A `plugin-collection` is a named bag of plugins that activates by tag:

```yaml
plugin-collection:
  - name: dev
    when: [me]
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
A marketplace or plugin source with a pin (`#<ref>`, `ref`, or `sha`) is re-cloned instead of pulled.
Run it after adding a marketplace or to pull upstream plugin updates. Local-path
marketplaces are skipped (they're read in place).

## Inspecting

```bash
llmenv status marketplaces    # marketplaces, marking those referenced by selected plugins
llmenv status plugins         # plugins, marking those the active scope selects
```

`llmenv doctor` flags plugin orphans: a collection no scope can select, a
marketplace no selectable collection references, and a plugin referencing an
undeclared marketplace.
