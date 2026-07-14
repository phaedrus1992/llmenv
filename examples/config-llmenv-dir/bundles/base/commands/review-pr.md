<!-- markdownlint-disable MD013 -->
# Review and Fix PR

___

- description: "Review an existing PR with parallel agents, fix findings, and push"
- arguments: `$PR_NUMBER`: GitHub PR number to review and fix

___

Read PR #$ARGUMENTS via `gh pr view`—context: description, linked issues, commits, diff vs base.

Use `upstream` remote if it exists; else `origin`. Resolve `owner/name` via `git remote get-url`. Use `--repo <owner/name>` on all `gh` commands. Run `git fetch <upstream-remote>` for latest.

Check out PR branch.

Execute steps sequentially. No stops, no confirmation.

## 1. Review

Run two review passes in parallel, then merge findings.

### Pass A — pr-review-toolkit agents

Launch agents **in parallel** (single message, multiple calls) with `subagent_type` from pr-review-toolkit. Tell each which files changed (`git diff --name-only <base>...HEAD`):

| agent | focus |
| ------- | ------- |
| `pr-review-toolkit:code-reviewer` | Code quality, style, project guidelines |
| `pr-review-toolkit:silent-failure-hunter` | Silent failures, swallowed errors, bad fallbacks |
| `pr-review-toolkit:pr-test-analyzer` | Test coverage gaps and missing edge cases |

### Pass B — external second opinion

Launch agents **in parallel with Pass A**—all 5 in one message, multiple calls. Use `subagent_type: general-purpose`.

**Codex reviewer** — tell the agent to run:

```bash
codex review --base <upstream-remote>/<base-branch> \
  -c model='"gpt-5.3-codex"' \
  -c model_reasoning_effort='"xhigh"'
```

- `--base` no custom prompts (codex reads `AGENTS.md` if exists)
- If `gpt-5.3-codex` auth fails, retry `gpt-5.2-codex`
- Set `timeout: 600000` on the Bash call
- Summarize findings only—skip `[thinking]`/`[exec]` blocks, sandbox warnings
- If `codex` not installed, report and skip

**Gemini reviewer** — tell the agent to run:

```bash
git diff <upstream-remote>/<base-branch>...HEAD > /tmp/pr-review-diff.txt

# Build prompt file (avoids heredoc shell expansion issues)
{
  echo "Review this diff for code quality, bugs, and improvements."
  if [ -f CLAUDE.md ] || [ -f .claude/CLAUDE.md ]; then
    echo ""
    echo "Project conventions:"
    echo "---"
    cat CLAUDE.md .claude/CLAUDE.md 2>/dev/null
    echo "---"
  fi
  echo ""
  echo "Diff:"
  cat /tmp/pr-review-diff.txt
} > /tmp/pr-review-prompt.txt

# Pipe prompt via stdin to avoid shell metacharacter issues
cat /tmp/pr-review-prompt.txt | gemini -p - \
  -m gemini-3-pro-preview \
  --yolo
```

- Uses stdin (`-p -`) to avoid shell expansion of `$`, backticks in diffs
- Set `timeout: 600000` on the Bash call
- If `gemini` not installed, report and skip

### Merge findings

Collect results from all 5 sources. Deduplicate—keep most specific description, note consensus. Rank by severity:

- **P1** — blocks merge (correctness, security)
- **P2** — important (error handling, test gaps, logic)
- **P3** — nice to have (style, naming, simplifications)
- **P4** — informational (observations, future work)

## 2. Fix findings

Address P1–P3 findings. For each, either:

- **Fix it** — apply change, or
- **Dismiss it** — explain false positive/not worth churn. Document reasoning.

For fixes needing context (unfamiliar library, unclear API, unknown error), use WebSearch.

P4 findings informational—note but don't fix unless trivial.

After fixes, review diff—verify correct, no new issues, no regressions. Fix problems before proceeding.

## 3. Verify

### 3a. Discover project checks (CI is the source of truth)

