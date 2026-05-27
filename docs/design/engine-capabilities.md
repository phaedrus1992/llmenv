# Design: engine capabilities (replacing the `settings.json` stub)

Status: proposed — 2026-05-27
Supersedes the placeholder work tracked in #34.
Related: #59 (plugins/marketplaces), #85 (SessionStart hook).

## Problem

`generate_settings_json` (`src/adapter/claude_code.rs:183`) emits a fixed,
wrong-shaped stub:

```json
{ "hooks": [], "permissions": [], "mcp": [] }
```

Issue #34 ("generate settings.json from merged hook + permission bundles") was
once marked CLOSED/COMPLETED but never implemented. It is now implemented across
#90 (hooks), #91 (native passthrough), and #34 itself (neutral permission
rendering): bundles declare capabilities in `bundle.yaml` (the TOML proposal in
the original ticket predates the YAML switch and is dropped), the cross-bundle
merge lives in `crate::merge`, and `ClaudeCodeAdapter::generate_settings_json`
renders neutral rules into Claude's string grammar. Resolved design questions:
the schema is llme-neutral YAML (not native-shaped TOML); hooks dedup by the full
`Hook` value (equivalent to the `(event, matcher, command)` tuple since handler
kind/tool are determined by the command vs. tool field); native permission rules
win over conflicting neutral rules.

The naive fix — "model `settings.json`" — is wrong. `settings.json` is a
Claude-Code-specific *container*. Codex has a different file with different
keys. Modeling "settings" as a concept couples llmenv to one engine and also
collides with llmenv's existing `Config.settings` (cache/sync) field.

## Principle

**Don't model the container. Model the capabilities inside it.**

The portable concepts — what tools are allowed, what paths are reachable, what
hooks fire on what events, what plugins load — are engine-agnostic. Each engine
adapter translates them into its native config. Everything non-portable goes
through a per-engine `native` escape hatch.

Two layers:

1. **Generic capabilities** — `permissions`, `hooks`, `plugins`. Modeled
   structurally, translated per adapter.
2. **Per-engine `native` passthrough** — opaque key/value fragments merged
   verbatim into that engine's native config (e.g. `alwaysThinkingEnabled: false`
   → `settings.json`).

### Invariant: every major feature gets both layers

This is a structural requirement, not a per-feature judgement call. **There are
always going to be platform-specific things people want to do** — a Claude-only
permission grammar, a Codex-only hook event, an engine flag llmenv never models.
So for *every* major feature (permissions, hooks, plugins, MCP servers, …) the
schema MUST offer:

- **(a)** a *generic, engine-neutral* way to declare it (translated per adapter), and
- **(b)** an *engine-specific* `native` override that drops to that engine's own
  language and is emitted verbatim.

The override lives next to the feature it overrides: `permissions.native.<engine>`
for permission rules, `hooks.native.<engine>` for hook registrations, and so on.
The top-level `native.<engine>` block (D3) is the catch-all for keys that belong
to *no* modeled feature. A feature that has only layer (a) is **incomplete** —
the long tail of platform-specific needs has nowhere to go. Today only
`permissions` satisfies both layers; hooks, plugins, and MCP have (a) but not
(b), and the top-level `native` block is parsed but not yet wired through to the
adapter. These are tracked gaps (see *Implementation status*).

## Decisions

These were settled in design discussion (2026-05-27):

### D1 — Permission rule grammar: neutral + `native` override

Portable rules use a **neutral structured form** the adapter renders to its
engine's syntax. Rules that don't translate live in a **per-engine `native`**
override list.

```yaml
permissions:
  default_mode: acceptEdits        # neutral: acceptEdits | plan | default | bypassPermissions
  allow:
    - { tool: Bash, pattern: "git diff *" }
    - { tool: Read, paths: ["./src"] }
  deny:
    - { tool: Read, paths: ["./.env", "./.env.*"] }
  # engine-specific rules that have no neutral equivalent
  native:
    claude_code:
      deny: ["WebFetch(domain:internal.example.com)"]
```

The neutral `{tool, pattern}` / `{tool, paths}` form covers the common case
(tool + glob, tool + path roots). The Claude `Bash(...)`/`Read(./.env)` string
grammar is **generated** by the adapter, never authored. `native` is a per-engine
list of native rule strings appended verbatim — escape hatch for the long tail.
The word **`native`** is used everywhere for this "drop to the engine's own
language" move (here, and the top-level `native:` block in D3).

