# Shared Memory Across Rust Projects

Configure llmenv's memory backend (ICM) so every project tagged `rust` shares
the same long-term memory context, instead of each repo getting its own.

## Config

```yaml
# ~/.config/llmenv/config.yaml

host:
  memory-server:
    addr: "192.168.1.50"

features:
  memory:
    - server_host: memory-server
      port: 9092
      when: [rust]
```

```yaml
# Any Rust project: /path/to/any-rust-repo/.llmenv.yaml

tags: [rust]
```

## How it works

- `rust` is a plain activation tag, contributed by any project marker with
  `tags: [rust]` — same selection model as bundles and MCP servers.
- When `rust` is in the active tag set, this `features.memory` entry fires and
  llmenv points the adapter at `memory-server:9092`.
- Every Claude Code session in every repo tagged `rust` resolves to the same
  memory backend, so decisions/patterns recorded in one Rust project are
  recallable from any other.

## Scoping instead of sharing

At most one `features.memory` entry can be active per scope — the resolver
errors if two entries' tags match simultaneously. So isolation is a tagging
decision, not a backend flag: give a project a tag that doesn't intersect the
shared entry's `when`, or add a second entry with its own host/tag pair for a
project (or group of projects) that should have private memory instead:

```yaml
features:
  memory:
    - server_host: memory-server
      port: 9092
      when: [rust]              # shared pool
    - server_host: memory-server
      port: 9093
      when: [proprietary-repo]  # private — different port, different tag
```

## Verify

```bash
cd /path/to/any-rust-repo
llmenv doctor
llmenv context   # shows the resolved memory host/port for the active tags
```