Read CI config first—learn what project actually runs. Overrides fallback tables.

1. **Read CI workflows.** Scan `.github/workflows/` for main CI (typically `ci.yml`, `test.yml`, `build.yml`).
   Extract:
   - Test commands with feature flags (e.g. `cargo test --features foo,bar`)
   - Lint/format with non-default flags
   - Steps with `git diff --exit-code` → **codegen sync checks** (schema, snapshots, help). Record.
   - Docs/site build (e.g. `make site`, `mkdocs build`)
2. **Read Makefile** (if present). Cross-ref CI targets.
3. **Read CLAUDE.md** (repo root or `.claude/`). May define quality gates.

Store commands. Override fallback table.

### 3b. Run the quality pipeline

Detect language from manifests (`Cargo.toml`→Rust, `pyproject.toml`/`setup.py`→Python, `package.json`→Node/TS, `go.mod`→Go). Run checks for each.

Run in order. Use CI-discovered command if found; else fallback.

1. **Build** — compile/bundle
2. **Test** — full suite with CI feature flags. Iterate until green.
3. **Lint/format** — fix issues
4. **Extended checks** — per-language extras (fallback table)
5. **Codegen sync** — for each check, run and verify `git diff --exit-code`. If diff non-empty, regenerate/stage.
6. **Docs build** — if PR changes docs and build cmd exists, verify compile.

### Fallback defaults (when CI config is absent or unclear)

**Rust** (detected by `Cargo.toml`):

| step         | command                                             |
|--------------|-----------------------------------------------------|
| build        | `cargo build`                                       |
| test         | `cargo test`                                        |
| lint         | `cargo clippy -- --deny warnings`                   |
| format       | `cargo fmt --check`                                 |
| supply chain | `cargo deny check` (if `deny.toml` exists)          |
| careful      | `cargo careful test` (if `cargo-careful` installed) |

**Python** (detected by `pyproject.toml` or `setup.py`):

| step         | command                                        |
|--------------|------------------------------------------------|
| test         | `pytest -q`                                    |
| lint         | `ruff check`                                   |
| format       | `ruff format --check`                          |
| types        | `ty check` (or `mypy` if configured)           |
| supply chain | `pip-audit`                                    |

**Node/TypeScript** (detected by `package.json`):

| step         | command                                        |
|--------------|------------------------------------------------|
| build        | per project (`npm run build`, `tsc`, etc.)     |
| test         | `vitest` (or project test script)              |
| lint         | `oxlint` (or project lint script)              |
| format       | `oxfmt --check` (or project format script)     |
| types        | `tsc --noEmit`                                 |
| supply chain | `pnpm audit --audit-level=moderate`            |

**Go** (detected by `go.mod`):

| step         | command                                        |
|--------------|------------------------------------------------|
| build        | `go build ./...`                               |
| test         | `go test ./...`                                |
| lint         | `golangci-lint run`                            |
| format       | `gofmt -l .`                                   |
| vet          | `go vet ./...`                                 |

If tool missing, skip with note.

## 4. Commit and push

- Commit fixes separately (don't squash—preserve history)
- Write commit covering:
  - Subject: `fix: resolve code review findings for PR #$PR_NUMBER`
  - Body: findings by severity, fixed vs dismissed (reasoning), quality pipeline pass
- Push (regular, not force)
- Delete resolved todos in `todos/`

## 5. PR comment

Post a review summary as a PR comment using
`gh pr comment $PR_NUMBER --repo <owner/name>`.

Format the comment body as:

```markdown
## Review Summary

### Findings

[For each severity level that has findings, list them as a table:]

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | P1 | [description] | Fixed: [what was done] |
| 2 | P2 | [description] | Dismissed: [reasoning] |
| ... | ... | ... | ... |

### Verification

- **Tests**: [pass/fail count]
- **Lint**: [clean/issues]
- **Format**: [clean/issues]

### Commit

[commit SHA and subject line]
```
