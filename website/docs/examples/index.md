---
sidebar_label: Overview
---

# Examples

Complete, copy-pasteable worked examples for common llmenv scenarios.

Each example assumes you've already run `llmenv init` and have a working
`~/.config/llmenv/config.yaml`. See [Getting Started](../getting-started.md) if
you haven't.

| Example | What it shows |
| --- | --- |
| [Office vs. home network](./office-home-network.md) | Different MCP servers per network, automatic switching |
| [Per-repo plugin sets](./per-repo-plugins.md) | Project markers activating repo-specific tooling |
| [Shared memory across Rust projects](./shared-memory-rust.md) | Tag-based memory backend shared by a language tag |
| [Multi-host memory topology](./multi-host-memory.md) | Same memory feature, different network exposure per host |
| [Precedence walkthrough](./precedence-walkthrough.md) | Project scope overriding host scope, step by step |

For a complete, working config tree instead of an isolated snippet, see
[`examples/config-llmenv-dir/`](https://github.com/phaedrus1992/llmenv/tree/main/examples/config-llmenv-dir)
in the repo — real scopes, bundles, `config.yaml`, and `AGENTS.md`, all wired together.
