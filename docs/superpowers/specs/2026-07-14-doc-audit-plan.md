# Doc Audit Implementation Plan — 2026-07-14

## Scope

Five files, three tiers from the audit:

1. **Fill gaps** — missing config fields and CLI commands
2. **Fix README** — deprecated commands, missing commands, table alignment
3. **De-duplicate and tighten** — remove duplicated content, trim prose

## Changes

### README.md

- Replace deprecated `*-ls` commands with `status <subcommand>` forms
- Add missing commands: `setup`, `regenerate`, `upgrade`, `validate`, `edit`, `completions`, `login`, `check-stale`, `memory`
- Fix introspection env vars table alignment
- Remove `sync` (deprecated git sync cmd) and add `plugin-sync`

### configuration.md

- Add `context_mode` under `features:`
- Add `disabled_engines` to top-level table
- Add `init:` section (seeded settings)
- Add `skills:` top-level section
- Add `--compress` export flag mention
- Tighten `session_log:` prose

### commands.md

- Add `setup`, `upgrade`, `memory`, `login`, `read-once`, `throttle` commands
- Add `--compress` to export flags
- Mark deprecated section accurately

### getting-started.md

- Replace full commands table with short reference + link to commands.md
- Add `--compress` to export flags

### mcp.md

- Add cross-reference from lifecycle hooks section to commands.md
