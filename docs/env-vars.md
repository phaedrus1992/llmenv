# Environment Variables — Naming & Usage

This document standardizes how llmenv handles environment variables across its codebase, configuration, and user-facing features.

## Categories

### 1. **llmenv Internal/IPC Variables** (`LLMENV_*` prefix — required)

Variables used for llmenv-internal communication, state management, or integration with its own subsystems. **Must** use the `LLMENV_` prefix.

| Variable | Purpose | Set By | Scope |
|----------|---------|--------|-------|
| `LLMENV_STATE_DIR` | llmenv state directory (config, cache, sessions) | llmenv adapter | Session/process |
| `LLMENV_CONFIG` | Path to active config file | llmenv adapter | Session/process |
| `LLMENV_CONFIG_DIR` | Path to config directory | llmenv adapter | Session/process |
| `LLMENV_PROJECT_ROOT` | Active project root directory | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_PROJECT` | Active project name | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_TAGS` | Colon-separated active tags | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_SCOPES` | Colon-separated active scopes | llmenv scope matcher | Session/process |
| `LLMENV_ACTIVE_BUNDLES` | Colon-separated active bundles | llmenv scope matcher | Session/process |
| `LLMENV_ICM_CONTEXT` | ICM context chunk (from memory store) | llmenv SessionStart | Session/process |
| `LLMENV_BASH_BAN` | Bash commands to deny in PostToolUse | User config bundle | Session/process |
| `LLMENV_VERSION` | llmenv version (compile-time) | llmenv binary | Build-time |
| `LLMENV_VERSION_TAG` | llmenv version tag (compile-time) | llmenv binary | Build-time |

**Rule:** All new internal/IPC variables **must** use the `LLMENV_` prefix. Exceptions only with justification in code comments.

**Validation:** The adapter's capabilities validation rejects any bundle env var using `LLMENV_` that wasn't explicitly declared as reserved — this prevents accidental shadowing of internal variables.

### 2. **External Tool Variables** (no `LLMENV_` prefix)

Variables for controlling external LLM CLI tools, tools installed on the system, or third-party libraries. These should **not** use the `LLMENV_` prefix to avoid confusion with llmenv's own settings.

| Variable | Purpose | Tool | Scope |
|----------|---------|------|-------|
| `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` | Claude Code context compaction threshold | Claude Code CLI | User session |
| `BASH_MAX_OUTPUT_LENGTH` | Max bash command output length | Claude Code tool dispatch | User session |
| `MAX_MCP_OUTPUT_TOKENS` | Max MCP tool output token count | Claude Code integration | User session |
| `ENABLE_PROMPT_CACHING_1H` | Enable 1-hour prompt caching | Claude API / Claude Code | User session |
| `HOME` | User home directory | System | System-wide |
| `PATH` | Executable search path | System | System-wide |
| `EDITOR` | Default text editor | System | System-wide |
| `XDG_STATE_HOME` | XDG state directory | System / freedesktop.org | System-wide |
| `RUST_*` | Rust toolchain variables | Rust / cargo | System-wide |
| `CARGO_*` | Cargo build variables | Cargo / Rust | System-wide |

**Rule:** External tool variables should use the tool's existing naming convention. Do **not** prefix them with `LLMENV_` — that's reserved for llmenv internals only.

### 3. **Bundle-Provided Variables** (user-defined, optional prefix)

Variables that bundles define for their own use (e.g., token-efficiency bundle thresholds). These can be named freely, but **recommended to use `LLMENV_` if they're configuration that affects llmenv's own behavior**, and **recommended NOT to use `LLMENV_` if they're just helper variables for the bundle's own rules/hooks**.

Example:

```yaml
# ✅ Recommended: LLMENV_ prefix for variables that affect llmenv behavior
env:
  LLMENV_BASH_BAN: "cat,find,grep"

# ✅ OK: no prefix for variables that are just bundle configuration
env:
  CBM_WARN_THRESHOLD: 50000
  CBM_AUTOINDEX: "true"
```

## Validation & Enforcement

**At bundle load time:**
- Any env var in a bundle starting with `LLMENV_` must be in the explicit `LLMENV_OWNED_SETTINGS_KEYS` allowlist. Rejected bundles fail with a clear error message.
- New internal variables require an entry in both the code (adapter / scope matcher) and the allowlist constant.

**At test time:**
- Property tests verify that capabilities validation correctly rejects non-allowlisted `LLMENV_*` keys.
- Example bundles are validated by the test suite to catch documentation drift.

## Migration Path (for existing code)

The audit in #466 found that `BASH_BAN` was already standardized to `LLMENV_BASH_BAN` in code (#465), but documentation lag existed. This was fixed:

- ✅ CHANGELOG.md updated
- ✅ website/docs/changelog.md updated
- ✅ examples/my-llmenv/bundles/token-efficiency/rules/bash.md updated

Future audits should:
1. Search codebase for env var usage: `grep -r 'env::var\|std::env'`
2. Categorize each by purpose (internal, external, bundle-config)
3. Update code/docs as needed
4. Add new internal variables to the allowlist with a comment explaining purpose

## See Also

- [`CLAUDE.md`](../CLAUDE.md) — global development standards (covers env var conventions in **Workflow** section)
- [`rules/general.md`](../rules/general.md) — general coding rules (includes env var validation principles)
