# Status line and output styles reference

Sources: <https://code.claude.com/docs/en/statusline>,
<https://code.claude.com/docs/en/output-styles> (fetched 2026-05-27).

## Status line

A customizable bar at the bottom showing context %, cost, git status, etc.
Configured in `settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.claude/statusline.sh",
    "padding": 2
  }
}
```

`type: "command"` runs a script that receives session JSON on stdin and prints the
status line. `padding` controls horizontal padding. Disabled by `disableAllHooks`
(which also kills custom status lines). You can also generate one interactively
with `/statusline <description>`.

## Output styles

Output styles change *how* Claude responds (role, tone, format) by editing the
system prompt — not what it knows. Select via setting:

```json
{ "outputStyle": "Explanatory" }
```

Read once at session start / `/clear` (not hot-reloaded). Custom styles are
markdown files in `output-styles/*.md` (project or `~/.claude/`):

```markdown
---
name: Diagrams first
description: Lead every explanation with a diagram
keep-coding-instructions: true
---

When explaining code, start with a Mermaid diagram, then explain in prose.
```

Frontmatter: `name`, `description`, `keep-coding-instructions` (whether to retain
Claude Code's default coding instructions).

## Gaps vs llmenv

- **Both unmodeled.** No YAML vocabulary for `statusLine` or `outputStyle`, and
  `output-styles/*.md` files are not copied/validated/generated.
- A status line is a natural per-host or per-user thing (different prompts/info on
  different machines) — fits llmenv's scope model well. Generating the
  `statusLine` object requires the broader `settings.json` generator first.
- Custom output-style files would byte-copy through `manifest.files` if placed
  under `output-styles/`, but the `outputStyle` *selection* (a settings key) can't
  be set until the settings generator exists.
- Low priority relative to settings/hooks/permissions, but cheap once the
  settings generator lands.
