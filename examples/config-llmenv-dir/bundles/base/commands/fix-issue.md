# Fix GitHub Issue

___

- description: "End-to-end: plan, implement, test, review, fix, push, and PR for a GitHub issue"
- arguments: `$ISSUE_NUMBER`: GitHub issue number to fix

___

Read Issue #$ARGUMENTS from canonical repo
(`gh issue view $ARGUMENTS --repo <owner/name>`).
Understand context: problem, acceptance criteria, linked PRs, discussion.
Follow linked issues, PRs, external docs for complete understanding before planning.

Detect upstream repo: if git remote `upstream` exists, use it (fetch, branch, PR).
Otherwise fall back to `origin`. Resolve canonical repo `owner/name`
(e.g. from `git remote get-url upstream`) and store — use `--repo <owner/name>`
on every `gh` command (views, PR creation, comments) to target correct repo.
Run `git fetch <upstream-remote>` for up-to-date code.

Execute every step sequentially. Do not stop or ask confirmation at any step.

## 1. Research (if needed)

Before planning, determine if issue needs external context: unfamiliar APIs, protocols, libraries,
error messages, domain concepts. If so, use WebSearch for:

- Official docs for referenced libraries/APIs
- Known solutions for error messages/symptoms
- Implementation patterns from similar projects

Skip for straightforward bugs where fix is clear from codebase.

## 2. Plan

Write detailed implementation plan to `plan-issue-$ISSUE_NUMBER.md` in repo root.
Plan must:

- Summarize issue requirements
- List every file to create/modify
- Describe approach and key design decisions
- Call out risks or open questions
- Reference relevant code paths by file:line

## 3. Create branch

Create working branch before writing code (changes never left uncommitted on main).

- Determine branch prefix: `fix/` for bugs, `feat/` for features, `refactor/` for refactors,
  `docs/` for documentation. Ambiguous → use `fix/`.
- Create branch `{prefix}issue-$ISSUE_NUMBER` from upstream remote's main branch
  (e.g. `upstream/main` if exists, else `origin/main`)

## 4. Implement

Implement plan across all necessary files. Follow project's CLAUDE.md standards.
Keep changes minimal and focused on issue requirements — no speculative features.

Add tests for changed behavior as part of implementation — tests are code, not quality gate.

When stuck (confusing error, unfamiliar API, broken approach), use WebSearch for solutions.

## 5. Build, test, lint

### 5a. Discover project checks (CI is source of truth)

Before running anything, read project's CI config to learn what project *actually* runs.
Priority over fallback tables.

1. **Read CI workflows.** Scan `.github/workflows/` for main CI workflow
   (typically `ci.yml`, `test.yml`, `build.yml`). Extract:
   - Test commands with feature flags (e.g. `cargo test --features foo,bar`)
   - Lint/format commands with non-default flags
   - Steps that run command then check `git diff --exit-code` → **codegen sync checks**
     (schema generation, snapshots, help text, etc.). Record command.
   - Docs/site build commands (e.g. `make site`, `mkdocs build`)
2. **Read Makefile** (if present). Cross-reference targets used in CI — those matter.
3. **Read CLAUDE.md** (if at repo root or `.claude/`). May define project-specific quality gates.

Store discovered commands. Override fallback table for overlapping steps.

### 5b. Run quality pipeline

Detect project language from manifest files (`Cargo.toml` → Rust,
`pyproject.toml`/`setup.py` → Python, `package.json` → Node/TypeScript, `go.mod` → Go).
Projects may use multiple languages; run checks for each.

Run checks in order. For each step, use CI-discovered command if found; else fallback.

1. **Build** — compile or bundle
2. **Test** — run full test suite with same feature flags as CI. Iterate failures until green.
3. **Lint and format** — fix issues
4. **Extended checks** — per-language extras (see fallback table)
5. **Codegen sync** — for every codegen check from 5a, run command and verify `git diff --exit-code`.
   Non-empty diff → generated files stale. Regenerate and stage.
6. **Docs build** — if changes touch docs and docs build command exists, run to verify compile.

### Fallback defaults (when CI config absent or unclear)

**Rust** (`Cargo.toml`):

| step         | command                                             |
|--------------|-----------------------------------------------------|
| build        | `cargo build`                                       |
| test         | `cargo test`                                        |
| lint         | `cargo clippy -- --deny warnings`                   |
| format       | `cargo fmt --check`                                 |
| supply chain | `cargo deny check` (if `deny.toml` exists)          |
| careful      | `cargo careful test` (if `cargo-careful` installed) |

**Python** (`pyproject.toml` or `setup.py`):

| step         | command                                        |
|--------------|------------------------------------------------|
| test         | `pytest -q`                                    |
| lint         | `ruff check`                                   |
| format       | `ruff format --check`                          |
| types        | `ty check` (or `mypy` if configured)           |
| supply chain | `pip-audit`                                    |

**Node/TypeScript** (`package.json`):

| step         | command                                        |
|--------------|------------------------------------------------|
| build        | per project (`npm run build`, `tsc`, etc.)     |
| test         | `vitest` (or project test script)              |
| lint         | `oxlint` (or project lint script)              |
| format       | `oxfmt --check` (or project format script)     |
| types        | `tsc --noEmit`                                 |
| supply chain | `pnpm audit --audit-level=moderate`            |

**Go** (`go.mod`):

| step         | command                                        |
|--------------|------------------------------------------------|
| build        | `go build ./...`                               |
| test         | `go test ./...`                                |
| lint         | `golangci-lint run`                            |
| format       | `gofmt -l .`                                   |
| vet          | `go vet ./...`                                 |

Tool not installed → skip with note. Don't fail pipeline.

## 6. Self-review

Docs-only changes: focused manual review (verify links, check prose, confirm rendering).
Code changes: use `/pr-review-toolkit:review-pr` for deep review against diff
(compare working tree to upstream main). Produce findings ranked by severity
(P1 = blocks merge, P2 = important, P3 = nice to have).

## 7. Fix findings

Address all P1–P3 findings. For each, either:

- **Fix it** — apply change, or
- **Dismiss it** — explain why false positive or not worth churn (stylistic disagreement,
  impossible edge case). Document reasoning inline.

After addressing findings, review own fixes: read diff from this step and verify
each fix is correct, doesn't introduce new issues, doesn't regress implementation.
Problem found → fix before proceeding.

Re-run full quality pipeline (build, test, lint). Iterate until clean.

## 8. Commit and push

- Delete plan file (`plan-issue-$ISSUE_NUMBER.md`) — working artifact, don't commit
- Commit all changes with conventional commit message referencing issue
- Push branch

## 9. Create PR

Create PR with:

- Concise title (under 70 chars)
- Description that maps changes to issue requirements
- Link to issue with "Closes #$ISSUE_NUMBER" (or "Refs" if doesn't fully close)
- If `upstream` remote exists, submit PR to upstream repo using
  `gh pr create --repo <upstream-owner/repo>`

## 10. Comment on issue

Post summary comment on Issue #$ISSUE_NUMBER in canonical repo
(`gh issue comment $ISSUE_NUMBER --repo <owner/name>`) linking to PR. Include:

- What was implemented (1–3 bullets)
- Key design decisions
- Link to PR
