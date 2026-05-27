# Skills and commands reference

Source: <https://code.claude.com/docs/en/skills> (fetched 2026-05-27).

Skills extend what Claude can do. A skill is a directory with a `SKILL.md`. Custom
commands (`commands/*.md`) are single-file skills using the same mechanism.

## SKILL.md format

```yaml
---
name: summarize-changes
description: Summarizes uncommitted changes and flags risk. Use when the user asks what changed or wants a commit message.
when_to_use: trigger phrases / example requests (appended to description)
allowed-tools: Read Grep
---

## Instructions
...markdown body Claude follows when the skill runs...

## Dynamic content
!`git diff HEAD`
```

Frontmatter fields (all optional; `description` recommended):

| Field | Notes |
| --- | --- |
| `name` | Display name; defaults to directory name. (The *command* you type is derived separately — see below.) |
| `description` | What it does + when to use. Drives auto-invocation. If omitted, first markdown paragraph is used. |
| `when_to_use` | Extra trigger context, appended to `description`. |
| `allowed-tools` | Space- or comma-separated tool restriction. |

`description` + `when_to_use` combined are truncated at **1,536 chars** in the
skill listing (`maxSkillDescriptionChars` setting). The listing budget is
`skillListingBudgetFraction` (default 1% of context).

The body supports inline shell with `` !`cmd` `` and ` ```! ` blocks (disabled by
`disableSkillShellExecution`).

## Where skills live

| Location | Path | Scope |
| --- | --- | --- |
| Enterprise | managed settings dir | All org users |
| Personal | `~/.claude/skills/<name>/SKILL.md` | All your projects |
| Project | `.claude/skills/<name>/SKILL.md` | This project |
| Plugin | `<plugin>/skills/<name>/SKILL.md` | Where plugin enabled |

Precedence: enterprise > personal > project. Plugin skills are namespaced
`plugin-name:skill-name` (no conflicts). If a skill and a `commands/*.md` command
share a name, the **skill wins**.

`skillOverrides` (setting) sets per-skill visibility:
`on`/`name-only`/`user-invocable-only`/`off`.

## Commands (`commands/*.md`)

Single-file prompts in `commands/`, same loading mechanism as skills. Invoked
`/name`. Frontmatter supports `description`, `argument-hint`, `model`,
`allowed-tools` (the commands and skills systems were unified).

## Gaps vs llmenv

- llmenv **validates** skills but does not **generate** them. `validate_skills`
  (`src/adapter/claude_code.rs:115`) checks each `skills/*/` has a `SKILL.md` with
  `name` + `description` frontmatter, then errors otherwise. Skills arrive only as
  copied bundle files.
- Validation is shallow vs the real schema: it requires `name` (the docs make
  `name` optional and `description` the recommended one), ignores `when_to_use`,
  `allowed-tools`, the 1,536-char cap, and the frontmatter-parsing edge case where
  a file `---\n...\n---` with no trailing newline is handled by a special case.
  Consider aligning validation with the documented field set.
- **Commands (`commands/*.md`)** are entirely unmodeled — no copy path, no
  validation, no generation. If bundles want to ship slash commands, that's a
  gap. (They'd byte-copy fine through `manifest.files` today, but nothing
  documents or validates them.)
- No skill **selection** semantics: skills are whatever files a bundle happens to
  contain. There's no tag-gated "this skill only on rust projects" mechanism
  distinct from which bundle is selected. That may be fine (bundle selection *is*
  the gate), but worth stating explicitly in a design doc.
