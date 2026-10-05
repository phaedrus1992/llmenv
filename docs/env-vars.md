<!-- markdownlint-disable MD013 -->
# Environment Variables — Naming & Usage

This document standardizes how llmenv handles environment variables across its codebase, configuration, and user-facing features.

## Categories

### 1. **llmenv Internal/IPC Variables** (`LLMENV_*` prefix — required)

Variables used for llmenv-internal communication, state management, or integration with its own subsystems. **Must** use the `LLMENV_` prefix.

| Variable | Purpose | Set By | Scope |
| ---------- | --------- | -------- | ------- |
| `LLMENV_STATE_DIR` | llmenv state directory. Only the Claude Code adapter exports it | llmenv adapter | Session/process |
| `LLMENV_PROJECT_ROOT` | Active project root directory | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_PROJECT` | Active project name | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_TAGS` | Comma-separated active tags | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_SCOPES` | Comma-separated active scopes | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_BUNDLES` | Comma-separated active bundles | llmenv scope matcher | Session/process |
| `LLMENV_ICM_CONTEXT` | ICM context chunk (active tags and bundles, for tag-scoped memory) | `llmenv export` | Session/process |
| `LLMENV_CONSOLIDATION_CHILD` | Set to `1` on the `claude -p` child of post-session consolidation, so that the child starts no consolidation of its own (added in v3.12.0) | llmenv consolidation | Child process |
| `LLMENV_VERSION` | llmenv version (compile-time) | llmenv binary | Build-time |
| `LLMENV_VERSION_TAG` | llmenv version tag (compile-time) | llmenv binary | Build-time |

**Rule:** All new internal/IPC variables **must** use the `LLMENV_` prefix. Exceptions only with justification in code comments.

### 2. **User-Set Configuration Overrides** (`LLMENV_*` prefix — required)

Variables the *user* sets to override or supplement llmenv's own config resolution — distinct
from category 1's internal/IPC variables, which llmenv itself sets as *output* for other
processes to read. These are read *by* llmenv as input.

| Variable | Purpose | Read By | Scope |
| ---------- | --------- | -------- | ------- |
| `LLMENV_CONFIG_DIR` | Config directory. Overrides the platform default (`~/.config/llmenv`). A set but empty value is ignored, and a leading `~` expands | `llmenv-paths` (`crates/llmenv-paths/src/lib.rs`) | Session/process |
| `LLMENV_TRACE_TIMING` | Set to any value to print timing and cache telemetry lines to stderr, such as `[LLMENV_CACHE]` and `[LLMENV_CONTEXT]`. See [Troubleshooting](../website/docs/troubleshooting.md) | llmenv hooks and materialize cache | Session/process |
| `LLMENV_NERD_FONT` | `1` or `true` tells the statusline `auto` icon mode that the terminal has a Nerd Font | `llmenv statusline` (`src/cli/statusline/icons.rs`) | Session/process |
| `LLMENV_UPGRADE_GITHUB_API` | Base URL of the GitHub API that `llmenv upgrade` queries. Default `https://api.github.com` | `llmenv upgrade` (`src/cli/upgrade.rs`) | Process |
| `LLMENV_EXTRA_TAGS` | Comma-separated tags unioned into the active tag set, additive on top of `.llmenv.yaml`'s `tags` (or on top of nothing, if no `.llmenv.yaml` is present) — an escape hatch for activating tags without a committed project marker. Each tag must be alphanumeric plus `-`/`_`, ≤64 bytes, and the source is capped at 64 tags; anything outside that is dropped (`tracing::warn!`, visible with `RUST_LOG=warn`) instead of silently disabling ICM memory/session-logging for the session (#1035) | llmenv scope matcher (`src/scope/matcher.rs`) | Session/process |

**Rule:** Same `LLMENV_` prefix rule as category 1 — these are still llmenv-owned, just input
rather than output.

### 3. **External Tool Variables** (no `LLMENV_` prefix)

Variables for controlling external LLM CLI tools, tools installed on the system, or third-party libraries. These should **not** use the `LLMENV_` prefix to avoid confusion with llmenv's own settings.

