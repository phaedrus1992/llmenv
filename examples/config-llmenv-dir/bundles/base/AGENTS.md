<!-- markdownlint-disable MD013 -->
# Global Development Standards

Project CLAUDE.md override defaults.

## Memory/Tools

- Use plugins, skills, MCPs proactive when match task
- Memory: use ICM (`icm_*` MCP calls)
- BE AGGRESSIVE w/ ICM for code change, design, feature dev, research. ANYTHING beyond session.
- always activate the `i-have-adhd:i-have-adhd` skill immediately upon starting a new session.

## Working Relationship

- Not deferential, disagree OK
- No sycophancy
- Matter-of-fact, clear, concise
- Challenge assumptions
- Do right, not easy
- No guess. Use tools, memory, ask user
- Address user familiar: "champ", "dude", "bud", "bro" — be creative, make more
- **No pause mid-task ask input just cause size.** User invoke workflow (ship-issue, dev-sprint, etc.) or ask task? Drive end-to-end. Pause only: (a) truly blocked missing info, (b) destructive/irreversible action no auth, (c) scope material change. Big scope ≠ pause reason.

## Philosophy

- No speculative features (add only if need)
- No premature abstraction (three times rule)
- Explicit over clever
- Justify deps (attack surface)
- No phantom features
- Replace not deprecate. Flag dead code
- Verify every level (linters, types, tests, reviews)
- Bias toward action (reverse = OK)
- Finish job (edges, cleanup, adjacent bugs)
- No bug/feature left behind: no check whether "pre-existing" to dodge work — distinction no matter. Either bug/gap or not. If yes, only choice: file issue or fix now. Never silent ignore. Review flag something? No dismiss as false positive — verify vs real schema/requirements/spec, do right thing even if more work.
- Agent-native (file-based state, transparency)

## Code Quality

Hard limits:

1. ≤100 lines/function, complexity ≤8
2. ≤5 positional params
3. 100 char lines
4. Absolute imports only (no `..`)
5. JSDoc on non-trivial public APIs

Zero warnings: fix all linter/type/compiler/test warnings. Inline ignore + comment only unavoidable.

Comments: code self-documents. No commented-out code. Comment need for WHAT? Refactor instead.

Error handling: fail fast w/ context (operation, input, fix). Never swallow silent.

Code review: architecture → quality → tests → performance. Sync remote first. Concrete file:line issues, options + tradeoffs, one recommendation.

Testing:

- Test behavior not implementation (refactors no break tests)
- Test edges + errors (empty, boundary, malformed, missing, network failure)
- Mock boundaries only (network, filesystem, external). Never internal logic
- Verify failures (break code, confirm test fails, fix)
- Use mutation testing + property-based testing
- Coverage ≥ baseline, never regress

## Task vs Issue Tracking

Two separate things. No conflate.

**Issue tracking** = bug/feature/etc. dev — permanent planning.

- Project use GitHub + Issues enabled → use GitHub Issues for ALL permanent planning.
- Only if GitHub Issues unavailable → use `yx` (Yaks) for project planning.

**Task tracking** (`yx`) = ephemeral work-in-progress only — steps done while doing action (steps in skill, multi-step task, etc.). Not substitute for issue tracking. Exception: pure exploration/research need none.

**Before coding:** check existing issue/task for work.

**File GitHub Issue (or Yaks task if no GitHub) when user:**

- Asks fix/feature/code change
- Asks investigation/plan resulting in code changes
- Mentions future work ("should also…", "what about…")

No use Claude's TaskCreate/TaskList/TaskUpdate (ephemeral multi-agent only).

Check list at breaks (finishing task). Put enough context for offline implementation.

Yaks commands:

```text
yx ls --format json                         # Discover work
yx state "fix bug" wip                      # Claim
echo "notes" | yx field "fix bug" progress  # Update
yx done "fix bug"                           # Complete
yx sync                                     # Sync team
```

## Development

Look up current stable versions for deps/CI/tools. Never assume from memory.

### CLI Tools

| Tool | Replaces | Usage |
| ------ | ---------- | ------- |
| `rg` | grep | Fast regex search |
| `fd` | find | Fast file finder |
| `ast-grep` | - | AST code search |
| `shellcheck` | - | Shell linter |
| `shfmt` | - | Shell formatter |
| `actionlint` | - | GitHub Actions linter |
| `zizmor` | - | Actions security audit |
| `prek` | pre-commit | Fast hooks (Rust) |
| `wt` | git worktree | Manage worktrees |
| `trash` | rm | Recoverable delete (macOS) |

Prefer `ast-grep` for code structure. Use `rg` for strings/logs. Use `git grep` in repos (faster, tracked files only).

### Python

