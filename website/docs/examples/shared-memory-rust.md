# Shared Memory Across Rust Projects

Configure a memory backend that activates for every project tagged `rust`, giving
all your Rust repos a shared long-term memory context.

## Config

```yaml
# ~/.config/llmenv/config.yaml

memory:
  backend: networked
  when: [rust]
  host: memory-server
  port: 7700

host:
  memory-server: 192.168.1.50
```

```yaml
# Any Rust project: /path/to/any-rust-repo/.llmenv.yaml

tags: [rust]
```

## How it works

- When `rust` is in the active tag set (contributed by any project marker with `tags: [rust]`), the memory backend fires.
- The adapter emits an MCP server entry pointing at `memory-server:7700`.
- All Claude Code sessions in any repo tagged `rust` share the same memory context.

## Isolated memory per project

If you want project-private memory instead:

```yaml
# ~/.config/llmenv/config.yaml

memory:
  backend: local       # per-project, no sharing
```

Or use the `state:` block to give each project its own state directory:

```yaml
state:
  tools:
    - env: CONTEXT_MODE_DATA_DIR
      subdir: context-mode
```

llmenv emits `CONTEXT_MODE_DATA_DIR=<state>/<subdir>` where `<state>` is a stable
directory that survives version and config changes.

## Verify

```bash
cd /path/to/any-rust-repo
llmenv doctor
# should show: active tags: [rust], memory: networked → memory-server:7700
```
