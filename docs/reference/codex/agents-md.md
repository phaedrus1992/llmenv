# AGENTS.md — project instructions

`AGENTS.md` is Codex's equivalent of `CLAUDE.md`: free-text guidance injected
into the first turn of a session. Codex has **no separate `rules/` directory** —
where Claude Code uses `rules/*.md` with path-glob frontmatter, Codex relies on
layered `AGENTS.md` files discovered by walking the directory tree.

## Discovery and layering

Codex discovers `AGENTS.md` by walking up from the working directory to the
**project root**, reading guidance from each level. Project root detection:

```toml
project_root_markers = [".git", ".hg", ".sl"]  # default is [".git"]
```

`project_root_markers = []` disables parent-directory walking — the cwd is
treated as the root.

Two knobs control how much is read:

- `project_doc_max_bytes` — per-`AGENTS.md` read cap.
- `project_doc_fallback_filenames` — extra filenames to try when `AGENTS.md` is
  missing at a directory level.

There is also a user-global `AGENTS.md` under `CODEX_HOME` (`~/.codex/AGENTS.md`)
layered beneath project files. Untrusted projects skip project-scoped layers.

## Relationship to config

`AGENTS.md` is free text; the only config knobs are the three discovery keys
above plus `model_instructions_file` (a hard override of system instructions).
The `instructions` config key is **reserved** — docs say prefer
`model_instructions_file` or `AGENTS.md`.

## Gaps vs llmenv

llmenv already produces `manifest.agents_md` (rendered to `CLAUDE.md` today) and
`manifest.rules` (rendered to `rules/*.md`). For Codex:

- **`agents_md` maps directly** to `AGENTS.md`. Easy.
- **Rules have no native target.** Codex has no per-file rule mechanism with
  glob frontmatter. A `CodexAdapter` must **fold rules into `AGENTS.md`** — e.g.
  concatenate rule bodies as sections, dropping or commenting the path-glob
  frontmatter (which Codex won't honor). This is a lossy transform: Claude Code's
  conditional, path-scoped rule application becomes unconditional AGENTS.md prose.
- **Multi-level layering is unused.** llmenv resolves a single manifest and would
  write one user-level `AGENTS.md`; Codex's per-directory layering is a feature
  llmenv doesn't exploit (and arguably shouldn't, since llmenv owns the whole
  config).
- Optionally set `project_doc_max_bytes` if generated AGENTS.md is large enough
  to be truncated by Codex's default cap.