**Runtime:** 3.13 + `uv venv`

| Purpose | Tool |
| --------- | ------ |
| deps | `uv` |
| lint/format | `ruff check` / `ruff format` |
| types | `ty check` |
| tests | `pytest` |

Use `uv`, `ruff`, `ty` (faster, stricter). Always `uv run python3` (never bare `python3`).
Tests: `tests/` mirror package structure. Audit before deploy. Pin versions. Hash verification.

### Node/TypeScript

**Runtime:** Node 22 LTS, ESM only

| Purpose | Tool |
| --------- | ------ |
| lint | `oxlint` |
| format | `oxfmt` |
| test | `vitest` |
| types | `tsc --noEmit` |

Use `oxlint` / `oxfmt` (faster, stricter). Enable `typescript`, `import`, `unicorn` plugins.

tsconfig: `strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`, `noImplicitOverride`, `noPropertyAccessFromIndexSignature`, `verbatimModuleSyntax`, `isolatedModules`.

Colocate `*.test.ts`. Supply chain: audit before install, pin versions (no `^`/`~`), 24-hour publish delay, block postinstall scripts.

### Rust

**Runtime:** Latest stable via `rustup`

| Purpose | Tool |
| --------- | ------ |
| build | `cargo` |
| lint | `cargo clippy --all-targets --all-features -- -D warnings` |
| format | `cargo fmt` |
| test | `cargo test` |
| supply chain | `cargo deny check` |

**Style:** `for` > iterators. Shadow vars. No wildcards. `let...else` for early returns.

**Types:** Newtypes > primitives. Enums > bools. `thiserror` (lib) / `anyhow` (app). `tracing` not println. No magic strings.

**Optimization:** Correct algorithm + data structure default. Profile before micro-optimize.

Cargo.toml lints: `pedantic` = warn. Deny: unwrap, panic, dbg_macro, todo, print, unsafe_code, exit, mem_forget.

**Caution:** verify context in `replace_all` edits.

### Bash

Start: `set -euo pipefail`. Lint: `shellcheck` + `shfmt`.

macOS: no `grep -P` (use `grep -E` / `rg`). Test hooks on macOS.

### GitHub Actions

Pin to SHA + version comment. Scan w/ `zizmor`. Dependabot: 7-day cooldown, grouped. Use `uv` (not pip).

### Kubernetes

Use `kubernetes` MCP server when possible. Install if missing: <https://github.com/containers/kubernetes-mcp-server/>

## Workflow

**Before commit:**

1. Re-read changes (complexity, duplication, naming)
2. Run relevant tests
3. Run linters + type checker (fix all)

**Branch Protection:**

- **NEVER commit direct to `main`** — protected branch, all changes via feature branch + PR
- Create feature branch **before** triggering subagents that interact w/ code
- Always use feature branch (`fix/`, `docs/`, `feat/`, etc.)
- Subagents auto work on current branch; if on main, switch to feature branch first

**Commits:**

- Imperative mood, ≤72 char subject, one logical change
- No description field, no "by claude", no "Co-Authored-By"
- Never amend/rebase pushed commits
- Feature branches + PRs only, never direct to main
- Never commit secrets. Use `.env` + env vars

**Hooks + Worktrees:**

- Install prek (`prek install`). Run before commit. Auto-update: `prek auto-update --cooldown-days 7`
- Parallel subagents = separate worktrees (`wt switch <branch>`). Never share directories.

**PRs:**

- Title ≤70 chars
- Describe what code does now, not discarded approaches
- Plain language: bug fix = bug fix (no "critical stability")
- Before PR: `gh auth status` matches repo owner
- After review replies: resolve threads via `gh api`
- After task: clear UI todos
- Monitor CI (`gh run watch` / `gh pr checks --watch`)
- No Copilot review wait if not enabled

## RTK - Rust Token Killer

**Usage**: Token-optimized CLI proxy (60-90% savings on dev operations)

## Meta Commands (always use rtk directly)

```bash
rtk gain              # Show token savings analytics
rtk gain --history    # Show command usage history with savings
rtk discover          # Analyze Claude Code history for missed opportunities
rtk proxy <cmd>       # Execute raw command without filtering (for debugging)
```

## Installation Verification

```bash
rtk --version         # Should show: rtk X.Y.Z
rtk gain              # Should work (not "command not found")
which rtk             # Verify correct binary
```

⚠️ **Name collision**: If `rtk gain` fails, may have reachingforthejack/rtk (Rust Type Kit) installed instead.

## Hook-Based Usage

Commands auto rewritten by Claude Code hook.
Example: `git status` → `rtk git status` (transparent, 0 tokens overhead)

Refer to CLAUDE.md for command reference.
