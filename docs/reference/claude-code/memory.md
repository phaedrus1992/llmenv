# Memory / CLAUDE.md / rules reference

Source: <https://code.claude.com/docs/en/memory> (fetched 2026-05-27).

Two mechanisms carry knowledge across sessions: **CLAUDE.md** (you write) and
**auto memory** (Claude writes).

## CLAUDE.md vs auto memory

| | CLAUDE.md | Auto memory |
| --- | --- | --- |
| Who writes | You | Claude |
| Contains | Instructions and rules | Learnings and patterns |
| Scope | Project, user, or org | Per repository, shared across worktrees |
| Loaded | Every session | Every session (first 200 lines or 25KB) |

## CLAUDE.md locations

| Scope | Path |
| --- | --- |
| User | `~/.claude/CLAUDE.md` |
| Project | `CLAUDE.md` or `.claude/CLAUDE.md` |
| Local | `CLAUDE.local.md` |
| Org (managed) | `claudeMd` setting |

Related settings: `claudeMdExcludes` (glob/abs paths to skip),
`autoMemoryEnabled`, `autoMemoryDirectory`, `includeGitInstructions`,
`maxSkillDescriptionChars`/`skillListingBudgetFraction` (skill listing, not
memory).

`@path` imports pull other files in (the user's global CLAUDE.md uses
`@RTK.md`). Files referenced this way are inlined.

## Rules (`rules/*.md`)

Topic-scoped instructions, optionally path-gated via frontmatter, in
`~/.claude/rules/` or `.claude/rules/`. Claude Code has a **native rules-directory
convention** — files are loaded as instructions, frontmatter preserved.

## Auto memory

Claude maintains notes based on your corrections/preferences in
`autoMemoryDirectory` (per-repo, worktree-shared). Loads first 200 lines / 25KB
each session. Toggle with `/memory` or `autoMemoryEnabled`.

## Gaps vs llmenv (parity on CLAUDE.md + rules)

This is the **best-supported** surface:

- `CLAUDE.md` is generated from `manifest.agents_md` and written at the config
  root (`src/adapter/claude_code.rs:37`).
- `rules/*.md` are written verbatim with frontmatter preserved
  (`src/adapter/claude_code.rs:43`), correctly using Claude Code's native rules
  convention. The adapter comment even notes that adapters lacking this
  convention should inline via `merge::agents_md::concat_with_rules`.

Narrow gaps / open questions:

1. **Auto memory** is unmodeled. Given llmenv already has a first-class `memory`
   backend (ICM/MCP), Claude Code's *native* auto memory is a separate system.
   Worth a design note: do they coexist, or does llmenv intend ICM to replace
   native auto memory? `autoMemoryEnabled: false` could be generated to avoid two
   memory systems fighting.
2. **`claudeMdExcludes` / `@`-imports** are not generated or validated. If a
   bundle's CLAUDE.md uses `@imports`, the referenced files must also be
   materialized — verify the merge layer handles transitive imports.
3. **`CLAUDE.local.md`** (personal, gitignored) has no equivalent; llmenv writes a
   single merged CLAUDE.md, which is probably correct for its model.
