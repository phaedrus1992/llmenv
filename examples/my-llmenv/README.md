# my-llmenv — Example Configuration

This is a fully-annotated example of a real-world `my-llmenv` repository. It
shows how all the moving parts fit together: scopes, bundles, rules, skills,
hooks, plugin collections, and the ICM memory backend.

## What is `my-llmenv`?

`my-llmenv` is your **personal fork** of your llmenv configuration. Instead of
editing the managed cache directly (which gets overwritten on every `llmenv
regenerate`), you keep your configuration source in a git repository that you
own, then point llmenv at it:

```yaml
# ~/.config/llmenv/config.yaml  ← managed by llmenv, points here
source: ~/git/my-llmenv
```

Everything under `my-llmenv/` is **your** config. llmenv reads it, merges
capability fragments from bundle.yaml files, and materializes the final agent
config into the cache directory.

## Layout

```
config.yaml                 ← top-level: cache, scope, bundle, plugin-collection,
                               mcp, memory. THE entry point. Read this first.
bundles/<name>/             ← one directory per bundle
  AGENTS.md                 ← rules injected into every session (CLAUDE.md equiv.)
  bundle.yaml               ← capability fragment: hooks, permissions, plugins
                               that this bundle contributes (merged at materialize)
  rules/<topic>.md          ← topic-specific rules (injected alongside AGENTS.md)
  skills/<name>/SKILL.md    ← invocable skills (slash commands in Claude Code)
  hooks/<name>.sh           ← shell scripts fired by hook events
  scripts/<name>.sh         ← helper scripts called by skills (not hooks)
  commands/<name>.md        ← slash-command definitions
scopes/                     ← (optional) scope overrides; most scopes live in
                               config.yaml's `scope:` block
```

## How the pieces connect

```
                ┌─────────────────────────────────────────────────┐
                │  llmenv regenerate                               │
                │                                                  │
                │  1. Reads config.yaml                            │
                │  2. Evaluates scope conditions (hostname, user,  │
                │     SSID) → active tags                          │
                │  3. Selects bundles whose `when:` tags match     │
                │  4. Merges capability fragments (bundle.yaml)     │
                │  5. Writes final agent config to cache dir        │
                └────────────────────────┬────────────────────────┘
                                         │
                                         ▼
          ┌──────────────────────────────────────────────────────────┐
          │  Agent config (materialized)                             │
          │                                                          │
          │  • AGENTS.md  — concatenation of all active AGENTS.md   │
          │                 files (base + any matching bundles)      │
          │  • Hooks      — merged list from all active bundle.yaml  │
          │  • Plugins    — from plugin-collection entries whose     │
          │                 when: tags matched                       │
          │  • MCP servers — from mcp: entries whose when: matched  │
          │  • Permissions — merged allow/deny lists                 │
          └──────────────────────────────────────────────────────────┘
```

### Scope → Tag → Bundle chain (example)

```
hostname == "work-laptop.local"
    → scope `work-laptop` fires
    → emits tags: [host-work-laptop, work]

tag `work` matches:
    → bundle `work` loads        (Slack integration, work permissions)
    → plugin-collection `work` loads (claude-plugins-official:slack)
```

### Project-level tag injection (`.llmenv.yaml`)

Any project can emit its own tags by placing a `.llmenv.yaml` at its root:

```yaml
# myproject/.llmenv.yaml
tags: [lang-rust, kubernetes]
```

This causes `rust-dev` and `kubernetes` bundles to activate when Claude Code
is opened inside that project directory. The agent gets Rust rules, the
build-check skill, and Kubernetes manifests rules — scoped to that project
only.

## Editing workflow

```bash
# After any change:
llmenv doctor    # validate config syntax + bundle structure
llmenv status    # show which scopes and bundles are active right now
llmenv regenerate  # write new agent config to cache (Claude Code picks it up)
```

## Sync (optional)

Because this is a git repo you can push it to a private remote and pull it on
all your machines — changes propagate via `git pull && llmenv regenerate`.
