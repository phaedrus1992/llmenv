# Crush engine support (multi-engine foundation)

## Goals

- Add a `CrushAdapter` implementing the existing `AgentAdapter` trait, reaching
  feature parity with `ClaudeCodeAdapter` wherever Crush has an equivalent
  capability.
- Refactor the call sites that hardcode `ClaudeCodeAdapter` into an adapter
  registry, so `export`/`hook`/`regenerate` materialize and export env vars
  for every installed, supported engine in one pass — no engine-selection
  flag, no "which engine is active" state to track.
- Extend the engine-agnostic schema to cover domains Crush has and Claude
  doesn't (LSP), and promote a domain that exists ad hoc today to first-class
  (skills, decoupled from plugins).
- Close MCP server field parity gaps (`headers`, `disabled`, `disabled_tools`,
  `timeout`).
- Preserve the existing `native_*`/`native` per-engine escape hatch pattern.
  It is already keyed by engine name (`"claude_code"` is a literal string
  today — see `src/merge/mod.rs`, `src/merge/capabilities.rs`), so adding
  `"crush"` as a second key is purely additive. Zero migration cost for
  existing native overrides.
- Minimize migration pain for llmenv 3.0: only break what must break.

## Non-goals

- Building a plugin/marketplace equivalent for Crush. It has no such concept
  (no `plugin`/`marketplace` package in its codebase) and inventing one is
  out of scope.
- Provider/model selection as a first-class, engine-agnostic concept. For
  this round, defining providers/models under the existing `native.crush`
  escape hatch is sufficient. A **separate brainstorming session and issue**
  should cover providers/models as first-class config, scoped for other
  future engines too (opencode, pi, etc.) — not just Crush.
- Running multiple engines simultaneously in one session — a Crush/Claude
  product question, not llmenv's.

## Background: what's already in place

llmenv was already built with multi-engine in mind, even though only one
adapter exists today:

- `AgentAdapter` trait (`src/adapter/mod.rs`): `name()`, `env_vars()`,
  `materialize()`, `emit_hook_context()`. One implementation,
  `ClaudeCodeAdapter` (`src/adapter/claude_code.rs`).
- `crates/llmenv-config/src/schema.rs`'s `Capabilities` struct is explicitly
  documented as "Engine-agnostic capability vocabulary," with per-engine
  escape hatches already wired in: `native`, `native_permissions`,
  `native_hooks`, `native_plugins`, `native_mcp` — all
  `BTreeMap<engine_name, Value>`.
- `StateConfig`/`StateTool` (`#175`) relocates per-tool state out of the
  content-hashed cache dir into a stable sibling directory, addressing
  `CLAUDE_CONFIG_DIR` churn on every content-hash change.

What's missing: the call sites that consume `AgentAdapter` hardcode
`ClaudeCodeAdapter` by name. No registry, no dispatch.

## Feature comparison: llmenv/Claude Code vs. Crush