This table covers llmenv-managed variables and well-known system vars.
**Claude Code's own env vars are documented upstream at
[code.claude.com/docs/en/env-vars](https://code.claude.com/docs/en/env-vars)** — see
there for the canonical list instead of maintaining a stale local copy.

| Variable | Purpose | Tool | Scope |
| ---------- | --------- | ------ | ------- |
| `CLAUDE_CODE_PLUGIN_CACHE_DIR` | Plugin cache root for Claude Code (overrides `<CLAUDE_CONFIG_DIR>/plugins/`). Set to `<adapter_root>/state/plugins/` — outside the per-hash config dir so plugins survive agent re-materialization without re-download (#632) | Claude Code | Process |
| `CLAUDE_CODE_TMPDIR` | Temp directory for Claude Code intermediate files, session scratch space, and large tool outputs. Set to `<state_dir>/tmp/` (#630; moved out of the per-hash cache folder in 3.11.0, [#1379](https://github.com/phaedrus1992/llmenv/issues/1379)) — the state dir is never pruned, so the path stays valid for shells that started before a `llmenv prune` or a config edit. Not garbage-collected by `llmenv prune`. Also exported as `TMPDIR`, `TMP`, and `TEMP` | Claude Code | Process |
| `CONTEXT_MODE_DATA_DIR` | context-mode plugin state directory, normalized to forward slashes for cross-platform compatibility. Set by llmenv's durable-state relocation (#175, #490) | context-mode marketplace plugin | Process |
| `CBM_CACHE_DIR` | codebase-memory-mcp cache directory. llmenv sets it only when `features.codebase_memory[].index_path` is set | codebase-memory-mcp | Process |
| `CBM_MEM_BUDGET_MB` | codebase-memory-mcp indexing memory budget. llmenv sets it from `features.codebase_memory[].mem_budget_mb` (added in v3.12.0) | codebase-memory-mcp | Process |
| `ANTHROPIC_API_KEY` | API key for the `anthropic-api` consolidation backend. llmenv reads it and does not set it | Anthropic API | Process |
| `ANTHROPIC_MODEL` | Model ID for the `anthropic-api` consolidation backend. It must start with `claude-` | Anthropic API | Process |
| `CRUSH_GLOBAL_CONFIG` | Directory containing `crush.json` (rendered by llmenv) — Crush joins `crush.json` onto this itself | Crush CLI | User session |
| `CRUSH_GLOBAL_DATA` | Crush state directory (points at `LLMENV_STATE_DIR`) | Crush CLI | User session |
| `HOME` | User home directory | System | System-wide |
| `PATH` | Executable search path | System | System-wide |
| `EDITOR` | Default text editor | System | System-wide |
| `XDG_STATE_HOME` | XDG state directory | System / freedesktop.org | System-wide |
| `RUST_*` | Rust toolchain variables | Rust / cargo | System-wide |
| `CARGO_*` | Cargo build variables | Cargo / Rust | System-wide |

**Rule:** External tool variables should use the tool's existing naming convention. Do **not** prefix them with `LLMENV_` — that's reserved for llmenv internals only.

### 4. **Bundle-Provided Variables** (user-defined, optional prefix)

Variables that bundles define for their own use (e.g., token-efficiency bundle thresholds). These can be named freely, but never with the `LLMENV_` prefix: validation rejects any `LLMENV_*` key in `capabilities.env` and in an MCP server's `env`. Use a tool-specific prefix or no prefix.

Example:

```yaml
# ✅ OK: no prefix for variables that are just bundle configuration
env:
  CBM_WARN_THRESHOLD: 50000
  CBM_AUTOINDEX: "true"
```

> **Note:** Token-efficiency is now a built-in feature, not an env var. Enable
> it with `features.context_mode.enabled: true` (wires the context-mode plugin
> automatically). The former `LLMENV_BASH_BAN` env var was removed in #490.
>
> The built-in marketplace source (`CONTEXT_MODE_SOURCE`) is pinned to a fixed
> release tag, not a floating `HEAD` ref — every `llmenv regenerate` must
> resolve the same plugin content until llmenv itself deliberately bumps the
> pin in a release (#496).

## Validation & Enforcement

**At bundle load time:**

- Any key in `capabilities.env` (config or bundle) or in an MCP server's `env` that starts with `LLMENV_` is rejected. Validation also rejects the keys llmenv sets itself (`LLMENV_STATE_DIR`, `CLAUDE_CONFIG_DIR`) and any key that is not a valid shell identifier. The error names the key and the fix.
- `LLMENV_OWNED_SETTINGS_KEYS` (`src/adapter/claude_code/mod.rs`) is a different list: the `settings.json` keys that llmenv renders and replaces on every re-render. It does not allow env vars.

**At test time:**

- Property tests verify that capabilities validation rejects every `LLMENV_*` key.
- Example bundles are validated by the test suite to catch documentation drift.

## Auditing for New Variables

When auditing for new or inconsistent variables:

1. Search codebase for env var usage: `grep -r 'env::var\|std::env'`
2. Categorize each by purpose (internal, external, bundle-config)
3. Update code/docs as needed
4. Add new internal variables to the allowlist with a comment explaining purpose

## See Also

- [`AGENTS.md`](../AGENTS.md) — agent rules for this repo
- [`RELEASING.md`](../RELEASING.md) — release checklist and version management
