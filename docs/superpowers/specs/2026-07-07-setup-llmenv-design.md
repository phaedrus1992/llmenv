# Setup llmenv — Design

## Problem

New llmenv users have no guided path from "installed llmenv" to "working configuration
that reflects their existing tooling, preferences, and project needs." The existing
`llmenv init` creates a bare template; everything else is manual.

## Design

Two layers: a CLI command for the mechanical bootstrap, and a skill (embedded in the
binary) that an AI agent runs for the evaluative work — scanning, sorting, migrating
existing configs.

### Layer 1: `llmenv setup` (CLI command)

The mechanical, non-AI part. Runs first.

**What it does:**

1. **Config dir** — Creates `~/.config/llmenv/` (or specified path) with standard structure
2. **config.yaml** — Writes initial config with a `base` bundle + user identity tag. Reuses
   `generate_template()` as the base, same as `run_init`
3. **AGENTS.md** — Writes a starter orientation guide
4. **GitHub repo** — Prompts: existing repo URL, help-me-create instructions, or skip
5. **Scan existing configs** — Reads contents of `~/.claude/settings.json`,
   `~/.claude/plugins.json`, `~/.claude/projects/*/settings.json`, and
   `~/.cursor/settings.json` at the content level (not just existence check)
6. **Write enumeration JSON** — Dumps found configs + contents into
   `.llmenv-setup-state.json` in the config dir, structured for AI consumption
7. **Install the skill** — Copies the embedded setup skill into
   `bundles/base/skills/setup-llmenv/SKILL.md` so it's available by name
8. **Engine handoff** — Detects available engines (`claude`, `crush`) on PATH,
   asks "Launch setup skill with [engine]?", pipes the skill + enumeration to the engine
   via its stdin prompt (`claude -p` / `crush run`)

**Flags:**
- `--path` — custom config dir (same as `init`)
- `--repo` — pre-set repo URL (non-interactive)
- `--no-launch` — skip the engine handoff prompt (for scripting / CI)

### Layer 2: The enumeration JSON

Written to `{config_dir}/.llmenv-setup-state.json`. Format:

```json
{
  "version": 1,
  "user": "you",
  "config_dir": "/Users/you/.config/llmenv",
  "engines_available": ["claude", "crush"],
  "existing_configs": {
    "claude_code": {
      "settings": { ... full key-value content of settings.json ... },
      "plugins": [ ... plugin refs from plugins.json (name, marketplace) ... ],
      "marketplaces": [ ... marketplace sources from settings.json/plugins.json ... ],
      "claude_md": "raw text content or null",
      "gemini_md": "raw text content or null",
      "projects": {
        "project-name": { "settings": { ... } }
      }
    },
    "cursor": {
      "settings": { ... }
    }
  },
  "created_bundles": ["base"]
}
```

This is the skill's sole data dependency. No filesystem scraping in the skill layer.

### Layer 3: The setup skill (embedded in binary)

**Where it lives:** Embedded as a string constant via `include_str!()` in the Rust
binary, written to the user's bundle skills dir at setup time. Ships with llmenv —
no repo checkout needed, version sync guaranteed.

**Skill flow:**

1. **Context** — Load `.llmenv-setup-state.json`, greet the user, show what was found
2. **Claude Code settings** — Walk through discovered settings keys: "This permission
   rule was in your ~/.claude — keep in the `base` bundle or create a separate one?"
   - Plugin refs (with marketplace sources) → suggest migrating to llmenv `plugin-collection` entries; detect if the marketplace is already available (e.g. `dev-commons`, `claude-plugins-official`) and suggest the right `marketplace:` declaration
   - Marketplace registrations → map to llmenv `marketplace:` entries in config.yaml
   - MCP servers → suggest migrating to llmenv `mcp:` entries
   - Hooks → check for llmenv equivalents, import if unique
   - Custom instructions (CLAUDE.md/AGENTS.md content) → merge into bundle instructions
3. **Project configs** — For each `~/.claude/projects/*/settings.json`:
   - "Found per-project overrides for $PROJECT. These are very project-specific —
     keep them as native passthrough or drop them?"
4. **Sort into bundles** — Interactive questions to bucket config:
   - Work vs personal division
   - Language-specific tooling (Rust, Python, JS, etc.)
   - Common platforms (AWS, GCP, etc.)
   - Creates new bundle directories + `bundle.yaml` `when:` declarations