### D2 — Attach point: bundle-contributed fragments via `bundle.yaml`

Capabilities attach to **bundles**, not new top-level lists. But a bundle is a
**directory on disk**, not a config entry — today the `config.yaml` `bundle:`
list only carries name/tags/vars, while content lives in the bundle dir and
`merge()` (`src/merge/mod.rs:37`) walks fixed subdirs (`skills/`, `plugins/`,
`hooks/`) plus `AGENTS.md` and `rules/`. There is no per-bundle config file
today.

So fragments live in an **optional `bundle.yaml` inside the bundle directory**,
*not* as inline fields on the `config.yaml` entry. This keeps a bundle
self-contained: the hook *script* (`hooks/check.sh`) and its *registration* sit
together, the bundle versions as a unit, and copying the dir copies everything.
`merge()` reads `bundle.yaml` during the existing dir walk.

```yaml
# bundles/dev-core/bundle.yaml — identical shape to a top-level block
permissions:
  default_mode: acceptEdits
  allow:
    - { tool: Bash, pattern: "cargo *" }
hooks:
  - event: PreToolUse
    matcher: Bash
    handler: { type: command, command: "hooks/check.sh" }   # bundle-relative path
plugins:
  - "superpowers:superpowers"
```

**Hook command paths are bundle-relative** (`hooks/check.sh`), resolved against
the bundle dir at materialize time — the bundle doesn't know its final install
path.

#### Merge model: by value shape, not key identity

The capability structs are **identical** at top-level config and in
`bundle.yaml`. There is no list of "global-only" keys. Instead, how a value
merges across contributors (bundles + top-level) is determined by its **shape**:

- **Lists** (`allow`/`ask`/`deny`, `hooks`, `plugins`) → **concatenate + dedup**.
  Order-independent union; matches Claude Code's own array-merge. No winner
  problem.
- **Scalars** (`default_mode`, and scalars inside `native`) → **highest-precedence
  scope wins** (managed > local > project > user), matching Claude Code's own
  scalar override.

The rule is mechanical: *if the value is a list it merges; if it's a scalar the
highest-precedence scope wins.* A bundle **may** set `default_mode` — it's simply
a scalar, so precedence resolves it. Nothing is forbidden to a bundle.

**Same-precedence scalar collision** (e.g. two bundles at the same scope both
setting `default_mode` to different values) → **hard-error**, naming both
contributors. There's no scope to break the tie, and silent resolution hides a
real ambiguity. Loud beats silent.

### D3 — Per-engine `native` escape hatch (top-level)

A top-level `native` block carries opaque, unvalidated fragments per adapter:

```yaml
native:
  claude_code:                # merged verbatim into settings.json
    alwaysThinkingEnabled: false
    outputStyle: Explanatory
    cleanupPeriodDays: 30
  codex:
    model_reasoning_effort: high
```

llmenv does not understand these keys — it merges and emits them. Scalars here
follow the same precedence rule as D2. Generic capabilities (Layer 1) win on
conflict with a `native` key that a modeled capability also produces — or
hard-error (see O3).

## Hooks: finish the existing machinery

Hook *files* are already copied and `{{ICM_MCP}}`-substituted
(`claude_code.rs:52-61`, `is_hook_json`), but nothing registers them. The `hooks` fragment in
D2 (`bundle.yaml`) is the missing registration. For Claude Code, `{event, matcher, handler}` renders to
`hooks.{Event}: [{ matcher, hooks: [handler] }]`. Engines lacking a given event
drop that registration. Handler types mirror Claude Code's (`command`,
`mcp_tool`, …); the `mcp_tool` type could retire `{{ICM_MCP}}` substitution
entirely.

## Plugins

Folded into D2 as a `bundle.yaml` `plugins:` list of `<marketplace>:<plugin>` ids, plus
a top-level `marketplace` registry. This subsumes #59 — **but note #59 is
written against `config.toml`/TOML syntax; llmenv config is YAML
(`serde_yaml_ng`)**. #59 needs its examples re-expressed in YAML before
implementation.

## Schema changes

