# Sandbox and approvals

Codex couples two orthogonal controls: **`sandbox_mode`** (what the agent can
touch — filesystem/network) and **`approval_policy`** (when Codex pauses to ask).
Claude Code expresses access control through its permissions system; Codex makes
sandbox a first-class top-level key. This is net-new vocabulary for llmenv.

## `sandbox_mode`

| Value | Behavior |
| --- | --- |
| `read-only` | no writes; safest |
| `workspace-write` | writes within workspace roots; network off by default |
| `danger-full-access` | unrestricted (may be blocked by managed `requirements.toml`) |

### `[sandbox_workspace_write]` (applies when `workspace-write`)

```toml
[sandbox_workspace_write]
writable_roots = ["/Users/YOU/.pyenv/shims"]  # extra writable dirs
network_access = false                          # opt in to outbound network
exclude_tmpdir_env_var = false                  # false = $TMPDIR writable
exclude_slash_tmp = false                       # false = /tmp writable
```

Protected paths (`.git`, `.codex`) stay read-only even inside writable roots.

## `approval_policy`

| Value | Behavior |
| --- | --- |
| `untrusted` | pause for most commands |
| `on-request` | model asks when it judges necessary |
| `on-failure` | run, ask only if a sandboxed command fails |
| `never` | never pause (may be blocked by managed requirements) |
| `{ granular = { ... } }` | per-category allow/auto-reject |

Granular example:

```toml
approval_policy = { granular = {
  sandbox_approval = true,
  rules = true,
  mcp_elicitations = true,
  request_permissions = false,
  skill_approval = false,
} }
```

Related keys: `approvals_reviewer` (`user` | `auto_review`),
`allow_login_shell` (hardening: disallow login shells for the shell tool),
`[auto_review].policy` (local reviewer instructions; managed
`guardian_policy_config` takes precedence).

## Beta: permission profiles

`[permissions.<name>]` (beta) bundle filesystem + network access together; see
Codex's Permissions doc. `permissions.<name>.network.*` configures sandboxed
network allow rules.

## Gaps vs llmenv

llmenv has **no sandbox or approval vocabulary at all**. A `CodexAdapter` would
need:

- New schema fields (or a `codex:` block) for `sandbox_mode`, the
  `workspace-write` sub-keys, and `approval_policy`. These are
  **security-sensitive** and Codex forbids setting them in project-local config —
  so the adapter must write them to the **user-level** `config.toml`, never a
  project layer.
- A decision on defaults. Codex's own default is `workspace-write` +
  `on-request`; llmenv could mirror that or expose it as policy.
- Awareness of managed `requirements.toml`: an org may forbid `never` /
  `danger-full-access`, so generated values can be rejected at runtime. The
  adapter shouldn't assume its output is always honored.

This is the largest *conceptual* gap — it's not a serialization difference, it's
a domain llmenv's config model doesn't currently describe.
