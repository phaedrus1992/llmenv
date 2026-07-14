# Precedence Walkthrough

This example shows how a project scope overrides a host scope, step by step.

## Setup

```yaml
# ~/.config/llmenv/config.yaml

scope:
  host:
    - name: my-laptop
      hostname: ranger-mbp
      tags: [me]

capabilities:
  permissions:
    - allow: "Bash(*)"

bundle:
  - name: default-mcp
    when: [me]
    mcp:
      - name: filesystem
        transport: stdio
        command: uvx
        args: ["mcp-filesystem", "--root", "~"]
```

```yaml
# /path/to/restricted-project/.llmenv.yaml

tags: [restricted]

# Narrow permissions for this project
capabilities:
  permissions:
    - allow: "Bash(git *)"
    - allow: "Read(*)"
    - deny: "Bash(*)"
```

## Trace through the pipeline

**Step 1 — Scopes resolve:**

You're on `ranger-mbp` inside `restricted-project/`. Two scopes are active:

| Scope | Source | Tags added |
| --- | --- | --- |
| `host:my-laptop` | config.yaml | `me` |
| `project` | `.llmenv.yaml` | `restricted` |

Active tag set: `{me, restricted}`

**Step 2 — Contributors fire:**

- `bundle:default-mcp` → tags `[me]` ∩ `{me, restricted}` = `{me}` → **fires**
- No contributor with tag `restricted` exists → project doesn't add any contributor

**Step 3 — Capabilities merge (project > host):**

The project marker declares its own `capabilities.permissions`. Project scope wins:

```text
Final permissions:
  allow: "Bash(git *)"
  allow: "Read(*)"
  deny: "Bash(*)"
```

The host-level `allow: "Bash(*)"` is **not present** — it was overridden.

**Step 4 — Materialize:**

The merged manifest is written to the cache directory. The adapter emits
`settings.json` with the final (narrowed) permissions and the `filesystem` MCP
(from the bundle that fired).

## Key takeaway

Scopes don't cancel each other — all active scopes contribute tags and their
contributors fire additionally. But when the same **capability field** is declared
at multiple scope levels, **the most specific scope wins** (project > user > host > network).

Use this to set permissive defaults at the host level and tighten them per project.