- New `Capabilities` struct (`permissions`, `hooks`, `plugins`) reused in two
  places: a top-level `Config` field, and a new `bundle.yaml` parsed during
  `merge()`. **Identical shape** in both.
- New `Permissions`, `Hook`, `Plugin` structs. `Permissions` carries
  `default_mode` (scalar), `allow`/`ask`/`deny` (lists of neutral rules), and
  `native: BTreeMap<String, _>` (per-engine raw rule strings).
- New `native` top-level map: `BTreeMap<String, serde_yaml::Value>` (opaque
  passthrough per engine).
- New `marketplace` top-level list (absorbs #59).
- `BundleRef`/`merge()` (`src/merge/mod.rs`) gain a `bundle.yaml` read; the
  `MergedManifest` carries merged `Capabilities`.
- **Rename** `Config.settings` → `Config.cache` (kills the "settings" name
  collision; no back-compat constraint on this project).

A shared merge routine implements the value-shape rule (list → concat+dedup;
scalar → precedence, hard-error on same-precedence collision) so top-level and
bundle fragments compose identically.

## Adapter changes

`generate_settings_json` becomes `generate_settings` that:

1. Takes the already-merged `Capabilities` + top-level `native.claude_code` from
   the manifest (merge happened in `merge()`, not here).
2. Renders neutral permission rules → Claude string grammar; appends
   `permissions.native.claude_code` rule strings verbatim.
3. Renders hook registrations into the `hooks` object. `{event, matcher,
   handler}` → `hooks.{Event}: [{ matcher, hooks: [handler] }]`. Resolves
   bundle-relative `command` paths against the install location.
4. Deep-merges top-level `native.claude_code` passthrough (capabilities win, or
   hard-error per O3).
5. Emits a correctly-shaped `settings.json` (object-valued `hooks` and
   `permissions`; **no `mcp` key**). Optionally emit `$schema`.

## Open questions

- **O1** — MCP enable/deny (`enabledMcpjsonServers`, etc.): derive from which
  servers llmenv emits, or model explicitly? Lean derive.
- **O2** — Neutral `default_mode` vocabulary: adopt Claude's
  (`acceptEdits`/`plan`/`default`/`bypassPermissions`) as the neutral set, or
  invent engine-neutral names? Claude's are reasonable defaults.
- **O3** — Conflict policy between modeled capability and top-level
  `native.<engine>` passthrough: capabilities-win vs. hard-error. Lean hard-error
  (loud beats silent). Note this is distinct from the same-precedence scalar
  collision in D2, which is already decided as hard-error.
- **O4** — Does ICM/memory replace Claude's auto memory? If so emit
  `autoMemoryEnabled: false` via the `native` escape hatch by default.

## Sequencing

1. Schema: rename `settings`→`cache`; add `Capabilities`/`Permissions`/`Hook`/
   `Plugin` structs + top-level `native` block; teach `merge()` to read
   `bundle.yaml`; add the shared value-shape merge routine.
2. Permissions generator (neutral→string + `permissions.native`), correct
   `settings.json` shape, drop bogus `mcp` key.
3. Hooks generator — wire the already-copied files in (bundle-relative paths).
4. Top-level `native.<engine>` passthrough merge.
5. Plugins + marketplaces (absorbs #59, re-expressed in YAML).

## Implementation status

Tracking the two-layer invariant (generic + per-engine `native`) per feature:

| Feature | (a) generic | (b) native override | Notes |
|---------|-------------|---------------------|-------|
| Permissions | done | done (`permissions.native.<engine>`) | rendered into `settings.json`; native wins over conflicting neutral rule |
| Hooks | done | **missing** | `hooks` list renders; no `hooks.native.<engine>` for engine-only events/handlers |
| Plugins | done | **missing** | `plugins` list; no per-engine override path |
| MCP servers | done | **missing** | `mcp` list resolves; no `mcp.native.<engine>` |
| Top-level `native` (catch-all) | n/a | **parsed, not wired** | `Config.native` deserializes but never reaches `merge()` → `MergedManifest` → adapter |

Open gaps tracked in their own issues — see the issue tracker (milestone M4.5)
for the top-level `native` passthrough wiring and the per-feature `native`
override for hooks/plugins/MCP.
