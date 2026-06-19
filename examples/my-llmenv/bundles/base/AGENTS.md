# Global Development Standards

<!--
  This file is injected into every Claude Code session as part of the agent's
  system context (equivalent to CLAUDE.md at the project level, but global).

  It lives in bundles/base/ so it activates whenever the `user-alice` tag is
  present — i.e., every session. Project-level CLAUDE.md files take precedence
  over this file when they conflict, so teams can override these defaults.

  Sections here should be things you want enforced universally across every
  project, not project-specific rules. Keep it focused and scannable.
-->

Project CLAUDE.md override defaults.

## Memory/Tools

- Use plugins, skills, MCPs proactively when they match the task
- Memory: use ICM (`icm_*` MCP calls) for anything beyond a single session
- Be aggressive with ICM for code changes, design decisions, feature dev, research

## Working Relationship

- Not deferential — disagreement is OK
- No sycophancy
- Matter-of-fact, clear, concise
- Challenge assumptions
- Do the right thing, not the easy thing
- No guessing — use tools, memory, or ask
- **No pause mid-task just because of scope size.** Drive end-to-end. Pause only:
  (a) truly blocked on missing info, (b) destructive/irreversible action not authorized,
  (c) scope has materially changed. Big scope ≠ pause reason.

## Philosophy

- No speculative features (add only what's needed)
- No premature abstraction (three-times rule)
- Explicit over clever
- Justify dependencies (attack surface)
- No phantom features
- Replace, don't deprecate. Flag dead code.
- Verify at every level (linters, types, tests, reviews)
- Bias toward action (reversible is OK)
- Finish the job (edges, cleanup, adjacent bugs)
- No bug or feature left silently ignored — either file an issue or fix it now

## Code Quality

Hard limits:
1. ≤100 lines/function, cyclomatic complexity ≤8
2. ≤5 positional parameters
3. 100 character line limit
4. Absolute imports only (no `..`)
5. JSDoc on non-trivial public APIs

Zero warnings: fix all linter/type/compiler/test warnings. Inline suppression +
comment only for unavoidable cases.

Comments: code self-documents. No commented-out code. If you need a comment to
explain WHAT, refactor instead.

Error handling: fail fast with context (operation, input, suggested fix). Never
swallow errors silently.

## Task vs Issue Tracking

Two separate things — do not conflate.

**Issue tracking** = bugs/features/planning — permanent.
- GitHub Issues enabled on project → use GitHub Issues for ALL planning.
- No GitHub Issues → use `yx` (Yaks).

**Task tracking** (`yx`) = ephemeral work-in-progress steps only.

**Before coding:** check existing issues for prior work.

Yaks commands:
```
yx ls --format json                         # discover work
yx state "fix bug" wip                      # claim
echo "notes" | yx field "fix bug" progress  # update
yx done "fix bug"                           # complete
yx sync                                     # sync team
```

## Development Standards

Look up current stable versions for deps/CI/tools. Never assume from memory.

### CLI Tools

| Tool | Replaces | Usage |
|------|----------|-------|
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

### Python

**Runtime:** 3.13 + `uv venv`

| Purpose | Tool |
|---------|------|
| deps | `uv` |
| lint/format | `ruff check` / `ruff format` |
| types | `ty check` |
| tests | `pytest` |

Always `uv run python3` (never bare `python3`).

### Node/TypeScript

**Runtime:** Node 22 LTS, ESM only. Use `oxlint` / `oxfmt`.

tsconfig: `strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`.

### Rust

**Runtime:** Latest stable via `rustup`

| Purpose | Tool |
|---------|------|
| build | `cargo` |
| lint | `cargo clippy --all-targets --all-features -- -D warnings` |
| format | `cargo fmt` |
| test | `cargo test` |
| supply chain | `cargo deny check` |

**Style:** `for` > iterators. Shadow vars. No wildcards. `let...else` for early returns.

Cargo.toml lints: `pedantic` = warn. Deny: `unwrap`, `panic`, `dbg_macro`, `todo`,
`print`, `unsafe_code`, `exit`, `mem_forget`.

### Bash

Start: `set -euo pipefail`. Lint: `shellcheck` + `shfmt`.

macOS: no `grep -P` (use `grep -E` or `rg`). Test hooks on macOS.

### GitHub Actions

Pin to SHA + version comment. Scan with `zizmor`. Dependabot: 7-day cooldown,
grouped updates. Use `uv` not pip.

## Workflow

**Before commit:**
1. Re-read changes (complexity, duplication, naming)
2. Run relevant tests
3. Run linters + type checker (fix all warnings)

**Branch protection:**
- **NEVER commit directly to `main`** — all changes via feature branch + PR
- Always use feature branch (`fix/`, `docs/`, `feat/`, etc.)

**Commits:**
- Imperative mood, ≤72 char subject, one logical change
- No "by claude", no "Co-Authored-By"
- Never amend/rebase pushed commits
- Never commit secrets — use `.env` + env vars

**PRs:**
- Title ≤70 chars
- Describe what the code does now, not discarded approaches
- Plain language: bug fix = bug fix
- Monitor CI (`gh run watch` / `gh pr checks --watch`)
