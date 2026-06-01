---
sidebar_position: 1
slug: /philosophy
---

# Why llmenv?

## The Problem

AI coding agents like Claude Code have exactly one place to put their configuration: a global settings file. That file is always active, regardless of which network you're on, which machine you're using, or which repository you're in.

This leads to real friction:

- **Office vs. home**: Your office MCP server is only reachable on the office network, but you want it loaded automatically when you connect — not after remembering to toggle it.
- **Per-repo tooling**: Some projects use a custom memory backend or a specialized plugin. The global config means either everyone loads it everywhere, or nobody gets it.
- **Shared memory by language**: You want all your Rust projects to share a memory context, but your TypeScript projects to have their own. There's no hook for that today.

The root cause: agent config is *flat* and *global*, but developer context is *scoped* and *dynamic*.

## The Model

llmenv introduces a layer of indirection between your intentions and the agent's config:

```
scopes → tags → bundles → materialize → adapter emit
```

**Scopes** describe where you are. Four kinds:

| Scope kind | Matches on |
|---|---|
| `network` | gateway MAC address |
| `host` | hostname |
| `user` | `$USER` |
| `project` | a `.llmenv.yaml` marker file |

Each active scope contributes **tags** to the active set — arbitrary labels you define (`office`, `rust`, `me`).

**Contributors** (bundles, MCP servers, plugins, memory) select on those tags. If *any* of a contributor's tags is in the active set, it fires.

The result is **materialized** into a content-hashed config directory. The adapter (e.g. the Claude Code adapter) emits agent-native files (`settings.json`, `mcp.json`, `CLAUDE.md`) into it and sets the agent's config pointer there.

The shell hook re-evaluates on every prompt. Move networks, move into a new repo — the right config follows you, automatically.

## Design Principles

### Agent-native and transparent

llmenv writes real files the agent already understands. No plugin, no API integration, no patched binary. The materialized directory is inspectable with `ls`. `llmenv check-stale` tells you if a running agent's config is stale. `llmenv doctor` validates end-to-end.

### Engine-neutral capability model

Capabilities are declared once in an engine-neutral vocabulary (`capabilities.permissions`, `capabilities.hooks`, `capabilities.plugins`). The adapter knows how to translate them into the agent's format. If a capability can't be modeled generically, `native.*` pass-through lets you reach engine-specific fields without sacrificing the ability to run against multiple agents.

### Content-hashed materialization

The materialized folder is named after your binary version (or, in strict mode, a content hash of the merged manifest). Identical inputs are free — no re-rendering. The manifest dotfile tracks which files llmenv owns; re-renders clean up stale files without touching anything it didn't create. Foreign state (a plugin's runtime files, Claude's session cache) survives config edits.

### Precedence

Scopes stack. More specific wins:

```
project > user > host > network
```

A project marker overrides your user-level config overrides your host-level config. You can put your personal defaults at the user level and let each repo tighten or expand from there.

### No magic, no daemon

llmenv is a CLI with a shell hook. It doesn't run a background service or intercept your shell. The hook is one line in your shell rc: `eval "$(llmenv hook zsh)"`. All state lives in files at paths you can inspect.
