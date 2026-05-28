# llmenv Documentation

Start with the [project README](../README.md) for the elevator pitch and quick
start. These pages go deeper.

## Guides

- [Getting Started](getting-started.md) — install, shell hook (zsh + bash), first
  run, verifying with `doctor`, common first errors.
- [Concepts](concepts.md) — the `scopes → tags → bundles → materialize → adapter`
  pipeline, precedence, and project markers.
- [Troubleshooting](troubleshooting.md) — common failure modes and the
  diagnostic commands that surface them.

## Reference

- [Configuration](configuration.md) — every config block and field, plus
  `.llmenv.yaml` markers.
- [Commands](commands.md) — per-command reference.
- [Plugins](plugins.md) — marketplaces, plugin collections, `plugin-sync`.
- [MCP & Memory](mcp.md) — MCP servers, the ICM memory backend, tag-scoped memory
  and the env var contract.
- [Engines](engines.md) — the engine-neutral capability model and per-engine
  escape hatches.

## Maintainers

- [Maintainers index](maintainers.md) — release process and Homebrew tap setup.
- [Engine capabilities design](design/engine-capabilities.md) — the design doc
  behind the capability model.
