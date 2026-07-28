# Precedence Walkthrough

This example shows how a project-triggered bundle's capabilities interact with
a host-triggered bundle's — and the two different rules that apply depending
on whether the field is a **scalar** or a **list**.

## Setup

```yaml
# ~/.config/llmenv/config.yaml

scope:
  host:
    - id: laptop
      match: { hostname: "ranger-mbp" }
      tags: [me]

bundle:
  - name: default-mode
    when: [me]
  - name: restricted-mode
    when: [restricted]
```

```yaml
# bundles/default-mode/bundle.yaml
capabilities:
  permissions:
    default_mode: acceptEdits
    allow:
      - { tool: Bash, pattern: "*" }
```

```yaml
# bundles/restricted-mode/bundle.yaml
capabilities:
  permissions:
    default_mode: plan
    deny:
      - { tool: Bash, pattern: "rm -rf *" }
```

```yaml
# /path/to/restricted-project/.llmenv.yaml

tags: [restricted]
```

A project marker can't declare `capabilities:` directly — it only contributes
tags (and `enable_bundles`/`disable_bundles`). To give a project its own
capabilities, tag a bundle so a project-contributed tag fires it, like
`restricted-mode` above.

## Trace through the pipeline

**Step 1 — Scopes resolve:**

You're on `ranger-mbp` inside `restricted-project/`. Two scopes are active:

| Scope | Kind | Tags added |
| --- | --- | --- |
| `host:laptop` | host | `me` |
| `restricted-project` | project | `restricted` |

Active tag set: `{me, restricted}`

**Step 2 — Contributors fire:**

- `bundle:default-mode` → `[me]` ∩ `{me, restricted}` = `{me}` → **fires**,
  selected by the **host** scope
- `bundle:restricted-mode` → `[restricted]` ∩ `{me, restricted}` = `{restricted}`
  → **fires**, selected by the **project** scope

Bundle precedence is inherited from the scope kind that selected it —
`restricted-mode` outranks `default-mode` because project outranks host
(network → host → user → project, least to most specific).

**Step 3 — Capabilities merge, scalar vs. list:**

`default_mode` is a **scalar** — the highest-precedence contributor wins outright:

```text
default_mode: plan   # restricted-mode (project) wins over default-mode (host)'s acceptEdits
```

`allow`/`deny` are **lists** — every contributor's entries concatenate, they
never override:

```text
allow:
  - { tool: Bash, pattern: "*" }          # from default-mode
deny:
  - { tool: Bash, pattern: "rm -rf *" }   # from restricted-mode
```

Both rules are present in the final manifest. llmenv doesn't resolve the
allow/deny overlap itself — the engine's own runtime precedence (deny beats
allow on Claude Code) is what actually blocks `rm -rf *` despite the broader
allow rule also matching it.

**Step 4 — Materialize:**

The merged manifest is written to the cache directory. The adapter emits
`settings.json` with `default_mode: plan`, the concatenated `allow`/`deny`
lists, and the `filesystem`-style tooling from whichever bundles fired.

## Key takeaway

Scalars (`default_mode`, a single `env` key) resolve by precedence — the most
specific scope's bundle wins outright, and two bundles at the *same*
precedence disagreeing on a scalar is a hard error (no rank to break the tie).
Lists (`allow`/`ask`/`deny`, hooks, plugins) never override each other — every
active contributor's entries concatenate and de-duplicate, regardless of
precedence.

## Verify

```bash
cd /path/to/restricted-project
llmenv doctor
llmenv export --dry-run   # preview the merged manifest before it's written
```
