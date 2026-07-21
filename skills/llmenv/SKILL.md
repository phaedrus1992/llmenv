---
name: llmenv
description: >
  How to use llmenv's built-in features (task tracker, memory, context-mode,
  codebase-memory) effectively. Load this first; it points to a reference
  file per enabled feature rather than dumping all of them into context.
---

# llmenv Built-ins

This project has one or more llmenv built-in features enabled. Load only the
reference file for what you're about to do — don't read all of them up front.

- Tracking durable, cross-session work (tasks, sessions) →
  `references/task-tracker.md`
- Recalling or storing project memory (ICM) → `references/memory.md`
- Reducing token usage for large tool outputs → `references/context-mode.md`
- Looking up code structure/architecture in an indexed repo →
  `references/codebase-memory.md`

Only the reference files for features enabled in this project's config exist
under `references/` — if one of the above isn't listed there, that feature
isn't enabled here.
