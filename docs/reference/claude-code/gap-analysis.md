# Consolidated gap analysis: llmenv vs Claude Code config surfaces

Captured 2026-05-27 from <https://code.claude.com/docs/en/> against the current
`ClaudeCodeAdapter` (`src/adapter/claude_code.rs`) and config schema
(`src/config/schema.rs`).

This is the synthesis page. Per-surface detail lives in the sibling docs.

## Coverage matrix

| Surface | CC file(s) | llmenv status | Severity |
| --- | --- | --- | --- |
| Instructions | `CLAUDE.md` | ✅ Full | — |
| Rules | `rules/*.md` | ✅ Full (native convention) | — |
| MCP servers | `mcp.json` / `.mcp.json` | ✅ Full (minor: no `type` on remote, no auth headers) | Low |
| Memory backend | (via MCP) | ✅ Full (ICM desugars to MCP) | — |
| **settings.json** | `settings.json` | ❌ **Stub, wrong shape** | **Critical** |
| **Hooks wiring** | `settings.json.hooks` | ❌ Files copied, never registered | **High** |
| **Permissions** | `settings.json.permissions` | ❌ Empty array (wrong shape), unmodeled | **High** |
| Skills | `skills/<n>/SKILL.md` | ~ Validated, not generated | Medium |
| Subagents | `agents/*.md` | ❌ Unmodeled | Medium |
| Commands | `commands/*.md` | ❌ Unmodeled (byte-copy only) | Low/Medium |
| env (settings) | `settings.json.env` | ❌ Unmodeled | Medium |
| Status line | `settings.json.statusLine` | ❌ Unmodeled | Low |
| Output styles | `output-styles/*.md` + setting | ❌ Unmodeled | Low |
| Model defaults | `settings.json.model` etc. | ❌ Unmodeled | Low/Medium |
| Worktree includes | `.worktreeinclude` | ❌ Unmodeled | Low |
| Plugins/marketplaces | settings keys | ❌ Unmodeled (maybe out of scope) | Low |
| Sandbox | `settings.json.sandbox` | ❌ Unmodeled | Low |

## The critical defect

`generate_settings_json` (`src/adapter/claude_code.rs:183`) emits:

```json
{ "hooks": [], "permissions": [], "mcp": [] }
```

Every value is structurally wrong against the real schema:

- `hooks` must be an **object** keyed by event: `{ "PreToolUse": [ {matcher, hooks:[...]} ] }`.
- `permissions` must be an **object**: `{ allow:[], ask:[], deny:[], defaultMode, … }`.
- `mcp` **is not a settings key** at all — remove it.

So even the three keys it does emit don't match Claude Code's format.

> **Status note (verified 2026-05-27):** Issue #34 ("Full hook/permission
> merging") is marked **CLOSED/COMPLETED** (2026-05-26), but the fix was never
> landed — `generate_settings_json` (`claude_code.rs:183`) still emits the
> wrong-shaped stub, and `schema.rs` still has no Claude Code settings
> vocabulary. **#34 needs to be reopened** (or a new issue filed); the work it
> describes remains entirely undone.

## Why the copied hooks are inert

`materialize` copies `hooks/*.json` and substitutes `{{ICM_MCP}}`
(`claude_code.rs:54-66`), so bundles ship hook scripts. But a hook only fires if a
`settings.json` `hooks` entry references it at an event/matcher. Since
`generate_settings_json` writes an empty (and mis-shaped) `hooks`, **the copied
hook files do nothing.** Closing #34 is what makes the existing hook-copy
machinery actually functional.

## Schema-level gap

`src/config/schema.rs` models llmenv's *own* concerns (cache/sync `Settings`,
scopes, bundles, MCP, memory, hosts). It has **no vocabulary** for Claude Code's
settings: no way to express `model`, `env`, `permissions.*`, `outputStyle`,
`statusLine`, hook registrations, agents, etc. Any work here starts with a schema
decision:

1. **Bundle-contributed settings** (recommended to evaluate first): each bundle
   carries optional `settings`/`permissions`/`hooks`/`env` fragments,
   tag-selected and merged. Claude Code's **array-merge + scalar-override**
   semantics map naturally onto llmenv's existing bundle-merge model — arrays
   (permission rules, env entries) concatenate; scalars (`model`, `defaultMode`)
   need a single owning scope or explicit precedence.
2. **Dedicated top-level blocks** parallel to `bundle:`/`mcp:` (e.g. `hooks:`,
   `permissions:`), tag-selected the same way.

Either way, reuse the tag-intersection selection that bundles/MCP/memory already
share — don't invent a new selection model.

## Suggested sequencing

1. **Fix the stub shape** + drop the bogus `mcp` key (prevents writing invalid
   `settings.json`). Even before full merging, emit a correct empty object:
   `{ "permissions": {}, "hooks": {} }` or omit empty keys.
2. **Permissions generator** — smallest well-defined surface; array-merge fits
   bundles; immediate value (declarative `.env` denylist, per-language `Bash(...)`
   allows).
3. **Hooks generator** (#34) — wire copied `hooks/*.json` into the `hooks` object;
   decide `command` vs `mcp_tool` for the ICM integration (the latter could retire
   `{{ICM_MCP}}`).
4. **env + model** — small scalar/map additions to the settings generator;
   high leverage for per-scope routing.
5. **Subagents** — new copy+validate path mirroring skills; largest capability
   add.
6. **Skills generation, commands, statusLine, output-styles** — once the settings
   generator and copy/validate patterns exist, these are incremental.

## Open questions for design docs

- Does ICM/memory **replace** Claude Code's native auto memory? If so, generate
  `autoMemoryEnabled: false` to avoid two systems. (See [memory.md](./memory.md).)
- Remote MCP **auth headers** — needed for any authenticated http/sse server; not
  in the schema today. (See [mcp.md](./mcp.md).)
- Should llmenv manage **`enabledPlugins`/marketplaces**, or stay focused on
  directly materializing components? (See [plugins.md](./plugins.md).)
- Precedence interaction: llmenv writes a private `CLAUDE_CONFIG_DIR`; confirm
  **managed settings** on a host can't silently override generated keys.