| Domain | llmenv today (Claude Code) | Crush | Gap |
|---|---|---|---|
| Config format | YAML (`config.yaml`, `bundle.yaml`) → rendered to Claude's native JSON | JSON (`crush.json`), priority: `.crush.json` > `crush.json` > global | Rendering-only difference, no schema impact |
| Env vars | `CLAUDE_CONFIG_DIR` (+ `LLMENV_STATE_DIR` workaround via `StateConfig`) | `CRUSH_GLOBAL_CONFIG`, `CRUSH_GLOBAL_DATA`, `CRUSH_SKILLS_DIR` | Crush's clean config/data split means it doesn't need the `StateConfig` workaround at all |
| MCP | `McpServer { name, when, transport, command, args, env, url }` | adds `headers`, `disabled`, `disabled_tools`, `timeout` | Field parity gap, additive |
| LSP | none | first-class: `command`, `args`, `env`, `disabled`, `filetypes`, `root_markers`, `init_options`, `timeout` | Net-new domain for llmenv |
| Skills | only inside plugins (`ClaudeCodeAdapter::validate_skills`/`write_skill`) | first-class, `options.skills_paths`, default dirs incl. `.claude/skills` directly, `user-invocable`/`disable-model-invocation` frontmatter | Needs promotion to a first-class capability |
| Plugins/marketplace | `Marketplace`, `PluginCollection`, reserved-name validation (`RESERVED_OFFICIAL_MARKETPLACES`) | none | No Crush target; needs explicit handling |
| Custom subagents | plugin-bundled `agents/*.md` | `Config.Agents` is `json:"-"` (not configurable); `SetupAgents()` hardcodes exactly two roles (`coder`, `task`) | No Crush target at all — upstream limitation, not an llmenv gap |
| Hooks | open `event: String`, handler `command`\|`mcp_tool` | only `PreToolUse` implemented today; JSON stdin payload; **Claude-Code-compatible hook output format is the intended long-term shape** (per Crush maintainers — only `PreToolUse` is implemented so far, more events are on their roadmap) | `CrushAdapter` can delegate `emit_hook_context()` straight to the same Claude-format renderer; `supported_hook_events()` returns `["PreToolUse"]` for now and grows as Crush adds events, no llmenv-side redesign needed later |
| Permissions | `Permissions { default_mode, allow, ask, deny }` rule lists | `permissions.allowed_tools` allowlist only | Lossy collapse: `ask` has no Crush equivalent — fails closed (excluded from `allowed_tools`, never silently granted) |

## Design

### 1. Adapter registry + dispatch

Replace hardcoded `ClaudeCodeAdapter` usage at its current call sites
(`src/cli/mod.rs`, `src/hook_run/mod.rs`, `src/throttle/mod.rs`,
`src/cli/doctor.rs`, and 3 test files) with a registry:

```rust
fn registered_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![Box::new(ClaudeCodeAdapter), Box::new(CrushAdapter)]
}
```

`export`/`hook`/`regenerate` iterate the registry. For each adapter:

1. **PATH-gate**: skip entirely if the adapter's binary isn't found on
   `PATH`. Claude-only users see zero behavior change — no new `crush/`
   cache subtree, no new `CRUSH_*` env vars, no extra I/O. An engine only
   activates once the user actually installs the tool.
2. Materialize into a per-adapter cache subtree:
   `~/.cache/llmenv/<hash>/<adapter.name()>/` (e.g. `.../claude_code/`,
   `.../crush/`) so the two engines' trees never collide.
3. Emit that adapter's env vars.

Since `CLAUDE_CONFIG_DIR` and `CRUSH_GLOBAL_CONFIG`/`CRUSH_GLOBAL_DATA` don't
collide, both can be exported into the same shell session safely; the user
runs whichever binary they want and it picks up its own vars.

**Trait additions** for capability probing, since not every adapter supports
every domain:

```rust
trait AgentAdapter {
    // existing: name, env_vars, materialize, emit_hook_context
    fn supports_plugins(&self) -> bool;
    fn supports_lsp(&self) -> bool;
    fn supported_hook_events(&self) -> &'static [&'static str];
}
```

