# llmenv

[![CI](https://github.com/phaedrus1992/llmenv/actions/workflows/ci.yml/badge.svg)](https://github.com/phaedrus1992/llmenv/actions/workflows/ci.yml)
[![coverage](https://github.com/phaedrus1992/llmenv/actions/workflows/coverage.yml/badge.svg)](https://github.com/phaedrus1992/llmenv/actions/workflows/coverage.yml)
[![docs](https://img.shields.io/badge/docs-phaedrus1992.github.io%2Fllmenv-blue)](https://phaedrus1992.github.io/llmenv/)

A universal, scope-aware environment for AI coding agents.

**llmenv** is `direnv` for Claude Code and other AI tools. As you move between
networks, hosts, users, and projects, it detects the current context, selects
the matching configuration, materializes it into an agent-native config
directory, and points the agent at it — all from a shell hook that runs on every
prompt.

## Why

A single global agent config can't express "use the office MCP server only at
work", "load these plugins only in this repo", or "share memory across every
project tagged `rust`". llmenv lets you declare configuration once, attach it to
**scopes** via **tags**, and have the right slice activate automatically.

## Install

```bash
cargo install llmenv
```

Or build from source:

```bash
git clone https://github.com/phaedrus1992/llmenv.git
cd llmenv
cargo build --release
./target/release/llmenv --help
```

## Quick start

```bash
llmenv init                       # write a template ~/.config/llmenv/config.yaml
eval "$(llmenv hook zsh)"         # add this line to ~/.zshrc (or: llmenv hook bash)
llmenv doctor                     # validate config + adapter wiring
```

Edit `~/.config/llmenv/config.yaml` to add scopes and bundles, then drop a
`.llmenv.yaml` marker into any project directory to give that project its own
tags. The shell hook re-evaluates on every prompt, so the active environment
follows you between networks and repos.

See [Getting Started](https://phaedrus1992.github.io/llmenv/docs/getting-started) for the full first-run walkthrough.

## Concepts

llmenv resolves your environment through a fixed pipeline:

```
scopes → tags → bundles → materialize → adapter emit
```

**Scopes** describe where you are (`network`, `host`, `user`, `project`); each
contributes **tags** to the active set; **bundles**, MCP servers, plugins, and
the memory backend fire on matching tags; llmenv **materializes** the result into
a content-hashed config directory; and an **adapter** renders it into an agent's
native shape and exports the env vars that point the agent at it.

When scopes of different kinds set conflicting scalar values, precedence runs
**network → host → user → project** (project wins). See
[Concepts](https://phaedrus1992.github.io/llmenv/docs/concepts) for the full pipeline, precedence rules, and
the marker-based project scope.

## Example

`~/.config/llmenv/config.yaml`:

```yaml
cache:
  cache_dir: "~/.cache/llmenv"

scope:
  network:
    - id: office
      match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }
      tags: [office]
  host:
    - id: workstation
      match: { hostname: "work-mbp" }
      tags: [workstation]
  user:
    - id: me
      match: { user: "alice" }
      tags: [me]

bundle:
  - name: base
    tags: [me]
    vars:
      EDITOR: "code"
  - name: office-config
    tags: [office]
    vars:
      OFFICE_PROXY: "proxy.internal"
```

A project marker, `~/code/myapp/.llmenv.yaml`:

```yaml
id: myapp
name: MyApp
description: "Customer-facing API"
tags: [myapp, rust]
enable_bundles: [base]      # force-enable a bundle regardless of tags
```

On the office network, the `office` and `me` tags are active; inside
`~/code/myapp`, `myapp` and `rust` join them. The matching bundles, MCP servers,
and plugins activate automatically.

## Commands

| Command | Purpose |
|---------|---------|
| `llmenv init [PATH] [--repo URL]` | Write a template config (optionally clone from a repo) |
| `llmenv export [--scope ID] [--tag TAG]` | Print `export` lines for the current scope (used by the hook) |
| `llmenv hook <zsh\|bash>` | Print shell integration code |
| `llmenv status` | Show the active scopes, tags, and config status |
| `llmenv context` | Show the resolved environment and active scopes in detail |
| `llmenv scope-ls` | List configured scopes, marking active/orphaned |
| `llmenv tag-ls` | List tags, marking active/orphaned |
| `llmenv bundle-ls` | List bundles, marking active |
| `llmenv mcp-ls` | List selected MCP servers with resolved role and transport |
| `llmenv marketplace-ls` | List plugin marketplaces, marking referenced ones |
| `llmenv plugin-ls` | List plugins, marking those selected by the active scope |
| `llmenv plugin-sync` | Clone/fast-forward plugin marketplaces into the cache |
| `llmenv sync` | `git add`/`commit`/`push` the config repo to GitHub |
| `llmenv check-stale` | Warn if the running agent's config has drifted (SessionStart hook) |
| `llmenv prune [--all] [--older-than DUR] [--dry-run]` | Clean stale cache folders |
| `llmenv doctor [--gc]` | Validate wiring; optionally garbage-collect the cache |

Every command accepts `--color <auto\|always\|never>`. Run `llmenv <command> --help`
for full flag details. Per-command reference: [Commands](https://phaedrus1992.github.io/llmenv/docs/commands).

## Introspection environment variables

`llmenv export` emits these so the agent (and your shell) can see the resolved
context:

| Variable | Format | Meaning |
|----------|--------|---------|
| `LLMENV_ACTIVE_SCOPES` | `kind:id,kind:id,…` | Every matched scope, prefixed by kind |
| `LLMENV_ACTIVE_TAGS` | `tag,tag,…` (sorted) | The active tag set |
| `LLMENV_ACTIVE_BUNDLES` | `name,name,…` | Bundles that fired, in declaration order |
| `LLMENV_ACTIVE_PROJECT` | scope id | Deepest matched project (omitted if none) |
| `LLMENV_PROJECT_ROOT` | absolute path | Directory of the deepest `.llmenv.yaml` (omitted if none) |
| `LLMENV_ICM_CONTEXT` | text chunk | Active tags/bundles encoded for tag-scoped memory retrieval |

See [MCP & Memory](https://phaedrus1992.github.io/llmenv/docs/mcp) for the `LLMENV_ICM_CONTEXT` contract.

## Supported agents

llmenv emits agent-native config through pluggable adapters. The current adapter
surface targets **Claude Code** (`CLAUDE.md`, `settings.json`, `.claude.json`
`mcpServers`, hooks, permissions, plugins). The capability model is engine-neutral, with a
per-engine `native` escape hatch for keys that have no portable equivalent — see
[Engines](https://phaedrus1992.github.io/llmenv/docs/engines).

## Documentation

- [Getting Started](https://phaedrus1992.github.io/llmenv/docs/getting-started) — install, shell hook, first run, first errors
- [Concepts](https://phaedrus1992.github.io/llmenv/docs/concepts) — the scope → tag → bundle → materialize → adapter pipeline
- [Configuration Reference](https://phaedrus1992.github.io/llmenv/docs/configuration) — every config block and field
- [Commands](https://phaedrus1992.github.io/llmenv/docs/commands) — per-command reference
- [Plugins](https://phaedrus1992.github.io/llmenv/docs/plugins) — marketplaces, plugin collections, `plugin-sync`
- [MCP & Memory](https://phaedrus1992.github.io/llmenv/docs/mcp) — MCP servers, the ICM memory backend, env var contract
- [Engines](https://phaedrus1992.github.io/llmenv/docs/engines) — engine capability model and per-engine escape hatches
- [Troubleshooting](https://phaedrus1992.github.io/llmenv/docs/troubleshooting) — common failures and the commands that surface them
- [Maintainers](https://phaedrus1992.github.io/llmenv/docs/maintainers) — release and tap-setup index

## Development

Install the git hooks so the same checks CI enforces run locally:

```bash
prek install --hook-type pre-commit --hook-type pre-push
```

- `cargo fmt --check` runs on every commit (fast).
- `cargo clippy -D warnings`, `cargo test`, and `cargo deny check` run on push (slower).

Install `prek` from <https://github.com/j178/prek> if you don't have it.

## Releases

llmenv follows [Semantic Versioning](https://semver.org/) and a
[Keep a Changelog](https://keepachangelog.com/) changelog. A version exists only
once it is git-tagged — see [CHANGELOG.md](CHANGELOG.md) and the
[releases page](https://github.com/phaedrus1992/llmenv/releases) for what has
shipped.

## License

llmenv is dual-licensed under either [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option (`MIT OR Apache-2.0`).

Binary distributions statically link third-party crates whose licenses require
attribution; their notices are reproduced in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md) (a generated artifact). See
[Licensing & attribution](https://phaedrus1992.github.io/llmenv/docs/licensing) for the attribution and compatibility
policy and how to regenerate the notices.
