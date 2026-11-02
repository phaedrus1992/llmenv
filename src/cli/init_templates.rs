//! Starter text for the files that `llmenv init` writes into a new config folder.

pub(super) const AGENTS_TEMPLATE: &str = r#"# Agent Orientation

This directory contains llmenv configuration. Agents (Claude Code, Copilot, Gemini CLI)
operating here will have access to the merged config and bundles.

## Key Files & Directories

- **config.yaml** — Main configuration. Declares scopes, bundles, MCP servers, state locations.
- **bundles/** — Bundle directories. Each bundle contains files merged into agent config:
  - `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` — Agent instructions (loaded automatically)
  - `skills/` — Custom skills directory
  - `hooks/` — Hook definitions
  - `mcp.json` — MCP server configurations (optional)
- **state/** — Durable per-tool state (managed by llmenv; don't write here directly)
- **.llmenv.yaml** — Project scope marker (place in project roots, not here)

## Where to Add Things

### New Agent Instructions
Create or edit a bundle's `CLAUDE.md` (Claude Code), `AGENTS.md` (all agents), or `GEMINI.md` (Gemini CLI):
```
bundles/myname/CLAUDE.md
```

### New Skills
Add to a bundle's `skills/` directory:
```
bundles/myname/skills/my-skill.json
```

### New Hooks
Add to a bundle's `hooks/` directory or declare in `config.yaml`:
```
bundles/myname/hooks/some-hook.sh
```

### MCP Servers
Either add to a bundle's `mcp.json` or declare in `config.yaml` under `mcp:`.

### Per-Tool Durable State
Declare in `config.yaml` under `state: tools:`:
```yaml
state:
  tools:
    - env: MY_TOOL_STATE_DIR
      subdir: my-tool
```

## Scopes & Tags

Scopes (network, host, user, content, project) emit **tags** when they match. Bundles and other
resources fire when one of their tags is in the active tag set. See `config.yaml` comments
for examples.

---

For more, see the llmenv documentation and the `config.yaml` template comments.
"#;

pub(super) const README_TEMPLATE: &str = r#"# llmenv Configuration

This directory contains your llmenv configuration.

## Layout

- **config.yaml** — Main configuration file. Declares scopes, bundles, MCP servers,
  plugins, and the memory backend. Edit this to define which environments activate
  in which contexts (networks, hosts, users, project directory contents, projects).

- **.llmenv.yaml** — Project markers (one per project). Drop a marker file at the
  root of any project directory to give that project its own scope, tags, and
  enabled bundles. llmenv discovers these by walking upward from the current
  directory.

- **bundles/** — Bundle content directories. Each directory here matches a
  `bundle:` entry in `config.yaml`. Bundles can contain:
  - YAML files (`bundle.yaml`) with MCP servers, hooks, and other capabilities
  - Environment variables
  - Plugin declarations

## Getting Started

1. **Edit config.yaml** to add your first scopes (network, host, user) and a bundle.
   See the comments in the file for examples and reference the [Configuration docs](https://phaedrus1992.github.io/llmenv/docs/configuration).

2. **Install the shell hook** to activate llmenv on every prompt:

   ```bash
   eval "$(llmenv hook zsh)"      # Add to ~/.zshrc
   # or
   eval "$(llmenv hook bash)"     # Add to ~/.bashrc
   ```

3. **Verify setup**:

   ```bash
   llmenv doctor        # Check for configuration errors
   llmenv status        # Show active scopes and tags
   llmenv export        # Preview the exported environment
   ```

4. **Add projects** by creating `.llmenv.yaml` marker files at project roots.

## Concepts

- **Scopes** — Describe where you are (network/host/user/content/project). Each emits **tags**.
- **Tags** — Labels that trigger bundles, MCP servers, plugins, and memory.
- **Bundles** — Fire when their tags match the active set. Contribute config, env vars, and capabilities.
- **Materialize** — llmenv combines active scopes/bundles into a content-addressed config directory.
- **Adapter** — Renders the materialized config into the agent's native format (e.g. Claude Code).

## Documentation

- [Getting Started](https://phaedrus1992.github.io/llmenv/docs/getting-started) — First-run walkthrough
- [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration) — Complete schema reference
- [Concepts](https://phaedrus1992.github.io/llmenv/docs/concepts) — Scopes, tags, bundles, precedence
- [Main Site](https://phaedrus1992.github.io/llmenv/) — All documentation
"#;