`materialize()` callers use these to decide policy (see "Unsupported
capability handling" below) instead of silently dropping or guessing.

**Engine identity for hook-invoked commands.** `CheckStale`, `ConfigContext`,
`ConfigGuard`, and `HookRun` are invoked *by* a running agent, not by the
user's shell, so they need to know which engine called them. Each adapter
bakes its own identity into the command line it registers in its native
hook/settings format — e.g. Claude's `settings.json` gets
`llmenv check-stale --engine claude_code`, Crush's hook config gets
`llmenv check-stale --engine crush`. No runtime env-sniffing required.

### 2. Schema changes

**MCP field parity** — extend `McpServer` with `headers:
BTreeMap<String,String>`, `disabled: bool`, `disabled_tools: Vec<String>`,
`timeout: Option<u32>`. All optional/defaulted; existing bundles parse
unchanged.

**LSP — new top-level + bundle-level capability**, mirroring `McpServer`'s
tag-scoping shape:

```rust
pub struct LspServer {
    pub name: String,
    pub when: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub disabled: bool,
    pub filetypes: Vec<String>,
    pub root_markers: Vec<String>,
    pub timeout: Option<u32>,
}
```

Added as `Config::lsp: Vec<LspServer>` and `Capabilities::lsp:
Vec<LspServer>`, selected by tag intersection like `mcp`/`bundle` already
are. `ClaudeCodeAdapter::supports_lsp()` returns `false`; `materialize()`
skips rendering LSP for Claude as a no-op (not an error — LSP declared in a
bundle shared across engines is legitimate for an engine that simply has no
concept of it, unlike plugins, which actively lose user-visible
functionality when dropped).

**Skills — promoted to first-class, decoupled from plugins.** New
`Capabilities::skills` capability. The exact field shape should reuse
`ClaudeCodeAdapter`'s existing path-traversal and frontmatter validation
(`validate_skills`, `validate_skill_frontmatter`,
`scan_skill_files_for_hardcoded_paths` in `src/adapter/claude_code.rs`)
rather than duplicating it — settle exact fields during implementation.
Existing plugin-embedded skills keep working unchanged (additive, no
back-compat break). Both adapters consume the first-class list:
`ClaudeCodeAdapter` writes into its existing skills location (Claude already
supports skills outside plugins), `CrushAdapter` writes into its skills dir
and can pass through `user-invocable`/`disable-model-invocation` frontmatter
as native data.

**Engine naming convention.** The native key for Crush is the literal string
`"crush"` — matches the binary name, consistent with `"claude_code"`'s
existing precedent. `"claude_code"` is not renamed; renaming it would be a
gratuitous breaking change against the minimize-migration goal.

### 3. Unsupported-capability handling

When an engine's adapter returns `supports_plugins() == false`:

1. **Plugin → skill projection**: scan each selected plugin's `skills/`
   subdirectory and project that content through the same skill-writing path
   as first-class `skills:` entries. This is the only automatic
   cross-capability bridge in this design.
2. **Hard error for everything else**: non-skill plugin content (custom
   agents under a plugin's `agents/` dir, commands, plugin-only hooks,
   marketplace mechanics) has no projection target. `materialize()` fails
   with a message naming the offending bundle/plugin and the specific
   unsupported content (e.g. "plugin `foo` declares custom agents, which
   Crush does not support — scope this bundle away from Crush with `when:`
   or accept the gap"). This is a deliberate fail-fast choice, not a silent
   feature loss — matches the "no silent failures" standard.
3. **Custom agents are a known Crush limitation**, not an llmenv gap: Crush's
   `Config.Agents` is excluded from JSON deserialization entirely
   (`json:"-"`) and `SetupAgents()` hardcodes exactly two roles (`coder`,
   `task`). There is currently no way to define a named custom subagent in
   Crush at all. Worth a short note in the issue so this isn't mistaken for
   an oversight on llmenv's side.

When an engine's adapter returns `supports_lsp() == false`: skip silently
(no-op, not an error — see above).

When a hook's `event` isn't in `supported_hook_events()` for the active
adapter: hard error, naming the bundle and event.

When `Permissions.ask` rules exist and the active adapter only supports an
allowlist (Crush): the rule is excluded from the allowlist (fails closed —
never silently promoted to allow).

### 4. `CrushAdapter`

- **`materialize()`** renders `crush.json`:
  - `permissions` → `permissions.allowed_tools` (lossy collapse, see above).
  - `hooks` → `hooks.PreToolUse` array only (others already hard-error via
    `supported_hook_events()`). `command`-kind handlers map directly;
    `mcp_tool`-kind has no Crush equivalent → hard error.
  - `mcp` → `mcp.<name>` entries (`type: stdio|http|sse`), including the new
    `headers`/`disabled`/`disabled_tools`/`timeout` fields.
  - `lsp` → `lsp.<name>` entries.
  - `skills` (first-class + projected plugin skills) → written into Crush's
    skills directory / referenced via `options.skills_paths`.
  - `native.crush` / `native_permissions.crush` / `native_hooks.crush` /
    `native_mcp.crush` → deep-merged verbatim into the rendered `crush.json`.
    This is also where provider/model config lives for this round, per the
    non-goals above.
- **`env_vars(cache_dir)`** returns `CRUSH_GLOBAL_CONFIG=<cache_dir>` and
  `CRUSH_GLOBAL_DATA=<state_dir>`, reusing the same stable state directory
  llmenv already maintains for `StateConfig` — Crush needs no additional
  workaround here, unlike Claude.
- **`emit_hook_context()`** delegates to the same Claude-compatible hook
  output renderer `ClaudeCodeAdapter` uses — Crush's maintainers have
  confirmed Claude-compatible hook output is the intended long-term format;
  only `PreToolUse` is implemented today. No separate Crush-specific
  rendering needed.
- **`supported_hook_events()`** returns `["PreToolUse"]` for now, with a
  comment noting this should grow as Crush implements more events — no
  llmenv-side redesign required when that happens.

## Migration & breaking changes (llmenv 3.0)

The existing engine-aware architecture keeps the breaking-change surface
small:

- **Additive, no migration needed**: new `lsp:` capability, new `skills:`
  capability, new MCP fields, `"crush"` as a new `native_*` key. All
  optional/defaulted — existing `config.yaml`/`bundle.yaml` files parse
  unchanged.
- **Behavior change, not schema change**: with PATH-gating (see above),
  Claude-only users see no behavior change. Users who *do* have Crush
  installed will see a new `crush/` cache subtree and new `CRUSH_*` env vars
  appear — worth a changelog callout, not a migration step.
- **No required breaking changes identified.** The lossy permissions
  collapse and plugin/agent hard-errors only surface when a bundle is
  actually shared across engines and hits an unsupported shape — that's new
  validation surfacing a pre-existing gap, not a break of working
  Claude-only configs.

Recommendation: don't force config-shape breaks just because 3.0 allows
them. Nothing in this design requires one.

## Issue breakdown

One epic in `phaedrus1992/llmenv`, with linked sub-issues sized for
independent PRs:

1. **Epic: Crush support (multi-engine foundation)** — links the rest,
   carries goals/non-goals/migration summary.
2. **Adapter registry + PATH-gated dispatch refactor** — replace the 7
   hardcoded `ClaudeCodeAdapter` call sites; add `supports_plugins()` /
   `supports_lsp()` / `supported_hook_events()` to the trait; PATH-detection
   gating; per-engine cache subdirectory layout. *(Foundational — blocks
   everything else.)*
3. **Schema: LSP capability** — `LspServer`, `Config::lsp`,
   `Capabilities::lsp`, tag-scoped like MCP.
4. **Schema: skills as first-class capability** — `Capabilities::skills`,
   decoupled from plugins, reusing existing Claude skill validation;
   plugin→skill projection for engines without `supports_plugins()`.
5. **Schema: MCP field parity** — `headers`/`disabled`/`disabled_tools`/
   `timeout` on `McpServer`.
6. **`CrushAdapter` implementation** — `crush.json` rendering, env vars,
   hook rendering (delegates to Claude-compatible renderer), permissions
   lossy-collapse, custom-agent hard-error path. *(Depends on 2–5.)*
7. **Docs + changelog** — migration notes (dual-engine export behavior
   change), Crush-equivalent config guidance in llmenv's own docs.

Not part of this epic — tracked as a separate future issue: providers/models
as a first-class, engine-agnostic concept, scoped for Crush *and* other
future engines (opencode, pi, etc.).

Sequencing: (2) first — unblocks parallel work. (3)/(4)/(5) can proceed in
parallel once (2) lands. (6) depends on all of 2–5. (7) last.
