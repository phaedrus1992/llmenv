# Updating llmenv Configuration

## Source vs. Materialized Config

llmenv maintains two sets of configuration files:

- **Source config**: The canonical configuration files in your llmenv project directory
  - `config.yaml` — main config file
  - `bundles/<name>/bundle.yaml` — per-bundle fragments
  - `.llmenv.yaml` — optional project marker
  - Located in: `$LLMENV_PROJECT` (typically `~/.config/llmenv` or equivalent)

- **Materialized config**: The rendered output cached by Claude Code for active use
  - Located in: `~/.cache/llmenv/claude-code/<version>/<hash>/`
  - Includes: `settings.json`, plugins, hooks, etc.
  - **Ephemeral**: Regenerated on each `llmenv export` or `llmenv materialize`

**Critical:** Always edit the source config, never the materialized output. Changes to materialized files are lost on the next materialize run.

## Updating Config: Step-by-Step

### 1. Identify the Right Bundle

Bundles organize config by scope and audience:

- **User bundle** (`~/.config/llmenv/bundles/user/`): Personal preferences, keys, credentials
- **Project bundle** (`~/.config/llmenv/bundles/project/`): Project-specific tools, permissions, plugins
- **Network/shared bundles**: Team or organization-wide settings

If none fit, create a new bundle with a descriptive name.

### 2. Edit the Source Config

Edit the appropriate `bundle.yaml` (or top-level `config.yaml` for global settings):

```yaml
# bundles/<name>/bundle.yaml
capabilities:
  permissions:
    allow:
      - Tool(name:...)
  hooks:
    - event: TurnStart
      handler: ./hooks/my-hook.sh
  plugins:
    - marketplace:plugin-id
  auto_memory_enabled: true

native:
  claude_code:
    alwaysThinkingEnabled: true

# ... other config ...
```

Refer to `docs/design/engine-capabilities.md` for the full schema.

### 3. Materialize to Apply Changes

After editing, regenerate the materialized config:

```bash
llmenv materialize
```

This:
- Evaluates all scopes and active bundles
- Merges config by value shape (lists concatenate, scalars use precedence)
- Writes the final output to `~/.cache/llmenv/claude-code/<version>/<hash>/settings.json`

If Claude Code is running, the new config is NOT picked up until you restart the agent.

## Common Tasks

### Add a Permission

1. Identify the bundle (or create one)
2. Edit `bundle.yaml`:
   ```yaml
   capabilities:
     permissions:
       allow:
         - WebFetch(domain:example.com)
       ask:
         - Bash(substring:rm)
   ```
3. Run `llmenv materialize`

### Add a Hook

1. Create the hook script in `bundles/<name>/hooks/`:
   ```bash
   #!/bin/bash
   # my-hook.sh
   echo "Hook running!"
   ```
2. Register in `bundle.yaml`:
   ```yaml
   capabilities:
     hooks:
       - event: SessionStart
         handler: ./hooks/my-hook.sh
   ```
3. Run `llmenv materialize`

### Add an MCP Server

1. Edit `config.yaml` or `bundle.yaml`:
   ```yaml
   mcp:
     - name: my-server
       type: stdio
       command: python
       args: ["-m", "my_package.server"]
   ```
2. Run `llmenv materialize`

### Enable ICM Memory Backend

1. Edit top-level `config.yaml`:
   ```yaml
   features:
     memory:
       server:
         host: localhost
         port: 9999
   ```
2. Auto-disables Claude's auto-memory (set `capabilities.auto_memory_enabled: true` to override)
3. Run `llmenv materialize`

## Troubleshooting

- **Changes not taking effect?** Verify you edited the source config (not materialized output) and ran `llmenv materialize`
- **Config disappeared?** Check `~/.cache/llmenv/` — if you edited the cached config directly, it will be overwritten on the next materialize
- **Merge conflict?** Review active scopes (`llmenv doctor`) and precedence rules in `docs/design/engine-capabilities.md`
