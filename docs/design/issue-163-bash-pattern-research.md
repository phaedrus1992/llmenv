# Issue #163 — Research: Bash tool pattern support across LLM CLIs

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/163
- **Milestone:** Small Projects
- **Type:** Research / documentation only — **no code changes**
- **Difficulty:** Easy. Deliverable is one markdown doc plus follow-up issues.

## Problem

llmenv projects permission/allowlist rules for Bash tool invocations into
multiple LLM CLI tools (Claude Code today; Codex, Gemini CLI, Crush, OpenCode
later). Each tool has its own pattern grammar for those rules, and the
grammars are not identical. Examples of known divergence:

- Claude Code accepts both `command foo *` (space-separated) and
  `command foo:*` (colon-separated) forms.
- Pipe sequences (`cmd1 | cmd2`) appear to be honored in Claude permission
  rules; other tools' behavior is unknown.

llmenv needs a documented comparison so it can decide whether to normalize,
translate per-target, or warn on incompatible rules.

## Deliverable

One new doc: `docs/reference/bash-permission-patterns.md` containing:

1. A per-tool section for each of: **Claude Code**, **Codex**, **Gemini CLI**,
   **Crush**, **OpenCode**. Each section documents, with cited sources
   (official docs URL or source-code link):
   - Pattern form(s) accepted (exact syntax).
   - Wildcard semantics (`*` — prefix match? glob? token match?).
   - Separator style (`foo *` vs `foo:*`) and whether both work.
   - Pipe handling: given rule `cmd1:*`, does `cmd1 | cmd2` match? Is a rule
     containing a pipe (`cmd1 | cmd2`) accepted?
   - Quoting/escaping of special characters.
   - Edge cases (subcommands, flags, `&&`/`;` chains).
2. A summary comparison table (rows = features above, columns = tools).
3. A **Recommendation** section choosing one of:
   - normalize to a canonical llmenv form and translate per-target on
     projection,
   - pass through verbatim and warn on constructs a target can't express,
   - hybrid.
   Justify the choice in a short paragraph.
4. A **Follow-up work** section listing the implementation tasks the
   recommendation implies. File a GitHub issue for each (label
   `enhancement`, milestone left unset) and link them from the doc.

## How to research

1. Read llmenv's existing engine survey first: `docs/engine-capabilities.md`
   (it already catalogs engine config surfaces — match its tone/structure).
2. Check how llmenv currently emits permissions: search the adapter code
   (`rg -i 'permission' src/adapter/`) and note the exact key names and
   pattern strings written for Claude Code. The doc must describe what llmenv
   emits today, not just what tools accept.
3. For each tool, consult official docs (web search: e.g. "Claude Code
   settings permissions allow Bash", "OpenCode permission bash pattern",
   "Codex config approval policy", "Gemini CLI tool allowlist"). Where docs
   are ambiguous, check the tool's public source repo. Cite every claim.
4. Where behavior cannot be confirmed from docs or source, say so explicitly
   in the doc ("unverified") rather than guessing.

## Acceptance criteria

- [ ] `docs/reference/bash-permission-patterns.md` exists with all four parts
      above; every factual claim carries a source link or an "unverified"
      marker.
- [ ] Comparison table covers at minimum: separator style, wildcard
      semantics, pipe-in-command matching, pipe-in-rule acceptance, escaping.
- [ ] One recommendation is chosen (not a menu of options).
- [ ] Follow-up GitHub issues filed and cross-linked.
- [ ] No source code modified. No CHANGELOG entry needed (docs-only,
      non-user-facing).

## Out of scope

- Implementing normalization/translation (that's the follow-up issues).
- Non-Bash tool permissions (file read/write, network, MCP allowlists).
