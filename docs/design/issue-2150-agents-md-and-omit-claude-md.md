# Issue #2150 — drop the AGENTS.md shims; handle `omitClaudeMd`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2150
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** chore (repo files), a small adapter change, and docs

This is a spec, not a plan.

## Problem

1. Two files exist only to make Claude Code read `AGENTS.md`:
   - `CLAUDE.md` at the repo root: `# llmenv`, a blank line, `@./AGENTS.md`.
   - `examples/config-llmenv-dir/CLAUDE.md`: `# my-llmenv config`, a blank line, `@./AGENTS.md`.
   Claude Code 2.1.277 and later reads `AGENTS.md` itself when no `CLAUDE.md` is present.
2. Claude Code 2.1.271 added the subagent frontmatter field `omitClaudeMd`.
   llmenv's docs say nothing about it, and the opencode agent translation drops unknown frontmatter keys without the warning that the command translation gives.

## Claude Code facts (memory and sub-agents pages, fetched 2026-09-26)

| Fact | Source |
| --- | --- |
| With an `AGENTS.md` and no `CLAUDE.md`, `.claude/CLAUDE.md` or `CLAUDE.local.md` in the working directory or above, Claude reads `AGENTS.md` | memory page, "AGENTS.md" |
| `~/.claude/CLAUDE.md` does not count for that check; it keeps loading alongside `AGENTS.md` | same |
| Reading `AGENTS.md` directly needs Claude Code 2.1.277 or later | same |
| A `CLAUDE.local.md` stops `AGENTS.md` from loading unless **Project instructions** is set to `claude-md-and-agents-md` | same |
| `omitClaudeMd: true` launches a subagent without the user, project and local `CLAUDE.md` files; managed policy files still load; needs 2.1.271 or later | sub-agents page, frontmatter table |

Under llmenv, the "user `CLAUDE.md`" is llmenv's rendered `$CLAUDE_CONFIG_DIR/CLAUDE.md`, which holds every rule from every active bundle.
A subagent with `omitClaudeMd: true` therefore runs without any llmenv rule.

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| Repo shim | `CLAUDE.md` |
| Example shim; no code or docs reference it by path | `examples/config-llmenv-dir/CLAUDE.md` |
| `translate_agent_md` keeps only `description`, `model`, `tools`, `allowed_tools`, adds `mode: subagent`, and drops everything else silently | `src/adapter/opencode.rs` near line 1683 |
| Command translation prints `warning: opencode adapter does not support '<key>' in command frontmatter — dropping this field` for dropped keys | `src/adapter/opencode.rs` near line 1660 |
| `llmenv setup` touches `~/.claude/CLAUDE.md` (user file), not a project shim | `src/cli/setup.rs` line 37 |

## Changes

### 1. Remove the shims

Delete `CLAUDE.md` and `examples/config-llmenv-dir/CLAUDE.md`.
Do not add a `CLAUDE.local.md`.

Contributors on Claude Code older than 2.1.277 lose the automatic load.
The repo has no `CONTRIBUTING.md` and no contributor section in `README.md`. Add a `## Contributing` section at the end of `README.md` with one sentence: `Agent instructions for this repo are in AGENTS.md; Claude Code 2.1.277 or later reads it directly.`

### 2. opencode: warn on dropped agent keys

In `translate_agent_md`, after building `new_fm`, print one warning per source key that is not kept, in source order:
`warning: opencode adapter does not support '<key>' in agent frontmatter — dropping this field`.
Use the same wording pattern as the command translation.
Do not warn for `name`, which opencode takes from the file name.

This covers `omitClaudeMd` and every other Claude-only field (`effort`, `permissionMode`, `maxTurns`, `isolation`, `background`, …).
Do not translate `omitClaudeMd`; opencode has no equivalent.

### 3. Docs

In `website/docs/engines.md`, in the Claude Code section (the page already compares custom-agent support per engine near line 224), add a short subsection `Skipping CLAUDE.md in a subagent (Claude Code)`, tagged `(added in v3.12.0)`:

- `omitClaudeMd: true` in an agent's frontmatter makes Claude Code start that subagent without the user, project and local `CLAUDE.md` files.
- Under llmenv the user `CLAUDE.md` is the rendered file with every bundle rule, so such a subagent runs without llmenv's rules.
- Use it only for narrow agents that get everything they need from the delegation prompt, such as report-only analyzers.
- Other engines ignore the field; opencode prints a warning and drops it.

## Tests

1. `translate_agent_md` with `omitClaudeMd: true` and `effort: high`: output has no such keys, and the warning text is produced for each (factor the warning list into a returned `Vec<String>` or a testable helper so the test does not capture stderr).
2. Existing `translate_agent_md_*` tests still pass.

## Acceptance criteria

1. The two shim files are gone, and a Claude Code 2.1.277+ session in the repo shows `AGENTS.md loaded` at start.
2. opencode materialize of an agent with `omitClaudeMd` prints the drop warning.
3. Changelog `Changed`: the opencode adapter warns when it drops agent frontmatter fields. The shim removal is repo-only and needs no changelog entry.
4. Docs subsection exists with the version tag.

## Out of scope

- Setting `omitClaudeMd` on any agent in llmenv or in plugins.
- Translating Claude model aliases in agent `model:` fields for opencode.
