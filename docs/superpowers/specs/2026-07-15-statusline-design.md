<!-- markdownlint-disable MD013 -->
# Statusline — Design

## Problem

llmenv has no statusline integration. Users who want a statusline showing Claude
Code session state (model, context usage, budget, rate limits) must install and
configure a separate binary (rusty-claude-status) that reads a separate config
file (`~/.claude/statusline.json`) and a separate stdin JSON protocol. The
statusline cannot display llmenv-specific data like active scopes, plugin
counts, ICM stats, or cache health — the data lives in llmenv but the renderer
has no access to it.

A first-class statusline built into llmenv solves the split-brain: one config
(`config.yaml`), one binary (`llmenv statusline`), all data sources (engine
session JSON + llmenv internal stats + user widget layout) available to the
renderer.

## Design

Three layers, all inside llmenv core:

| Layer | What | Where |
|-------|------|-------|
| **Config** | Widget layout, formatting, colours | `config.yaml` `statusline:` section |
| **Data** | llmenv stats for this context | `<materialized_dir>/llmenv-status.json` |
| **Renderer** | Stdin + data + config → ANSI output | `llmenv statusline` subcommand |

### Data flow

1. Engine (Claude Code, Crush, etc.) spawns `llmenv statusline`, pipes session
   JSON to stdin — same shape as rusty-claude-status currently expects:
   `{workspace, model, cost, context_window, rate_limits}`