5. **Scopes** — Prompt for scope setup: network (SSID), host (hostname), user
6. **Validate** — Write all bundle fragments, run `llmenv regenerate`, report results
7. **Wrap-up** — Summary of what was created, pointer to docs

**Skill format:** Standard llmenv skill markdown with `##` sections reflecting each phase.
Lives as `skills/setup-llmenv/SKILL.md` at bundle install location.

### Layer 4: Engine handoff

At the end of `llmenv setup`:

1. Probe PATH for engines (`which claude`, `which crush`, detected by the same adapter
   probe logic llmenv already uses)
2. Present a Select prompt: "Launch setup skill with [claude/crush/skip]"
3. On selection: pipe the skill markdown + path to `.llmenv-setup-state.json` to the
   engine's stdin prompt:
   - Claude Code: `claude -p "$(cat skill.md | sed 's/{STATE_PATH}/\/path\/to\/state.json/g')"`
   - Crush: `crush run -p "$(cat skill.md | sed 's/{STATE_PATH}/\/path\/to\/state.json/g')"`

   The skill text has `{STATE_PATH}` as a placeholder that the CLI replaces with the
   absolute path to `.llmenv-setup-state.json` before piping. The skill itself tells the
   agent where to find its data.
4. The engine runs the skill interactively in the user's terminal
5. On completion, the user exits back to their shell with their config fully set up

### Backward compatibility

- `llmenv init` unchanged — remains the minimal, non-interactive bootstrap
- The existing `llmenv setup` output (interactive prompts) is preserved as the mechanical
  part; the engine handoff is appended
- No existing configs are modified during the scan phase — only read

### Files changed

| File | Change |
|------|--------|
| `src/cli/setup.rs` | Add enumeration scan, JSON write, engine detection, handoff prompt, skill embedding |
| `src/cli/mod.rs` | Add `--no-launch` flag to Setup variant |
| `skills/setup-llmenv/SKILL.md` | New — the skill content (embedded via include_str!) |
| (the skill is in the repo for dev/CI; the binary embeds it at compile time) |

### Future

- **Codex adapter handoff** — when Codex supports a `-p`-like stdin prompt pattern, add it
- **Gemini CLI handoff** — same when supported
- **Automatic re-scan** — `llmenv setup --rescan` re-runs the scan + enumeration without
  recreating the config dir, for existing users who want the skill to re-evaluate

## Testing

The `--no-launch` path is the primary test surface — the AI handoff (claude -p / crush run)
is tested at integration level only.

### Smoke tests (`--no-launch`)

| Scenario | What it verifies |
|----------|------------------|
| Fresh setup to temp dir | Config dir created, config.yaml valid, AGENTS.md written |
| Setup with `--repo` | Marketplace entry written, overwrite prompt not skipped |
| Re-run on existing config | Overwrite prompt fires, safe paths on "keep" |
| Claude Code settings exist | Enumeration JSON includes claude_code.settings with correct keys |
| Claude Code projects exist | Enumeration JSON includes project entries |
| Cursor settings exist | Enumeration JSON includes cursor.settings |
| No existing configs | Enumeration JSON has empty existing_configs |
| Setup with bad bundle names | `is_unsafe_join_target` rejected, user re-prompted |
| Enumeration JSON format | JSON is valid, has version field, has all expected sections |
| Engine probing | `engines_available` list matches which engines are on PATH |

### Integration tests

| Scenario | What it verifies |
|----------|------------------|
| Full `--no-launch` run | The CLI completes without error, exits 0 |
| Skill file installed | `bundles/base/skills/setup-llmenv/SKILL.md` exists and is valid markdown |
| Config round-trips | `llmenv regenerate` succeeds on the generated config |
| Engine handoff (smoke) | `claude -p` or `crush run` would be invoked with the right args (dry-run flag) |

### Test implementation

- All smoke tests run in tempdir (`tempfile::TempDir`), no ~/.config contamination
- Claude Code / Cursor settings are mocked as temporary files in the expected locations
- Tests use `--no-launch` to skip the interactive handoff
- Engine probing is tested separately (function that returns `Vec<String>` of found engines)