2. llmenv reads the JSON from stdin and parses it (`engine-sourced` fields)
3. llmenv reads `<materialized_dir>/llmenv-status.json` (`llmenv-sourced` fields)
4. llmenv reads widget config from the materialized `settings.json` (resolved
   `statusline:` section from the user's `config.yaml`)
5. llmenv merges engine data + llmenv data + config → renders ANSI rows to
   stdout, one line per configured row
6. Engine captures stdout and displays it in its status bar

The engine contract (spawns binary, pipes stdin, reads stdout, renders ANSI) is
adapter-specific. Each adapter that supports a statusline hook wires it the same
way: the adapter emits the hook configuration pointing at `llmenv statusline`.

### Data file: `llmenv-status.json`

Written to the **materialized folder** (`<adapter_root>/<version>/<hash>/`),
alongside `.llmenv-manifest.json` and the generated config files. The materialized
folder is already per-context (different config hash = different folder), so the
status data is correctly scoped to the current session.

**Write triggers** (cheap, in-memory recompute — no extra I/O):

1. **Materialization** — final step after writing config files and running hooks.
   llmenv has all the data in memory: scopes, tags, plugins, MCPs, ICM counts.
2. **`llmenv export`** — refreshes throttle state, cache usage, stale config
   detection.
3. **Session start** — written once before the engine launches, ensures the file
   exists for the first statusline render call.

```json
{
  "$schema": "llmenv-status-v1",
  "v": 1,
  "ts": "2026-07-15T14:23:00Z",

  "scopes": {
    "tags": ["dev", "rust", "llm"]
  },

  "plugins": {
    "total": 12,
    "errors": 0
  },

  "mcps": {
    "total": 12,
    "errors": 0
  },

  "icm": {
    "memories": 142,
    "concepts": 47
  },

  "throttle": null,

  "config_stale": false,

  "cache": {
    "prunable_bytes": 15728640
  },

  "session_log": 8
}
```

All fields are optional. The `ts` field is informational (staleness diagnostic).
The renderer never depends on the file existing — missing file = all llmenv
widgets render empty.

### Config: `config.yaml` `statusline:` section

```yaml
statusline:
  # Row templates — one per status line. Use {widget_name} placeholders.
  rows:
    - "{model} │ {context_pct} │ {budget}"
    - "{scopes:t} · {plugins} {config_stale}"

  style:
    separator: " │ "
    icon_set: auto       # auto | nerd | simple | none

  # Widget definitions — only needed when overriding defaults
  widgets:
    model:
      format: "{short_name} {version}"
      style: "bold cyan"
    context_pct:
      style: "yellow"
    scopes:
      format: "║ {tags}"
      max_len: 40
      style: "dim"
    plugins:
      format: "◇ {total}"
      style: "dim white"
    config_stale:
      format: "{stale_icon}"
      max_len: 1
    icm:
      format: "M{memories}"
      style: "dim white"
    cache:
      format: "{prunable}"
      style: "dim white"
    throttle:
      format: "{raw}"
      style: "bold yellow"

  # Icons for compact status indicators
  icons:
    config_ok: ""
    config_stale: "◌"
    icm_ok: ""
    throttle: "⚠"
    plugin_ok: ""
    plugin_error: "!"
    cache_ok: ""
    cache_prunable: "📦"
    session_log: "📝"
```

#### Row templates

Each item in `rows` is a string template with `{widget_name}` placeholders.
Widget names in the template are resolved against the `widgets:` map or the
default widget definitions. Unknown widget names render empty.

The shorthand `{scopes:t}` = apply the widget's default truncation (`max_len`).
This is redundant with `max_len` on the widget definition but provides an
inline override for cases where the same widget appears in multiple rows with
different truncation.

If `statusline:` is absent from config, a default single row is rendered:
`"{model} │ {folder} │ {branch} │ {context_pct} │ {budget}"`

#### Per-widget `format`

Controls what field(s) the widget displays from its data source. Available
fields depend on widget type (see widget table below). `{field}` is replaced
with the field value. If a referenced field is missing/unavailable, the
placeholder renders empty.

#### `max_len`

Maximum character length for the widget's rendered output. Longer values are
truncated with `…` (U+2026). Default: no limit.

#### `style`

ANSI style string: space-separated tokens from: `bold`, `dim`, `italic`,
`underline`, `blink`, `reverse`, `hidden`, `strikethrough`, and any named
colour (16-colour palette: `black`, `red`, `green`, `yellow`, `blue`,
`magenta`, `cyan`, `white`). 256-colour and true-colour tokens are also
supported: `#rrggbb` or `color-<n>`.

The style is applied to the entire widget's output, not individual characters
within it.

#### `icon_set`

Controls how icons are selected:
- `auto` — detect terminal font: Nerd Font icons when a Nerd Font is active,
  fall back to simple ASCII glyphs
- `nerd` — always use Nerd Font glyphs
- `simple` — always use ASCII/Unicode glyphs from the `icons:` config
- `none` — skip all icons, show bare values

### Widget rendering table

#### Engine-sourced (from stdin)

| Widget | Default format | Example output | Available fields |
|--------|---------------|----------------|-----------------|
| `model` | `{short_name} {version}` | `Claude Opus 4.8` | `short_name`, `version`, `full_name` |
| `folder` | `{basename}` | `llmenv` | `basename`, `path` |
| `branch` | `{name}` | `release/3.x` | `name` |
| `pr` | `#{number}` | `#834` | `number`, `title` |
| `progress_bar` | `{pct}% {bar}` | `35% ███░░░░░░░` | `pct`, `bar` |
| `tokens` | `{total}` | `10.0k` | `total`, `input`, `output`, `cache_read`, `cache_create` |
| `context_pct` | `{pct}%` | `35%` | `pct` |
| `budget` | `{used}/{max}` | `35k/200k` | `used`, `max` |
| `duration` | `{h}h{m}m` | `3h42m` | `h`, `m`, `s`, `total_ms` |
| `cache_pct` | `{pct}%` | `44%` | `pct` |

#### llmenv-sourced (from data file)

| Widget | Default format | Example output | Available fields |
|--------|---------------|----------------|-----------------|
| `scopes` | `║ {tags}` | `║ dev · rust` | `tags`, `count` |
| `plugins` | `◇ {total}` | `◇ 12` | `total`, `errors`, `error_icon` |
| `mcps` | `MCP {total}` | `MCP 12` | `total`, `errors` |
| `icm` | `M{memories}` | `M142` | `memories`, `concepts`, `memoirs`, `ready_icon` |
| `cache` | `{prunable}` | `15 MB` | `prunable`, `prunable_raw` |
| `config_stale` | `{stale_icon}` | `◌` | `stale_icon`, `fresh_icon` |
| `throttle` | `{raw}` | `⚠ 45s` | `raw`, `cooldown_secs`, `reason`, `icon` |
| `session_log` | `{icon} {entries}` | `📝 8` | `entries`, `icon` |

When a widget type has no explicit format in config, the **default format**
above is used.

### Engine vs llmenv widget naming

Widget names starting with `engine_` are reserved for future
engine-sourced data. Currently engine widgets use short names (`model`,
`folder`, etc.) — no prefix needed since there's no collision with llmenv
widgets. If a collision arises, the llmenv-sourced field takes the bare name
and the engine-sourced one moves to `engine_<name>`. (No current collisions.)

### Stdin format

The engine pipes session JSON on stdin. Contract:

```jsonc
{
  "workspace": { "current_dir": "/home/user/project" },
  "model": { "display_name": "Claude Opus 4.8" },
  "cost": { "total_duration_ms": 123456 },
  "context_window": {
    "remaining_percentage": 65.0,
    "context_window_size": 200000,
    "current_usage": {
      "input_tokens": 5000,
      "cache_creation_input_tokens": 1000,
      "cache_read_input_tokens": 4000
    }
  },
  "rate_limits": {
    "five_hour": { "used_percentage": 24.5, "resets_at": 1713264000 },
    "seven_day": { "used_percentage": 41.0, "resets_at": 1713700000 }
  }
}
```

All fields optional. Missing/parse-error defaults to empty for each widget.

### Renderer contract

- **Exit 0** on successful render (even if some widgets are empty).
- **Exit non-zero** on internal error (can't read config, I/O error on data
  file that's syntactically broken). Outputs nothing to stdout in this case.
- **Missing data file** is not an error — llmenv widgets render empty, exit 0.
- **No orphaned separators**: if all widgets in a row render empty, the row
  outputs an empty line (or nothing, per adapter contract).
- **Max width**: each widget is truncated to `max_len` if set, appended with `…`.
- **ANSI isolation**: each row is self-contained, terminated with `\n`.

### Implementation outline

New files:

```
src/cli/statusline.rs         — Subcommand entry point
src/cli/statusline/widget.rs  — Widget type definitions + rendering
src/cli/statusline/config.rs  — Config parsing (statusline section)
src/cli/statusline/data.rs    — Data file read + merge logic
```

Changes to existing files:

- `src/cli/mod.rs` — Register `llmenv statusline` subcommand
- `crates/llmenv-config/src/lib.rs` — Add `StatuslineConfig` to the config model
- `src/materialize/mod.rs` — Write `llmenv-status.json` during materialization
- `src/engine/mod.rs` (or adapter) — Wire statusline hook per-adapter

No new crate. All code lives in the existing `llmenv` crate. The config model
lives in `llmenv-config`.

### Separation of concerns

The renderer has no logic beyond "read files → merge → render":

- **`data.rs`**: deserialises `llmenv-status.json`. Pure parsing — no business
  logic (no scope resolution, plugin discovery, ICM queries). Those happen at
  data-file write time.
- **`config.rs`**: deserialises the `statusline:` config section. Pure parsing.
- **`widget.rs`**: stateless render functions: `render_model(data, cfg) → String`,
  `render_scopes(data, cfg) → String`, etc. Each receives complete input and
  returns a string. No side effects, no shared mutable state.
- **`statusline.rs`**: orchestrator — reads stdin, reads config, reads data file,
  calls widget renders in template order, assembles rows, writes to stdout.

### Error handling

- Config parse failure → use default config (single row, defaults)
- Data file parse failure → all llmenv widgets render empty
- Stdin parse failure → all engine widgets render empty
- I/O error on data file → same as parse failure (all llmenv widgets empty)
- Unknown widget name in template → renders empty string
- Missing `statusline:` in config → use default config

Goal: the statusline never fails to render. Distinguish "no data" (exit 0,
empty output) from "can't run" (exit non-zero, no output) so the engine
doesn't show stale/partial ANSI.

### Testing

- **Unit tests** per widget type: known inputs → expected ANSI output
- **Unit tests** for template parsing: `"{model} │ {context_pct}"` → field list
- **Unit tests** for data file merging: empty, partial, full JSON
- **Unit tests** for config parsing: YAML `statusline:` → struct, defaults
- **Integration test**: pipe known stdin JSON + data file + config → ANSI output
- **Integration test**: missing data file → engine fields only render
- **Integration test**: empty config → default single-row output
- **Integration test**: all widgets empty → empty/no-op stdout
