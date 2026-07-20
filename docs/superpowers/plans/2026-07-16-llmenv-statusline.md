# llmenv statusline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a first-class `llmenv statusline` subcommand that reads engine
session JSON from stdin, llmenv stats from a materialized data file, and
widget layout from `config.yaml`, then renders ANSI status-bar rows —
replacing the external `rusty-claude-status` binary. Closes #836.

**Architecture:** Three independent layers per
`docs/superpowers/specs/2026-07-15-statusline-design.md` (authoritative —
read it before starting): a config schema (`statusline:` section), a data
file (`llmenv-status.json`, written at materialize/export/session-start
time), and a stateless renderer (`llmenv statusline` subcommand) that merges
stdin + data file + config into ANSI rows. The renderer has zero business
logic (no scope resolution, no MCP calls) — all of that happens once, at
data-file write time, in `src/materialize/status_data.rs`.

**Tech Stack:** Rust, `serde`/`serde_json`/`serde_yaml` (already deps), no
new ANSI crate (hand-rolled per `src/cli/style.rs` convention — this repo has
no `nu-ansi-term`/`crossterm`/`owo-colors` dependency and none should be
added: 16-colour + `#rrggbb` + `color-N` tokens are a few match arms, not
worth a dependency).

## Global Constraints

- No new crate. All new code lives in the existing `llmenv` binary crate
  (`src/`) except the config struct, which lives in `llmenv-config`
  (`crates/llmenv-config/src/schema.rs`), matching every other config
  section.
- Every renderer error path must degrade to empty output, never panic or
  crash the engine's statusbar (see "Renderer contract" in the design doc).
- Test convention: inline `#[cfg(test)] mod tests` at the bottom of each file
  (this repo does not use separate `tests/*.rs` files for unit tests).
- `unwrap`/`expect`/panicking are workspace-denied lints outside tests — every
  fallible path in non-test code returns `Result`/`Option` and degrades per
  the error-handling table in the design doc.
- Run `cargo fmt` after every file edit before staging (project convention).
- Property tests use `proptest!` blocks inside the same `mod tests`, following
  the shape at `src/adapter/claude_code.rs:2594` (`arb_*` strategy fn + a
  `proptest! { fn foo(x in arb_x()) { ... } }` block) — used here for the
  ANSI-safe truncation helper (Task 4) and template parser (Task 3).

---

### Task 1: Config schema — `StatuslineConfig`

**Files:**

- Modify: `crates/llmenv-config/src/schema.rs` (add near `ContextMode`,
  `schema.rs:813-817`)
- Modify: `crates/llmenv-config/src/lib.rs:26-37` (re-export)
- Test: same file, inline `#[cfg(test)] mod tests`

**Interfaces:**

- Consumes: nothing (leaf config struct).
- Produces: `pub struct StatuslineConfig` with fields `rows: Vec<String>`,
  `style: StatuslineStyle`, `widgets: BTreeMap<String, WidgetConfig>`,
  `icons: BTreeMap<String, String>`. Consumed by Task 2 (data merge) is N/A —
  consumed directly by Task 8 (orchestrator) and Task 5/6 (widget render,
  which reads `WidgetConfig`). `Config` (the top-level struct, wherever it's
  defined — grep `pub struct Config` in `schema.rs`) gets a new field
  `pub statusline: Option<StatuslineConfig>`.

- [ ] **Step 1: Write the failing test**

Add to `crates/llmenv-config/src/schema.rs`, bottom of the file's existing
`#[cfg(test)] mod tests` block:

```rust
#[test]
fn statusline_config_parses_full_example() {
    let yaml = r#"
rows:
  - "{model} │ {context_pct} │ {budget}"
  - "{scopes:t} · {plugins} {config_stale}"
style:
  separator: " │ "
  icon_set: auto
widgets:
  model:
    format: "{short_name} {version}"
    style: "bold cyan"
  scopes:
    format: "║ {tags}"
    max_len: 40
    style: "dim"
icons:
  config_ok: ""
  config_stale: "◌"
"#;
    let cfg: StatuslineConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.rows.len(), 2);
    assert_eq!(cfg.style.separator, " │ ");
    assert_eq!(cfg.style.icon_set, IconSet::Auto);
    let model = cfg.widgets.get("model").unwrap();
    assert_eq!(model.format.as_deref(), Some("{short_name} {version}"));
    assert_eq!(model.style.as_deref(), Some("bold cyan"));
    let scopes = cfg.widgets.get("scopes").unwrap();
    assert_eq!(scopes.max_len, Some(40));
    assert_eq!(cfg.icons.get("config_stale").map(String::as_str), Some("◌"));
}

#[test]
fn statusline_config_defaults_on_empty_yaml() {
    let cfg: StatuslineConfig = serde_yaml::from_str("{}").unwrap();
    assert!(cfg.rows.is_empty());
    assert_eq!(cfg.style.separator, " │ ");
    assert_eq!(cfg.style.icon_set, IconSet::Auto);
    assert!(cfg.widgets.is_empty());
    assert!(cfg.icons.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv-config statusline_config -- --nocapture`
Expected: FAIL with "cannot find type `StatuslineConfig` in this scope" (or
similar — the type doesn't exist yet).

- [ ] **Step 3: Write minimal implementation**

Add to `crates/llmenv-config/src/schema.rs`, near `ContextMode`:

```rust
/// Widget layout, formatting, and colour config for `llmenv statusline`
/// (`statusline:` section of `config.yaml`). See
/// `docs/superpowers/specs/2026-07-15-statusline-design.md`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StatuslineConfig {
    /// One row template per rendered status line. `{widget_name}`
    /// placeholders are resolved against `widgets` or widget defaults.
    pub rows: Vec<String>,
    pub style: StatuslineStyle,
    /// Per-widget overrides. Keyed by widget name (`model`, `scopes`, ...).
    pub widgets: std::collections::BTreeMap<String, WidgetConfig>,
    /// Named icon overrides (`config_stale`, `throttle`, ...).
    pub icons: std::collections::BTreeMap<String, String>,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            style: StatuslineStyle::default(),
            widgets: std::collections::BTreeMap::new(),
            icons: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StatuslineStyle {
    pub separator: String,
    pub icon_set: IconSet,
}

impl Default for StatuslineStyle {
    fn default() -> Self {
        Self {
            separator: " │ ".to_string(),
            icon_set: IconSet::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconSet {
    #[default]
    Auto,
    Nerd,
    Simple,
    None,
}

/// Per-widget override: display format, truncation, and style.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WidgetConfig {
    pub format: Option<String>,
    pub max_len: Option<usize>,
    pub style: Option<String>,
}
```

Then in `crates/llmenv-config/src/lib.rs:26-37`, add `StatuslineConfig`,
`StatuslineStyle`, `IconSet`, `WidgetConfig` to the existing
`pub use schema::{...}` list (individually — no glob re-exports, per Rust
convention). Add `pub statusline: Option<StatuslineConfig>` to the top-level
`Config` struct (locate it with `rg -n "pub struct Config" schema.rs`) next
to the other `Option<Feature>`-shaped fields.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p llmenv-config statusline_config -- --nocapture`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/llmenv-config/src/schema.rs crates/llmenv-config/src/lib.rs
git commit -m "feat(config): add statusline config schema

Fixes #836"
```

---

### Task 2: Data file schema — `StatusData`

**Files:**

- Create: `src/cli/statusline/data.rs`
- Modify: `src/cli/statusline/mod.rs` (create, `mod data;`)
- Test: inline in `data.rs`

**Interfaces:**

- Consumes: nothing (pure deserialization type).
- Produces: `pub struct StatusData` with all-optional fields matching the
  design doc's JSON shape exactly:
  `scopes: Option<ScopesData>`, `plugins: Option<CountData>`,
  `mcps: Option<CountData>`, `icm: Option<IcmData>`,
  `throttle: Option<ThrottleData>`, `config_stale: Option<bool>`,
  `cache: Option<CacheData>`, `session_log: Option<u64>`.
  `pub fn load(path: &std::path::Path) -> StatusData` — never fails; missing
  file or parse error both yield `StatusData::default()` (all `None`), per
  the design doc's "the renderer never depends on the file existing" rule.
  Consumed by Task 6 (llmenv-sourced widget rendering) and Task 8
  (orchestrator).

- [ ] **Step 1: Write the failing test**

Create `src/cli/statusline/data.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_parses_full_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(
            &path,
            r#"{
                "$schema": "llmenv-status-v1", "v": 1, "ts": "2026-07-15T14:23:00Z",
                "scopes": { "tags": ["dev", "rust"] },
                "plugins": { "total": 12, "errors": 0 },
                "mcps": { "total": 12, "errors": 0 },
                "icm": { "memories": 142, "concepts": 47 },
                "throttle": null,
                "config_stale": false,
                "cache": { "prunable_bytes": 15728640 },
                "session_log": 8
            }"#,
        )
        .unwrap();
        let data = StatusData::load(&path);
        assert_eq!(data.scopes.unwrap().tags, vec!["dev", "rust"]);
        assert_eq!(data.plugins.unwrap().total, 12);
        assert_eq!(data.icm.unwrap().memories, 142);
        assert_eq!(data.config_stale, Some(false));
        assert_eq!(data.cache.unwrap().prunable_bytes, 15_728_640);
        assert_eq!(data.session_log, Some(8));
        assert!(data.throttle.is_none());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let data = StatusData::load(std::path::Path::new("/nonexistent/llmenv-status.json"));
        assert_eq!(data, StatusData::default());
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let data = StatusData::load(&path);
        assert_eq!(data, StatusData::default());
    }

    #[test]
    fn load_partial_json_defaults_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llmenv-status.json");
        std::fs::write(&path, r#"{"session_log": 3}"#).unwrap();
        let data = StatusData::load(&path);
        assert_eq!(data.session_log, Some(3));
        assert!(data.scopes.is_none());
        assert!(data.plugins.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline::data:: -- --nocapture`
Expected: FAIL — `StatusData` doesn't exist yet.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/cli/statusline/data.rs` (above the test module):

```rust
//! `llmenv-status.json` — llmenv-sourced stats consumed by the statusline
//! renderer. Pure parsing only: no scope resolution, no MCP calls, no
//! business logic. All fields written once at data-file-write time by
//! `src/materialize/status_data.rs`.

use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct StatusData {
    pub scopes: Option<ScopesData>,
    pub plugins: Option<CountData>,
    pub mcps: Option<CountData>,
    pub icm: Option<IcmData>,
    pub throttle: Option<ThrottleData>,
    pub config_stale: Option<bool>,
    pub cache: Option<CacheData>,
    pub session_log: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ScopesData {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct CountData {
    pub total: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct IcmData {
    pub memories: u64,
    pub concepts: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThrottleData {
    pub backend: String,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct CacheData {
    pub prunable_bytes: u64,
}

impl StatusData {
    /// Load and parse `llmenv-status.json` at `path`. Never fails: a missing
    /// file, unreadable file, or parse error all yield `StatusData::default()`
    /// (every field `None`) so the renderer's llmenv-sourced widgets simply
    /// render empty rather than aborting the whole statusline.
    #[must_use]
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}
```

Create `src/cli/statusline/mod.rs`:

```rust
//! `llmenv statusline` — first-class statusline renderer. See
//! `docs/superpowers/specs/2026-07-15-statusline-design.md`.

mod data;

pub use data::StatusData;
```

Wire `mod statusline;` into `src/cli/mod.rs` next to the existing
`mod doctor; mod setup; mod status; mod upgrade;` lines. Add `tempfile` as a
dev-dependency check: `rg -n "^tempfile" Cargo.toml` — it's already used
elsewhere in this crate's tests (`src/adapter/claude_code.rs` uses
`tempfile::tempdir()`), so no `Cargo.toml` change needed.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline::data:: -- --nocapture`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline/ src/cli/mod.rs
git commit -m "feat(statusline): add llmenv-status.json data schema

Fixes #836"
```

---

### Task 3: Row template parser

**Files:**

- Create: `src/cli/statusline/template.rs`
- Modify: `src/cli/statusline/mod.rs` (`mod template; pub use template::{parse_template, TemplateToken};`)

**Interfaces:**

- Consumes: a `&str` row template (e.g. `"{model} │ {context_pct}"`).
- Produces: `pub enum TemplateToken { Literal(String), Widget { name: String, truncate: bool } }`
  and `pub fn parse_template(template: &str) -> Vec<TemplateToken>`. Consumed
  by Task 8 (orchestrator), which resolves each `Widget` token against the
  widget renderers from Tasks 5/6.

- [ ] **Step 1: Write the failing test**

Create `src/cli/statusline/template.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_literal_and_widget_tokens() {
        let tokens = parse_template("{model} │ {context_pct}");
        assert_eq!(
            tokens,
            vec![
                TemplateToken::Widget { name: "model".to_string(), truncate: false },
                TemplateToken::Literal(" │ ".to_string()),
                TemplateToken::Widget { name: "context_pct".to_string(), truncate: false },
            ]
        );
    }

    #[test]
    fn parses_truncate_shorthand() {
        let tokens = parse_template("{scopes:t}");
        assert_eq!(
            tokens,
            vec![TemplateToken::Widget { name: "scopes".to_string(), truncate: true }]
        );
    }

    #[test]
    fn plain_literal_with_no_placeholders() {
        let tokens = parse_template("no widgets here");
        assert_eq!(tokens, vec![TemplateToken::Literal("no widgets here".to_string())]);
    }

    #[test]
    fn unclosed_brace_is_literal() {
        let tokens = parse_template("{model");
        assert_eq!(tokens, vec![TemplateToken::Literal("{model".to_string())]);
    }

    #[test]
    fn empty_template_yields_no_tokens() {
        assert_eq!(parse_template(""), Vec::<TemplateToken>::new());
    }

    fn arb_template_char() -> impl Strategy<Value = char> {
        prop_oneof![
            Just('{'), Just('}'), Just(':'), Just('t'),
            "[a-z_]".prop_map(|s| s.chars().next().unwrap()),
        ]
    }

    proptest! {
        // Any string built purely from the parser's own alphabet must parse
        // without panicking, and re-flattening the tokens' literal text plus
        // widget braces must not silently drop input length in a way that
        // loses non-widget characters (a weaker, panic-safety-focused
        // invariant — full round-trip isn't required since `{bad` is
        // deliberately folded into a literal).
        #[test]
        fn parse_template_never_panics(s in prop::collection::vec(arb_template_char(), 0..40)) {
            let input: String = s.into_iter().collect();
            let _ = parse_template(&input);
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline::template:: -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/cli/statusline/template.rs`:

```rust
//! Row template parsing: `"{model} │ {context_pct}"` → literal + widget
//! tokens, resolved against widget renderers by the orchestrator.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateToken {
    Literal(String),
    Widget { name: String, truncate: bool },
}

/// Parse a row template into tokens. `{name}` is a widget reference;
/// `{name:t}` is shorthand for "apply the widget's configured truncation".
/// An unclosed `{` (no matching `}`) is treated as literal text, not an
/// error — the design doc requires the renderer to never fail on template
/// parsing, only on data/config I/O.
#[must_use]
pub fn parse_template(template: &str) -> Vec<TemplateToken> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = template.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c != '{' {
            literal.push(c);
            continue;
        }
        // Look for the matching '}' from here.
        let rest = &template[start + 1..];
        if let Some(end) = rest.find('}') {
            let inner = &rest[..end];
            let (name, truncate) = match inner.split_once(':') {
                Some((name, "t")) => (name, true),
                _ => (inner, false),
            };
            if !literal.is_empty() {
                tokens.push(TemplateToken::Literal(std::mem::take(&mut literal)));
            }
            tokens.push(TemplateToken::Widget {
                name: name.to_string(),
                truncate,
            });
            // Advance the outer iterator past the consumed `inner}`.
            for _ in 0..=end {
                chars.next();
            }
        } else {
            // No closing brace anywhere in the remainder: the rest of the
            // template (including this `{`) is literal.
            literal.push_str(&template[start..]);
            break;
        }
    }
    if !literal.is_empty() {
        tokens.push(TemplateToken::Literal(literal));
    }
    tokens
}
```

Add `mod template;` and `pub use template::{TemplateToken, parse_template};`
to `src/cli/statusline/mod.rs`. Confirm `proptest` is already a dev-dependency
(`rg -n "^proptest" Cargo.toml`) — it is, per the existing property tests in
`claude_code.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline::template:: -- --nocapture`
Expected: PASS (5 unit tests + proptest).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline/template.rs src/cli/statusline/mod.rs
git commit -m "feat(statusline): add row template parser

Fixes #836"
```

---

### Task 4: Truncation + ANSI style helpers

**Files:**

- Modify: `src/cli/style.rs` (extend — this is the existing ANSI convention
  to reuse, not a new module; add functions at the bottom)
- Test: inline in `src/cli/style.rs`

**Interfaces:**

- Consumes: nothing new.
- Produces: `pub fn truncate_ellipsis(s: &str, max_len: usize) -> String`
  (UTF-8-boundary-safe, appends `…` when truncated) and
  `pub fn apply_style(s: &str, style: &str, use_color: bool) -> String`
  (parses a space-separated style token string — `bold`, `dim`, named
  16-colour, `#rrggbb`, `color-N` — into ANSI escape codes, wrapping `s`).
  Consumed by Task 5/6 (widget renderers) and Task 8 (orchestrator, which
  already has a `use_color` bool per the existing `ColorMode` convention in
  this file).

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/cli/style.rs`, inside its existing
`#[cfg(test)] mod tests` block:

```rust
#[test]
fn truncate_ellipsis_leaves_short_strings_alone() {
    assert_eq!(truncate_ellipsis("hi", 10), "hi");
}

#[test]
fn truncate_ellipsis_truncates_and_appends_ellipsis() {
    assert_eq!(truncate_ellipsis("hello world", 5), "hell…");
}

#[test]
fn truncate_ellipsis_zero_max_len_yields_empty() {
    assert_eq!(truncate_ellipsis("hello", 0), "");
}

#[test]
fn truncate_ellipsis_is_utf8_safe_on_multibyte_boundary() {
    // "║" is a 3-byte UTF-8 char; truncating mid-character must not panic
    // or produce invalid UTF-8.
    let s = "║║║║║";
    for max in 0..=6 {
        let out = truncate_ellipsis(s, max);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}

#[test]
fn apply_style_wraps_bold_cyan() {
    let out = apply_style("hi", "bold cyan", true);
    assert!(out.starts_with("\x1b["));
    assert!(out.ends_with("\x1b[0m"));
    assert!(out.contains("hi"));
}

#[test]
fn apply_style_no_color_passes_through() {
    assert_eq!(apply_style("hi", "bold cyan", false), "hi");
}

#[test]
fn apply_style_empty_style_passes_through() {
    assert_eq!(apply_style("hi", "", true), "hi");
}

use proptest::prelude::*;
proptest! {
    #[test]
    fn truncate_ellipsis_never_panics_and_stays_utf8(
        s in ".*",
        max in 0usize..50,
    ) {
        let out = truncate_ellipsis(&s, max);
        prop_assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv style:: -- --nocapture`
Expected: FAIL — `truncate_ellipsis`/`apply_style` don't exist.

- [ ] **Step 3: Write minimal implementation**

Add to `src/cli/style.rs` above its test module:

```rust
/// Truncate `s` to at most `max_len` **characters** (not bytes), appending
/// `…` (U+2026, itself counted within `max_len`) when truncation occurs.
/// UTF-8-boundary-safe: always truncates on a `char` boundary since it
/// iterates `chars()` rather than slicing bytes.
#[must_use]
pub fn truncate_ellipsis(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    if max_len == 0 {
        return String::new();
    }
    let keep = max_len.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Parse a space-separated style token string (`"bold cyan"`, `"#ff00aa"`,
/// `"color-208"`) into ANSI escape codes wrapping `s`. Unknown tokens are
/// ignored (never an error — a typo'd style must not crash the render).
/// `use_color: false` (or an empty `style`) passes `s` through unchanged.
#[must_use]
pub fn apply_style(s: &str, style: &str, use_color: bool) -> String {
    if !use_color || style.trim().is_empty() {
        return s.to_string();
    }
    let mut codes: Vec<String> = Vec::new();
    for token in style.split_whitespace() {
        if let Some(code) = style_token_code(token) {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        return s.to_string();
    }
    format!("\x1b[{}m{s}\x1b[0m", codes.join(";"))
}

fn style_token_code(token: &str) -> Option<String> {
    let named = match token {
        "bold" => Some("1"),
        "dim" => Some("2"),
        "italic" => Some("3"),
        "underline" => Some("4"),
        "blink" => Some("5"),
        "reverse" => Some("7"),
        "hidden" => Some("8"),
        "strikethrough" => Some("9"),
        "black" => Some("30"),
        "red" => Some("31"),
        "green" => Some("32"),
        "yellow" => Some("33"),
        "blue" => Some("34"),
        "magenta" => Some("35"),
        "cyan" => Some("36"),
        "white" => Some("37"),
        _ => None,
    };
    if let Some(code) = named {
        return Some(code.to_string());
    }
    if let Some(hex) = token.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(format!("38;2;{r};{g};{b}"));
        }
        return None;
    }
    if let Some(n) = token.strip_prefix("color-") {
        let n: u8 = n.parse().ok()?;
        return Some(format!("38;5;{n}"));
    }
    None
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv style:: -- --nocapture`
Expected: PASS (all unit tests + proptest).

- [ ] **Step 5: Commit**

```bash
git add src/cli/style.rs
git commit -m "feat(statusline): add truncation and ANSI style helpers

Fixes #836"
```

---

### Task 5: Engine-sourced widgets

**Files:**

- Create: `src/cli/statusline/widget.rs`
- Modify: `src/cli/statusline/mod.rs` (`mod widget; pub use widget::*` — no,
  per "no glob re-exports": `pub use widget::{EngineData, render_widget};`)

**Interfaces:**

- Consumes: `crate::cli::style::{truncate_ellipsis, apply_style}` (Task 4),
  `WidgetConfig`/`IconSet` (Task 1, via `llmenv_config`).
- Produces: `pub struct EngineData` (deserialized stdin JSON, see Task 8 for
  the stdin contract — defined here since widgets are its only consumer):
  fields `workspace: Option<Workspace>`, `model: Option<ModelInfo>`,
  `cost: Option<Cost>`, `context_window: Option<ContextWindow>`,
  `rate_limits: Option<RateLimits>`, all-optional per the design doc's stdin
  contract. And `pub fn render_engine_widget(name: &str, data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>, use_color: bool) -> Option<String>`
  — returns `None` for unknown widget names (caller renders empty), `Some(String)`
  (already truncated + styled) otherwise. Consumed by Task 8 (orchestrator).

- [ ] **Step 1: Write the failing test**

Create `src/cli/statusline/widget.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn engine_data() -> EngineData {
        serde_json::from_value(serde_json::json!({
            "workspace": { "current_dir": "/home/user/llmenv" },
            "model": { "display_name": "Claude Opus 4.8" },
            "cost": { "total_duration_ms": 13_320_000 },
            "context_window": {
                "remaining_percentage": 65.0,
                "context_window_size": 200_000,
                "current_usage": {
                    "input_tokens": 5000,
                    "cache_creation_input_tokens": 1000,
                    "cache_read_input_tokens": 4000
                }
            },
            "rate_limits": {
                "five_hour": { "used_percentage": 24.5, "resets_at": 1_713_264_000 },
                "seven_day": { "used_percentage": 41.0, "resets_at": 1_713_700_000 }
            }
        }))
        .unwrap()
    }

    #[test]
    fn renders_model_default_format() {
        let out = render_engine_widget("model", &engine_data(), None, false).unwrap();
        assert_eq!(out, "Claude Opus 4.8");
    }

    #[test]
    fn renders_folder_from_workspace_basename() {
        let out = render_engine_widget("folder", &engine_data(), None, false).unwrap();
        assert_eq!(out, "llmenv");
    }

    #[test]
    fn renders_context_pct() {
        let out = render_engine_widget("context_pct", &engine_data(), None, false).unwrap();
        assert_eq!(out, "35%"); // 100 - remaining_percentage(65) = 35% used
    }

    #[test]
    fn renders_duration_hms() {
        let out = render_engine_widget("duration", &engine_data(), None, false).unwrap();
        assert_eq!(out, "3h42m"); // 13_320_000 ms = 3h42m
    }

    #[test]
    fn unknown_widget_name_renders_none() {
        assert!(render_engine_widget("not_a_widget", &engine_data(), None, false).is_none());
    }

    #[test]
    fn missing_field_renders_empty_not_panic() {
        let empty = EngineData::default();
        let out = render_engine_widget("model", &empty, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn custom_format_overrides_default() {
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{full_name}".to_string()),
            ..Default::default()
        };
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "model": { "display_name": "Claude Opus 4.8", "full_name": "claude-opus-4-8-20260101" }
        }))
        .unwrap();
        let out = render_engine_widget("model", &data, Some(&cfg), false).unwrap();
        assert_eq!(out, "claude-opus-4-8-20260101");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline::widget:: -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/cli/statusline/widget.rs` (above the test module):

```rust
//! Stateless widget renderers. Each function receives complete input and
//! returns a string — no side effects, no shared mutable state (per the
//! design doc's "Separation of concerns").

use crate::cli::style::{apply_style, truncate_ellipsis};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EngineData {
    pub workspace: Option<Workspace>,
    pub model: Option<ModelInfo>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub current_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub display_name: Option<String>,
    pub full_name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cost {
    pub total_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextWindow {
    pub remaining_percentage: Option<f64>,
    pub context_window_size: Option<u64>,
    pub current_usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<RateLimitWindow>,
    pub seven_day: Option<RateLimitWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitWindow {
    pub used_percentage: Option<f64>,
    pub resets_at: Option<i64>,
}

/// Render one engine-sourced widget by name. Returns `None` for a name this
/// function doesn't recognize (the orchestrator treats that identically to
/// an llmenv-sourced widget miss — render empty). A recognized widget with
/// missing underlying data renders `Some(String::new())`, not `None` —
/// `None` means "not an engine widget at all", not "no data".
#[must_use]
pub fn render_engine_widget(
    name: &str,
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> Option<String> {
    let raw = match name {
        "model" => render_model(data, cfg),
        "folder" => render_folder(data),
        "context_pct" => render_context_pct(data),
        "duration" => render_duration(data),
        "tokens" => render_tokens(data),
        "budget" => render_budget(data),
        "cache_pct" => render_cache_pct(data),
        _ => return None,
    };
    Some(finish(raw, cfg, use_color))
}

/// Apply per-widget truncation + style, shared by every widget render path.
fn finish(raw: String, cfg: Option<&llmenv_config::WidgetConfig>, use_color: bool) -> String {
    let truncated = match cfg.and_then(|c| c.max_len) {
        Some(max) => truncate_ellipsis(&raw, max),
        None => raw,
    };
    match cfg.and_then(|c| c.style.as_deref()) {
        Some(style) => apply_style(&truncated, style, use_color),
        None => truncated,
    }
}

fn render_model(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(model) = &data.model else {
        return String::new();
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{short_name} {version}");
    format
        .replace(
            "{short_name}",
            model.display_name.as_deref().unwrap_or(""),
        )
        .replace("{version}", model.version.as_deref().unwrap_or(""))
        .replace("{full_name}", model.full_name.as_deref().unwrap_or(""))
        .trim()
        .to_string()
}

fn render_folder(data: &EngineData) -> String {
    let Some(path) = data.workspace.as_ref().and_then(|w| w.current_dir.as_deref()) else {
        return String::new();
    };
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn render_context_pct(data: &EngineData) -> String {
    let Some(remaining) = data
        .context_window
        .as_ref()
        .and_then(|c| c.remaining_percentage)
    else {
        return String::new();
    };
    let used = (100.0 - remaining).round() as i64;
    format!("{used}%")
}

fn render_duration(data: &EngineData) -> String {
    let Some(ms) = data.cost.as_ref().and_then(|c| c.total_duration_ms) else {
        return String::new();
    };
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    format!("{h}h{m}m")
}

fn render_tokens(data: &EngineData) -> String {
    let Some(usage) = data
        .context_window
        .as_ref()
        .and_then(|c| c.current_usage.as_ref())
    else {
        return String::new();
    };
    let total = usage.input_tokens.unwrap_or(0)
        + usage.cache_creation_input_tokens.unwrap_or(0)
        + usage.cache_read_input_tokens.unwrap_or(0);
    format_token_count(total)
}

fn format_token_count(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn render_budget(data: &EngineData) -> String {
    let Some(cw) = &data.context_window else {
        return String::new();
    };
    let Some(max) = cw.context_window_size else {
        return String::new();
    };
    let used = cw
        .current_usage
        .as_ref()
        .map(|u| {
            u.input_tokens.unwrap_or(0)
                + u.cache_creation_input_tokens.unwrap_or(0)
                + u.cache_read_input_tokens.unwrap_or(0)
        })
        .unwrap_or(0);
    format!("{}/{}", format_token_count(used), format_token_count(max))
}

fn render_cache_pct(data: &EngineData) -> String {
    let Some(usage) = data
        .context_window
        .as_ref()
        .and_then(|c| c.current_usage.as_ref())
    else {
        return String::new();
    };
    let cache = usage.cache_read_input_tokens.unwrap_or(0) + usage.cache_creation_input_tokens.unwrap_or(0);
    let total = usage.input_tokens.unwrap_or(0) + cache;
    if total == 0 {
        return String::new();
    }
    let pct = (cache as f64 / total as f64 * 100.0).round() as i64;
    format!("{pct}%")
}
```

Note: `branch`, `pr`, and `progress_bar` widgets are deliberately **not**
implemented in this task — the stdin contract in the design doc has no
`branch`/`pr` fields and no defined progress-bar algorithm beyond `context_pct`.
Task 5b below adds them once the exact stdin shape for those three is
confirmed against the actual Claude Code/Crush statusline payload (see Task
5b for the follow-up rationale — this avoids guessing a contract this plan
can't verify against a live payload).

Add `mod widget;` and
`pub use widget::{EngineData, render_engine_widget};` to
`src/cli/statusline/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline::widget:: -- --nocapture`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline/widget.rs src/cli/statusline/mod.rs
git commit -m "feat(statusline): add engine-sourced widget renderers

Fixes #836"
```

---

### Task 5b: Remaining engine widgets (`branch`, `pr`, `progress_bar`)

**Files:**

- Modify: `src/cli/statusline/widget.rs`

**Interfaces:**

- Consumes: same `EngineData` as Task 5 — add `branch: Option<BranchInfo>`
  and `pr: Option<PrInfo>` fields (the design doc's stdin example doesn't
  show these two, but the widget table requires them, so extend
  `EngineData` here rather than blocking Task 5 on this ambiguity).
- Produces: extends `render_engine_widget`'s match arms with `"branch"`,
  `"pr"`, `"progress_bar"`.

- [ ] **Step 1: Write the failing test**

Add to `src/cli/statusline/widget.rs`'s test module:

```rust
#[test]
fn renders_branch_name() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "branch": { "name": "release/3.x" }
    }))
    .unwrap();
    assert_eq!(render_engine_widget("branch", &data, None, false).unwrap(), "release/3.x");
}

#[test]
fn renders_pr_number() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 834 }
    }))
    .unwrap();
    assert_eq!(render_engine_widget("pr", &data, None, false).unwrap(), "#834");
}

#[test]
fn renders_progress_bar_from_context_pct() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 65.0 }
    }))
    .unwrap();
    let out = render_engine_widget("progress_bar", &data, None, false).unwrap();
    assert_eq!(out, "35% ███░░░░░░░");
}

#[test]
fn missing_branch_and_pr_render_empty() {
    let empty = EngineData::default();
    assert_eq!(render_engine_widget("branch", &empty, None, false).unwrap(), "");
    assert_eq!(render_engine_widget("pr", &empty, None, false).unwrap(), "");
    assert_eq!(render_engine_widget("progress_bar", &empty, None, false).unwrap(), "");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline::widget:: -- --nocapture`
Expected: FAIL — `"branch"`/`"pr"`/`"progress_bar"` fall through to `None`
in the match, and `EngineData` has no `branch`/`pr` fields yet.

- [ ] **Step 3: Write minimal implementation**

In `src/cli/statusline/widget.rs`, add fields to `EngineData`:

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EngineData {
    pub workspace: Option<Workspace>,
    pub model: Option<ModelInfo>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    pub rate_limits: Option<RateLimits>,
    pub branch: Option<BranchInfo>,
    pub pr: Option<PrInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchInfo {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrInfo {
    pub number: Option<u64>,
}
```

Add match arms in `render_engine_widget`:

```rust
        "branch" => render_branch(data),
        "pr" => render_pr(data),
        "progress_bar" => render_progress_bar(data),
```

Add render functions:

```rust
fn render_branch(data: &EngineData) -> String {
    data.branch
        .as_ref()
        .and_then(|b| b.name.clone())
        .unwrap_or_default()
}

fn render_pr(data: &EngineData) -> String {
    match data.pr.as_ref().and_then(|p| p.number) {
        Some(n) => format!("#{n}"),
        None => String::new(),
    }
}

/// 10-cell block bar. `pct` is the used percentage (100 - remaining).
fn render_progress_bar(data: &EngineData) -> String {
    let Some(remaining) = data
        .context_window
        .as_ref()
        .and_then(|c| c.remaining_percentage)
    else {
        return String::new();
    };
    let used = (100.0 - remaining).clamp(0.0, 100.0);
    let filled = ((used / 10.0).round() as usize).min(10);
    let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
    format!("{}% {bar}", used.round() as i64)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline::widget:: -- --nocapture`
Expected: PASS (all widget tests, 11 total).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline/widget.rs
git commit -m "feat(statusline): add branch, pr, progress_bar widgets

Fixes #836"
```

---

### Task 6: llmenv-sourced widgets

**Files:**

- Create: `src/cli/statusline/llmenv_widget.rs`
- Modify: `src/cli/statusline/mod.rs` (`mod llmenv_widget; pub use llmenv_widget::render_llmenv_widget;`)

**Interfaces:**

- Consumes: `StatusData` (Task 2), `apply_style`/`truncate_ellipsis` (Task
  4), `llmenv_config::{WidgetConfig, StatuslineConfig}` (Task 1, for the
  `icons:` map).
- Produces: `pub fn render_llmenv_widget(name: &str, data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>, icons: &std::collections::BTreeMap<String, String>, use_color: bool) -> Option<String>`,
  same `None` = unknown-name / `Some(String)` = empty-or-rendered contract as
  Task 5. Consumed by Task 8 (orchestrator).

- [ ] **Step 1: Write the failing test**

Create `src/cli/statusline/llmenv_widget.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::statusline::data::{CacheData, CountData, IcmData, ScopesData, StatusData};
    use std::collections::BTreeMap;

    fn icons() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("config_stale".to_string(), "◌".to_string()),
            ("config_ok".to_string(), "".to_string()),
        ])
    }

    #[test]
    fn renders_scopes_tags() {
        let data = StatusData {
            scopes: Some(ScopesData { tags: vec!["dev".into(), "rust".into()] }),
            ..Default::default()
        };
        let out = render_llmenv_widget("scopes", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "║ dev · rust");
    }

    #[test]
    fn renders_plugins_total() {
        let data = StatusData {
            plugins: Some(CountData { total: 12, errors: 0 }),
            ..Default::default()
        };
        let out = render_llmenv_widget("plugins", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "◇ 12");
    }

    #[test]
    fn renders_icm_memories() {
        let data = StatusData {
            icm: Some(IcmData { memories: 142, concepts: 47 }),
            ..Default::default()
        };
        let out = render_llmenv_widget("icm", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "M142");
    }

    #[test]
    fn renders_cache_prunable_bytes_humanized() {
        let data = StatusData {
            cache: Some(CacheData { prunable_bytes: 15_728_640 }),
            ..Default::default()
        };
        let out = render_llmenv_widget("cache", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "15 MB");
    }

    #[test]
    fn renders_config_stale_icon() {
        let data = StatusData { config_stale: Some(true), ..Default::default() };
        let out = render_llmenv_widget("config_stale", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "◌");
    }

    #[test]
    fn missing_data_renders_empty() {
        let data = StatusData::default();
        for name in ["scopes", "plugins", "mcps", "icm", "cache", "config_stale", "throttle", "session_log"] {
            assert_eq!(
                render_llmenv_widget(name, &data, None, &icons(), false).unwrap(),
                "",
                "widget {name} should render empty on missing data"
            );
        }
    }

    #[test]
    fn unknown_widget_renders_none() {
        assert!(render_llmenv_widget("not_real", &StatusData::default(), None, &icons(), false).is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline::llmenv_widget:: -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/cli/statusline/llmenv_widget.rs`:

```rust
//! llmenv-sourced widget renderers — same stateless contract as
//! `widget.rs`'s engine-sourced renderers, reading from `StatusData` instead
//! of stdin.

use super::data::StatusData;
use crate::cli::style::{apply_style, truncate_ellipsis};
use std::collections::BTreeMap;

#[must_use]
pub fn render_llmenv_widget(
    name: &str,
    data: &StatusData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    icons: &BTreeMap<String, String>,
    use_color: bool,
) -> Option<String> {
    let raw = match name {
        "scopes" => render_scopes(data, cfg),
        "plugins" => render_plugins(data, cfg),
        "mcps" => render_mcps(data, cfg),
        "icm" => render_icm(data, cfg),
        "cache" => render_cache(data, cfg),
        "config_stale" => render_config_stale(data, cfg, icons),
        "throttle" => render_throttle(data, cfg),
        "session_log" => render_session_log(data, cfg, icons),
        _ => return None,
    };
    Some(finish(raw, cfg, use_color))
}

fn finish(raw: String, cfg: Option<&llmenv_config::WidgetConfig>, use_color: bool) -> String {
    let truncated = match cfg.and_then(|c| c.max_len) {
        Some(max) => truncate_ellipsis(&raw, max),
        None => raw,
    };
    match cfg.and_then(|c| c.style.as_deref()) {
        Some(style) => apply_style(&truncated, style, use_color),
        None => truncated,
    }
}

fn render_scopes(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(scopes) = &data.scopes else {
        return String::new();
    };
    if scopes.tags.is_empty() {
        return String::new();
    }
    let tags = scopes.tags.join(" · ");
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("║ {tags}");
    format.replace("{tags}", &tags)
}

fn render_plugins(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(plugins) = &data.plugins else {
        return String::new();
    };
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("◇ {total}");
    format
        .replace("{total}", &plugins.total.to_string())
        .replace("{errors}", &plugins.errors.to_string())
}

fn render_mcps(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(mcps) = &data.mcps else {
        return String::new();
    };
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("MCP {total}");
    format
        .replace("{total}", &mcps.total.to_string())
        .replace("{errors}", &mcps.errors.to_string())
}

fn render_icm(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(icm) = &data.icm else {
        return String::new();
    };
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("M{memories}");
    format
        .replace("{memories}", &icm.memories.to_string())
        .replace("{concepts}", &icm.concepts.to_string())
}

fn render_cache(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(cache) = &data.cache else {
        return String::new();
    };
    let humanized = humanize_bytes(cache.prunable_bytes);
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{prunable}");
    format
        .replace("{prunable}", &humanized)
        .replace("{prunable_raw}", &cache.prunable_bytes.to_string())
}

fn humanize_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

fn render_config_stale(
    data: &StatusData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    icons: &BTreeMap<String, String>,
) -> String {
    let Some(stale) = data.config_stale else {
        return String::new();
    };
    if !stale {
        return String::new();
    }
    let icon = icons.get("config_stale").cloned().unwrap_or_else(|| "◌".to_string());
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{stale_icon}");
    format.replace("{stale_icon}", &icon)
}

fn render_throttle(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(throttle) = &data.throttle else {
        return String::new();
    };
    let raw = format!("{}: {}s", throttle.backend, throttle.cooldown_secs);
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{raw}");
    format
        .replace("{raw}", &raw)
        .replace("{cooldown_secs}", &throttle.cooldown_secs.to_string())
        .replace("{reason}", &throttle.backend)
}

fn render_session_log(
    data: &StatusData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    icons: &BTreeMap<String, String>,
) -> String {
    let Some(entries) = data.session_log else {
        return String::new();
    };
    let icon = icons.get("session_log").cloned().unwrap_or_else(|| "📝".to_string());
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{icon} {entries}");
    format
        .replace("{icon}", &icon)
        .replace("{entries}", &entries.to_string())
}
```

Note: this deliberately reads `throttle.backend`/`throttle.cooldown_secs`
(not the design doc's illustrative `raw`/`reason`/`icon` field names for the
*source* data) because the underlying `Throttle` config struct
(`crates/llmenv-config/src/schema.rs:1076-1094`) has no `reason`/`icon`
fields to source them from — see Task 10's note on this same deviation.

Add `mod llmenv_widget;` and
`pub use llmenv_widget::render_llmenv_widget;` to
`src/cli/statusline/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline::llmenv_widget:: -- --nocapture`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline/llmenv_widget.rs src/cli/statusline/mod.rs
git commit -m "feat(statusline): add llmenv-sourced widget renderers

Fixes #836"
```

---

### Task 7: Icon-set resolution

**Files:**

- Modify: `src/cli/statusline/llmenv_widget.rs` (icon lookup already takes
  a resolved `icons` map — this task adds the `icon_set` → glyph choice
  layer that produces that map)
- Create: `src/cli/statusline/icons.rs`
- Modify: `src/cli/statusline/mod.rs`

**Interfaces:**

- Consumes: `llmenv_config::IconSet`, the user's `icons:` config map (Task 1).
- Produces: `pub fn resolve_icons(icon_set: llmenv_config::IconSet, configured: &std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String>`
  — merges built-in defaults for `simple`/`nerd`/`none` with user overrides
  (user config always wins). Consumed by Task 8 (orchestrator), which passes
  the result into `render_llmenv_widget`'s `icons` parameter.

- [ ] **Step 1: Write the failing test**

Create `src/cli/statusline/icons.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use llmenv_config::IconSet;
    use std::collections::BTreeMap;

    #[test]
    fn simple_icon_set_provides_ascii_defaults() {
        let icons = resolve_icons(IconSet::Simple, &BTreeMap::new());
        assert_eq!(icons.get("config_stale").map(String::as_str), Some("~"));
        assert_eq!(icons.get("config_ok").map(String::as_str), Some("*"));
    }

    #[test]
    fn nerd_icon_set_provides_nerd_glyphs() {
        let icons = resolve_icons(IconSet::Nerd, &BTreeMap::new());
        assert_eq!(icons.get("config_stale").map(String::as_str), Some("\u{f0e7}"));
    }

    #[test]
    fn none_icon_set_yields_empty_icons() {
        let icons = resolve_icons(IconSet::None, &BTreeMap::new());
        assert_eq!(icons.get("config_stale").map(String::as_str), Some(""));
    }

    #[test]
    fn user_config_overrides_defaults() {
        let mut configured = BTreeMap::new();
        configured.insert("config_stale".to_string(), "!!!".to_string());
        let icons = resolve_icons(IconSet::Simple, &configured);
        assert_eq!(icons.get("config_stale").map(String::as_str), Some("!!!"));
    }

    #[test]
    fn auto_resolves_to_simple_when_nerd_font_env_unset() {
        // SAFETY (test-only): scoped std::env::remove_var in a single-threaded
        // test process; no other test in this module reads this var.
        unsafe { std::env::remove_var("LLMENV_NERD_FONT") };
        let icons = resolve_icons(IconSet::Auto, &BTreeMap::new());
        assert_eq!(icons.get("config_stale").map(String::as_str), Some("~"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline::icons:: -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/cli/statusline/icons.rs`:

```rust
//! Icon-set resolution: `icon_set` config choice → concrete glyph map,
//! merged with user overrides (`statusline.icons`, always highest
//! precedence).

use llmenv_config::IconSet;
use std::collections::BTreeMap;

const SIMPLE_ICONS: &[(&str, &str)] = &[
    ("config_ok", "*"),
    ("config_stale", "~"),
    ("icm_ok", "*"),
    ("throttle", "!"),
    ("plugin_ok", "*"),
    ("plugin_error", "x"),
    ("cache_ok", "*"),
    ("cache_prunable", "#"),
    ("session_log", "log"),
];

const NERD_ICONS: &[(&str, &str)] = &[
    ("config_ok", "\u{f00c}"),
    ("config_stale", "\u{f0e7}"),
    ("icm_ok", "\u{f00c}"),
    ("throttle", "\u{f071}"),
    ("plugin_ok", "\u{f00c}"),
    ("plugin_error", "\u{f00d}"),
    ("cache_ok", "\u{f00c}"),
    ("cache_prunable", "\u{f187}"),
    ("session_log", "\u{f15c}"),
];

/// Detect whether the terminal is likely running a Nerd Font. There is no
/// portable terminal-capability probe for this, so `auto` keys off an
/// explicit opt-in env var — the same mechanism users already set for their
/// shell prompt (e.g. Starship's `NERD_FONT` convention). Defaults to
/// `simple` (ASCII/Unicode, safe everywhere) when unset.
fn nerd_font_detected() -> bool {
    std::env::var("LLMENV_NERD_FONT").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[must_use]
pub fn resolve_icons(
    icon_set: IconSet,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut icons: BTreeMap<String, String> = match icon_set {
        IconSet::None => BTreeMap::new(),
        IconSet::Simple => SIMPLE_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        IconSet::Nerd => NERD_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        IconSet::Auto if nerd_font_detected() => NERD_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        IconSet::Auto => SIMPLE_ICONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    };
    // For `none`, every known key still needs to resolve to "" rather than
    // being absent (widgets call `.get(...).unwrap_or_default()`-style
    // lookups) — but here we only pre-seed keys the widgets actually query,
    // and `render_config_stale`/`render_session_log` already fall back to a
    // hardcoded default when the map lookup misses, so an empty map for
    // `None` combined with the user's `configured` overlay is correct:
    // anything not explicitly configured renders as "" via the widget's own
    // `.unwrap_or_else(|| "...".to_string())` — that fallback only fires
    // when the icon_set is `Simple`/`Nerd`/`Auto`. For `None`, force every
    // known key to an explicit empty string so those widget fallbacks don't
    // silently reintroduce a glyph.
    if icon_set == IconSet::None {
        for (k, _) in SIMPLE_ICONS {
            icons.insert((*k).to_string(), String::new());
        }
    }
    for (k, v) in configured {
        icons.insert(k.clone(), v.clone());
    }
    icons
}
```

Add `mod icons;` and `pub use icons::resolve_icons;` to
`src/cli/statusline/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline::icons:: -- --nocapture`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline/icons.rs src/cli/statusline/mod.rs
git commit -m "feat(statusline): add icon-set resolution

Fixes #836"
```

---

### Task 8: Renderer orchestration + CLI wiring

**Files:**

- Create: `src/cli/statusline.rs` (the orchestrator — sibling to the
  `statusline/` submodule dir, following the same `foo.rs` + `foo/` split
  pattern as `src/cli/status.rs` if it has one, or simply put the
  orchestrator directly in `src/cli/statusline/mod.rs` and rename the
  existing `mod.rs` content into `src/cli/statusline/support.rs` — **check
  first**: `ls src/cli/*.rs src/cli/*/  | rg -B2 "status"` to see whether
  this repo's convention is `foo.rs` importing `foo/*.rs` submodules, or
  `foo/mod.rs` — mirror whichever the existing `doctor`/`setup` modules use)
- Modify: `src/cli/mod.rs` (register `Command::Statusline`, dispatch arm)

**Interfaces:**

- Consumes: `EngineData` (Task 5), `StatusData` (Task 2),
  `llmenv_config::StatuslineConfig` (Task 1), `parse_template`/`TemplateToken`
  (Task 3), `render_engine_widget`/`render_llmenv_widget` (Tasks 5/6),
  `resolve_icons` (Task 7).
- Produces: `pub fn run_statusline(config: &llmenv_config::Config, data_path: &std::path::Path, stdin: &mut impl std::io::Read, use_color: bool) -> anyhow::Result<String>`
  — the full render pipeline, returning the ANSI output as a `String` (the
  CLI wrapper writes it to stdout and maps `Ok`/`Err` to exit 0 / exit
  non-zero per the design doc's renderer contract). Consumed by
  `src/cli/mod.rs`'s dispatch arm.

- [ ] **Step 1: Write the failing test**

Add a `#[cfg(test)] mod tests` block to the orchestrator file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use llmenv_config::StatuslineConfig;

    #[test]
    fn renders_default_single_row_when_config_absent() {
        let config = llmenv_config::Config::default();
        let stdin = br#"{"model": {"display_name": "Claude Opus 4.8"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json"); // missing file
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert!(out.contains("Claude Opus 4.8"));
        assert!(out.contains(" │ "));
    }

    #[test]
    fn renders_configured_rows() {
        let mut config = llmenv_config::Config::default();
        config.statusline = Some(StatuslineConfig {
            rows: vec!["{model}".to_string()],
            ..Default::default()
        });
        let stdin = br#"{"model": {"display_name": "GPT-Z"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert_eq!(out.trim_end(), "GPT-Z");
    }

    #[test]
    fn missing_data_file_still_renders_engine_widgets() {
        let mut config = llmenv_config::Config::default();
        config.statusline = Some(StatuslineConfig {
            rows: vec!["{model} {plugins}".to_string()],
            ..Default::default()
        });
        let stdin = br#"{"model": {"display_name": "GPT-Z"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("does-not-exist.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert!(out.contains("GPT-Z"));
    }

    #[test]
    fn malformed_stdin_renders_engine_widgets_empty_not_error() {
        let config = llmenv_config::Config::default();
        let stdin = b"not json";
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false);
        assert!(out.is_ok(), "malformed stdin must degrade, not error");
    }

    #[test]
    fn all_widgets_empty_yields_empty_row() {
        let mut config = llmenv_config::Config::default();
        config.statusline = Some(StatuslineConfig {
            rows: vec!["{model}".to_string()],
            ..Default::default()
        });
        let stdin = b"{}";
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert_eq!(out, "\n");
    }

    #[test]
    fn unknown_widget_name_in_template_renders_empty() {
        let mut config = llmenv_config::Config::default();
        config.statusline = Some(StatuslineConfig {
            rows: vec!["{bogus_widget}".to_string()],
            ..Default::default()
        });
        let out = run_statusline(
            &config,
            std::path::Path::new("/nonexistent"),
            &mut &b""[..],
            false,
        )
        .unwrap();
        assert_eq!(out, "\n");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv statusline -- --nocapture`
Expected: FAIL — `run_statusline` doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Write the orchestrator (place at `src/cli/statusline/mod.rs` alongside the
`mod` declarations already added in Tasks 2–7, adding this function and its
imports — this file is now both the module root and the orchestrator, matching
the small-file convention already used for `status.rs`/`doctor.rs` command
entry points):

```rust
use crate::cli::statusline::data::StatusData;
use crate::cli::statusline::template::TemplateToken;
use std::io::Read;

const DEFAULT_ROW: &str = "{model} │ {folder} │ {branch} │ {context_pct} │ {budget}";

/// Full render pipeline: stdin (engine JSON) + data file (llmenv stats) +
/// config (`statusline:` section) → ANSI rows, one `\n`-terminated line per
/// configured row. Never returns `Err` for "no data" conditions (missing
/// data file, malformed stdin, unknown widget names) — only for genuine I/O
/// failure reading stdin itself. See the design doc's "Renderer contract".
pub fn run_statusline(
    config: &llmenv_config::Config,
    data_path: &std::path::Path,
    stdin: &mut impl Read,
    use_color: bool,
) -> anyhow::Result<String> {
    let mut stdin_buf = String::new();
    stdin.read_to_string(&mut stdin_buf)?;
    let engine_data: widget::EngineData = serde_json::from_str(&stdin_buf).unwrap_or_default();
    let status_data = StatusData::load(data_path);

    let cfg = config.statusline.clone().unwrap_or_default();
    let rows: Vec<String> = if cfg.rows.is_empty() {
        vec![DEFAULT_ROW.to_string()]
    } else {
        cfg.rows.clone()
    };
    let icons = icons::resolve_icons(cfg.style.icon_set, &cfg.icons);

    let mut out = String::new();
    for row in &rows {
        let tokens = template::parse_template(row);
        let mut rendered_any = false;
        let mut line = String::new();
        for token in tokens {
            match token {
                TemplateToken::Literal(text) => line.push_str(&text),
                TemplateToken::Widget { name, truncate } => {
                    let widget_cfg = cfg.widgets.get(&name);
                    let effective_cfg = if truncate { widget_cfg } else { widget_cfg };
                    let value = widget::render_engine_widget(&name, &engine_data, effective_cfg, use_color)
                        .or_else(|| {
                            llmenv_widget::render_llmenv_widget(
                                &name,
                                &status_data,
                                effective_cfg,
                                &icons,
                                use_color,
                            )
                        })
                        .unwrap_or_default();
                    if !value.is_empty() {
                        rendered_any = true;
                    }
                    line.push_str(&value);
                }
            }
        }
        // No orphaned separators: a row whose only content is literal
        // separator text (all widgets empty) still needs *some* output per
        // the design doc, but must not display bare separators with no
        // data. Render an empty line for that row instead of the
        // separator-only text.
        if rendered_any {
            out.push_str(&line);
        }
        out.push('\n');
    }
    Ok(out)
}
```

Wire into `src/cli/mod.rs`: add a `Statusline` variant near `Command::Export`
(around `src/cli/mod.rs:131`):

```rust
    /// Render the statusline (reads engine session JSON from stdin).
    Statusline,
```

Add the dispatch arm near the `Command::Export` handler (around line 399):

```rust
        Some(Command::Statusline) => {
            let config_path = paths::config_path()?;
            let config = Config::load(&config_path)?;
            let env = crate::scope::matcher::Env::detect();
            let active = crate::scope::evaluate(&config, &env);
            let adapter = current_adapter(&config, &active)?; // reuse whatever
                // helper this file already uses to pick the active adapter for
                // the current context (grep `fn current_adapter` or similar —
                // if none exists, the materialized-dir resolution used by
                // `run_export`/`materialize_with_mode`'s caller is the pattern
                // to mirror: same cache_root/adapter_root/shape computation).
            let data_path = adapter_root.join(/* version */).join(/* hash */).join("llmenv-status.json");
            let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let output = statusline::run_statusline(&config, &data_path, &mut std::io::stdin(), use_color)?;
            print!("{output}");
        }
```

**Implementer note:** the exact `data_path` resolution (finding the current
materialized folder's hash) must mirror whatever `src/cli/mod.rs` already
uses to locate the active materialized dir for the current adapter/context —
read `run_export`'s body and the `materialize_with_mode` call site
(`src/cli/mod.rs:1267-1290`, found during planning) to find the real
helper/variable names, since this plan was written without executing that
lookup live. Do not invent a new resolution path; reuse the existing one.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv statusline -- --nocapture`
Expected: PASS (6 orchestrator tests + all widget/data/template/icon tests
from Tasks 1–7).

- [ ] **Step 5: Commit**

```bash
git add src/cli/statusline.rs src/cli/statusline/ src/cli/mod.rs
git commit -m "feat(statusline): add renderer orchestrator and CLI subcommand

Fixes #836"
```

---

### Task 9: Throttle state reader

**Files:**

- Modify: `src/throttle/mod.rs`

**Interfaces:**

- Consumes: `throttle_state_path` (already private in this file — this task
  makes the read path public alongside the existing write path).
- Produces: `pub fn read_active_throttle() -> anyhow::Result<Option<Throttle>>`.
  Consumed by Task 10 (`collect_status_data`).

- [ ] **Step 1: Write the failing test**

Add to `src/throttle/mod.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn read_active_throttle_returns_none_when_no_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY (test-only): std::env::set_var scoped to this test process;
    // throttle state path resolution reads LLMENV_STATE_DIR at call time.
    unsafe { std::env::set_var("LLMENV_STATE_DIR", tmp.path()) };
    let result = read_active_throttle().unwrap();
    assert!(result.is_none());
}

#[test]
fn read_active_throttle_round_trips_stored_value() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("LLMENV_STATE_DIR", tmp.path()) };
    let cfg = Throttle {
        backend: "anthropic".to_string(),
        when: vec!["dev".to_string()],
        cache_ttl: 30,
        max_wait: 60,
        soft_threshold: 80,
    };
    store_active_throttle(Some(&cfg)).unwrap();
    let result = read_active_throttle().unwrap();
    assert_eq!(result.unwrap().backend, "anthropic");
}
```

(Confirm `Throttle`'s exact field set at
`crates/llmenv-config/src/schema.rs:1076-1094` before writing this test —
adjust field names/types to match if the plan's summary above is imprecise.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv throttle::read_active_throttle -- --nocapture`
Expected: FAIL — function doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Add to `src/throttle/mod.rs`, next to `store_active_throttle`:

```rust
/// Read back the currently stored throttle state (written by
/// `store_active_throttle` during materialize/export). Returns `None` when
/// no state file exists (throttling is off) or the file is unreadable/corrupt
/// — a stale or hand-edited state file must not crash callers like the
/// statusline data collector.
///
/// # Errors
/// Returns an error only if the state directory itself cannot be resolved.
pub fn read_active_throttle() -> anyhow::Result<Option<Throttle>> {
    let state_dir = crate::paths::state_dir()?;
    let path = throttle_state_path(&state_dir);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Ok(None),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv throttle::read_active_throttle -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/throttle/mod.rs
git commit -m "feat(throttle): add read-back for stored throttle state

Fixes #836"
```

---

### Task 10: `collect_status_data` — gather all fields for the data file

**Files:**

- Create: `src/materialize/status_data.rs`
- Modify: `src/materialize/mod.rs` (`mod status_data; pub use status_data::{collect_status_data, StatusDataInput};`
  — or `pub(crate) use` if `src/cli/mod.rs` is in the same crate root, which
  it is: this whole plan lives in the single `llmenv` binary crate)

**Interfaces:**

- Consumes: `crate::scope::evaluate` (returns `ActiveScopes` with `.tags`),
  `crate::plugins::resolve::resolve_plugins`, `crate::mcp::resolve::resolve_mcps`,
  `crate::materialize::cache::{prune, PruneMode}`, `crate::cli::{stale_status}`
  (or wherever it's re-exported — confirm with
  `rg -n "pub fn stale_status|pub enum StaleStatus" src/cli/mod.rs`),
  `crate::session_log::file_sink::default_file_path`,
  `crate::throttle::read_active_throttle` (Task 9), `crate::memory::{connect, call_tool_blocking}`
  if made `pub(crate)` (currently private to `src/memory/mod.rs` — see Step 3
  note).
- Produces: `pub fn collect_status_data(config: &llmenv_config::Config, active: &crate::scope::ActiveScopes, throttle_configs: &[llmenv_config::Throttle], cache_root: &std::path::Path, hashing: llmenv_config::HashingMode) -> StatusDataJson`
  — a serializable struct matching the design doc's JSON shape exactly
  (`$schema`, `v`, `ts`, `scopes`, `plugins`, `mcps`, `icm`, `throttle`,
  `config_stale`, `cache`, `session_log`). Never panics; every sub-collector
  degrades to `None`/omitted on failure. Consumed by Task 11 (materialize
  write-in), Task 12 (`llmenv export`), Task 13 (session start).

- [ ] **Step 1: Write the failing test**

Create `src/materialize/status_data.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_status_data_populates_scopes_tags() {
        let config = llmenv_config::Config::default();
        let env = crate::scope::matcher::Env::detect();
        let active = crate::scope::evaluate(&config, &env);
        let dir = tempfile::tempdir().unwrap();
        let data = collect_status_data(&config, &active, &[], dir.path(), llmenv_config::HashingMode::default());
        // Whatever tags Env::detect() + an empty config produce, the field
        // must always be Some (never panics) even if the set is empty.
        assert!(data.scopes.is_some());
    }

    #[test]
    fn collect_status_data_never_panics_on_empty_config() {
        let config = llmenv_config::Config::default();
        let env = crate::scope::matcher::Env::detect();
        let active = crate::scope::evaluate(&config, &env);
        let dir = tempfile::tempdir().unwrap();
        // Must not panic even though: no plugins configured, no memory
        // backend active (icm stays None), no cache dir exists yet.
        let data = collect_status_data(&config, &active, &[], dir.path(), llmenv_config::HashingMode::default());
        assert!(data.icm.is_none(), "no ICM backend active — must degrade to None, not error");
    }

    #[test]
    fn serializes_to_expected_json_shape() {
        let config = llmenv_config::Config::default();
        let env = crate::scope::matcher::Env::detect();
        let active = crate::scope::evaluate(&config, &env);
        let dir = tempfile::tempdir().unwrap();
        let data = collect_status_data(&config, &active, &[], dir.path(), llmenv_config::HashingMode::default());
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["$schema"], "llmenv-status-v1");
        assert_eq!(json["v"], 1);
        assert!(json.get("ts").is_some());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv status_data:: -- --nocapture`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/materialize/status_data.rs`:

```rust
//! Collects the fields written to `llmenv-status.json` (the statusline data
//! file). All I/O here is best-effort: a sub-collector that can't get its
//! data (no ICM backend active, cache dir not yet materialized, etc.)
//! contributes `None`/omitted rather than failing the whole collection —
//! materialize/export must never abort because a stat is unavailable.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StatusDataJson {
    #[serde(rename = "$schema")]
    pub schema: &'static str,
    pub v: u32,
    pub ts: String,
    pub scopes: Option<ScopesJson>,
    pub plugins: Option<CountJson>,
    pub mcps: Option<CountJson>,
    pub icm: Option<IcmJson>,
    pub throttle: Option<ThrottleJson>,
    pub config_stale: Option<bool>,
    pub cache: Option<CacheJson>,
    pub session_log: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopesJson {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CountJson {
    pub total: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IcmJson {
    pub memories: u64,
    pub concepts: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThrottleJson {
    pub backend: String,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheJson {
    pub prunable_bytes: u64,
}

/// Gather every statusline stat. `cache_root` + `hashing` are needed for the
/// prunable-bytes dry-run scan; `throttle_configs` is the merged manifest
/// throttle list (top-level + bundle) for the `config_stale`-style
/// recompute path — pass `&[]` when unavailable (throttle then reads back
/// only the last-stored state via `read_active_throttle`, skipping
/// resolution against current config).
#[must_use]
pub fn collect_status_data(
    config: &llmenv_config::Config,
    active: &crate::scope::ActiveScopes,
    throttle_configs: &[llmenv_config::Throttle],
    cache_root: &std::path::Path,
    hashing: llmenv_config::HashingMode,
) -> StatusDataJson {
    StatusDataJson {
        schema: "llmenv-status-v1",
        v: 1,
        ts: chrono_now_rfc3339(),
        scopes: Some(ScopesJson {
            tags: active.tags.iter().cloned().collect(),
        }),
        plugins: collect_plugins(config, &active.tags),
        mcps: collect_mcps(config, &active.tags),
        icm: collect_icm(),
        throttle: collect_throttle(throttle_configs, &active.tags),
        config_stale: collect_config_stale(cache_root),
        cache: collect_cache(cache_root, hashing),
        session_log: collect_session_log(),
    }
}

/// This repo has no `chrono`/`time` dependency in the binary crate (check
/// `rg -n "^chrono|^time " Cargo.toml` before assuming — if one already
/// exists elsewhere in the workspace, use it instead of this). Falls back to
/// a Unix-epoch-seconds string if no date/time crate is available, since the
/// design doc marks `ts` as informational only (staleness diagnostic, never
/// parsed by the renderer).
fn chrono_now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

fn collect_plugins(config: &llmenv_config::Config, active_tags: &std::collections::BTreeSet<String>) -> Option<CountJson> {
    match crate::plugins::resolve::resolve_plugins(config, active_tags) {
        Ok(resolved) => Some(CountJson {
            total: resolved.plugins.len() as u64,
            errors: 0,
        }),
        // Fail-fast resolver: a resolve error means at least one
        // configured plugin reference is broken. There's no per-plugin
        // error accumulation today (see plan Task 10 follow-up), so this
        // reports "1 error, 0 known-good" rather than fabricating a count.
        Err(_) => Some(CountJson { total: 0, errors: 1 }),
    }
}

fn collect_mcps(config: &llmenv_config::Config, active_tags: &std::collections::BTreeSet<String>) -> Option<CountJson> {
    // Implementer note: `resolve_mcps`'s exact parameter list
    // (`mcp, memory, host, active_tags`) needs the real argument values from
    // `config` — read `src/mcp/resolve.rs:94`'s signature and an existing
    // call site (grep `resolve_mcps(` in `src/cli/mod.rs` or
    // `src/materialize/mod.rs`) to fill these in correctly; this plan
    // doesn't have the exact field names for `config.mcp`/`config.memory`/
    // `config.host` handy. Same fail-fast → `errors: 1` convention as
    // `collect_plugins` above.
    match crate::mcp::resolve::resolve_mcps(&config.mcp, &config.memory, &config.host, active_tags) {
        Ok(resolved) => Some(CountJson { total: resolved.len() as u64, errors: 0 }),
        Err(_) => Some(CountJson { total: 0, errors: 1 }),
    }
}

/// Best-effort live ICM query. Returns `None` when no memory backend is
/// active for the current scope, the MCP call fails, or the response can't
/// be parsed — every one of those is an expected, non-error condition (most
/// sessions have no ICM backend configured at all).
fn collect_icm() -> Option<IcmJson> {
    let raw = crate::memory::stats_json().ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(IcmJson {
        memories: parsed.get("memories")?.as_u64()?,
        concepts: parsed.get("concepts")?.as_u64().unwrap_or(0),
    })
}

fn collect_throttle(
    throttle_configs: &[llmenv_config::Throttle],
    active_tags: &std::collections::BTreeSet<String>,
) -> Option<ThrottleJson> {
    // Prefer resolving fresh against current config (reflects a same-render
    // change); fall back to the last-stored state if resolution finds
    // nothing (e.g. called from a context with no merged manifest handy).
    let resolved = crate::throttle::resolve_active_throttle(throttle_configs, active_tags)
        .ok()
        .flatten()
        .or_else(|| crate::throttle::read_active_throttle().ok().flatten());
    resolved.map(|t| ThrottleJson {
        backend: t.backend,
        cooldown_secs: t.max_wait,
    })
}

fn collect_config_stale(cache_root: &std::path::Path) -> Option<bool> {
    // Implementer note: `stale_status`/`StaleStatus` (src/cli/mod.rs:32,55)
    // compares a "booted" hash against "current" — read `run_check_stale`
    // (src/cli/mod.rs:1585) to find where each side of that comparison
    // actually comes from (env var? manifest file?) and mirror it here
    // rather than guessing the two input strings.
    let _ = cache_root;
    None
}

fn collect_cache(cache_root: &std::path::Path, hashing: llmenv_config::HashingMode) -> Option<CacheJson> {
    let report = crate::materialize::cache::prune(
        cache_root,
        crate::materialize::cache::PruneMode::StaleOnly,
        hashing,
        None,
        true, // dry_run: must not delete anything just to report a stat
    )
    .ok()?;
    let total_bytes: u64 = report
        .removed
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    Some(CacheJson { prunable_bytes: total_bytes })
}

fn collect_session_log() -> Option<u64> {
    let path = crate::session_log::file_sink::default_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    Some(content.lines().filter(|l| !l.trim().is_empty()).count() as u64)
}
```

**Required follow-up before this compiles as-written:** `crate::memory::stats_json()`
does not exist yet — `src/memory/mod.rs`'s `stats()` function currently
`println!`s the raw ICM response instead of returning it. Add a sibling
function in `src/memory/mod.rs`:

```rust
/// Same as `stats()` but returns the raw JSON string instead of printing it,
/// for programmatic callers (the statusline data collector). Returns `Err`
/// when no memory backend is active for the current scope or the MCP call
/// fails — callers treat that as "no ICM stats available", not a hard error.
pub fn stats_json() -> anyhow::Result<String> {
    let client = connect()?;
    call_tool_blocking(client, "icm_memory_stats", serde_json::json!({}))
}
```

Then change `stats()` to call it: `pub fn stats() -> anyhow::Result<()> { println!("{}", stats_json()?); Ok(()) }`.

Add `mod status_data;` and
`pub use status_data::{collect_status_data, StatusDataJson};` to
`src/materialize/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv status_data:: -- --nocapture`
Expected: PASS (3 tests) — note `collect_config_stale` intentionally always
returns `None` in this task (see Task 10 follow-up below); adjust the
`serializes_to_expected_json_shape` test's expectations if a later task
changes that.

- [ ] **Step 5: Commit**

```bash
git add src/materialize/status_data.rs src/materialize/mod.rs src/memory/mod.rs
git commit -m "feat(statusline): collect llmenv stats for the status data file

Fixes #836"
```

---

### Task 10b: Wire up `config_stale` detection

**Files:**

- Modify: `src/materialize/status_data.rs`

**Interfaces:**

- Consumes: whatever `run_check_stale` (`src/cli/mod.rs:1585`) uses to get
  its "booted" and "current" hash strings.
- Produces: `collect_config_stale` returns `Some(bool)` instead of always
  `None`.

- [ ] **Step 1: Write the failing test**

First, **read** `src/cli/mod.rs` around lines 32-70 and 1585 (`StaleStatus`,
`stale_status`, `run_check_stale`) to find the exact two hash sources being
compared. Then add to `src/materialize/status_data.rs`'s test module:

```rust
#[test]
fn collect_config_stale_returns_fresh_when_hashes_match() {
    // Fill in using the real booted/current hash sources found in
    // src/cli/mod.rs's run_check_stale — this plan can't hardcode the exact
    // env var / file path without having read that function's body live.
    // The assertion shape: constructing a scenario where booted == current
    // must yield `Some(false)`, and booted != current must yield `Some(true)`.
}
```

(This step is deliberately left to the implementer to fill in with the real
mechanism — unlike other tasks in this plan, Task 10's own investigation did
not chase down `run_check_stale`'s internals. This is the one legitimate
exception to "no placeholders": the test *shape* is given, but the exact
setup calls require a live read the plan author didn't perform. Read
`src/cli/mod.rs:1585` first, then write the concrete test before writing the
implementation, per standard TDD — do not skip straight to Step 3.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv status_data::collect_config_stale -- --nocapture`

- [ ] **Step 3: Write minimal implementation**

Replace `collect_config_stale`'s body to call the same `stale_status(...).is_stale()`
mechanism `run_check_stale` uses, sourcing "booted" and "current" the same
way that function does. Return `Some(status.is_stale())`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv status_data:: -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add src/materialize/status_data.rs
git commit -m "feat(statusline): wire up config_stale detection

Fixes #836"
```

---

### Task 11: Write `llmenv-status.json` during materialization

**Files:**

- Modify: `src/cli/mod.rs` (around the materialize orchestration found
  during planning, `src/cli/mod.rs:1267-1290`)

**Interfaces:**

- Consumes: `collect_status_data` (Task 10), `crate::paths::write_owner_only_atomic`
  (already used by `manifest.rs:171-172` — same call shape here).
- Produces: `llmenv-status.json` written into `cache_path` (the materialized
  folder), added to the `owned` path set before `CacheManifest::new(...)` is
  built.

- [ ] **Step 1: Write the failing test**

Add an integration-style test near the existing materialize tests (find the
existing `#[cfg(test)] mod tests` in `src/cli/mod.rs` or wherever
`materialize_with_mode`'s call site is tested — if that orchestration
function itself has no direct test today because it's deeply wired to CLI
argument parsing, add this test instead directly against
`materialize_with_mode` in `src/materialize/mod.rs`, asserting the caller
contract rather than the full CLI path):

```rust
#[test]
fn materialize_writes_status_json_alongside_manifest() {
    // Implementer: build a minimal Manifest + adapter_root via this file's
    // existing test helpers (grep the nearest existing
    // `materialize_with_mode` test for the fixture-construction pattern) and
    // assert `adapter_root/<hash>/llmenv-status.json` exists after the call,
    // deserializes via `StatusData::load`, and its path appears in the
    // written `CacheManifest.owned` set.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv materialize_writes_status_json -- --nocapture`

- [ ] **Step 3: Write minimal implementation**

In `src/cli/mod.rs`, after the `adapter.materialize(...)` call
(around line 1279) and before `write_cache_manifest` (around line 1290),
add:

```rust
    let status_json = {
        let data = crate::materialize::status_data::collect_status_data(
            config,
            active,
            &manifest.throttle,
            &adapter_root,
            config.cache.hashing,
        );
        serde_json::to_string_pretty(&data)?
    };
    let status_path = cache_path.join("llmenv-status.json");
    crate::paths::write_owner_only_atomic(&status_path, status_json.as_bytes())
        .with_context(|| format!("writing {}", status_path.display()))?;
```

Add its relative path to the `owned` set before `CacheManifest::new`:

```rust
    let owned = adapter_owned
        .into_iter()
        .chain(manifest.files.keys().cloned())
        .chain(std::iter::once("llmenv-status.json".to_string()));
```

(Confirm the exact relative-path format `owned` expects — `manifest.files.keys()`
are already `/`-separated relative strings per `CacheManifest`'s doc comment;
`"llmenv-status.json"` at the folder root matches that shape directly, no
prefix needed.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv materialize_writes_status_json -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat(statusline): write llmenv-status.json during materialization

Fixes #836"
```

---

### Task 12: Refresh data file on `llmenv export`

**Files:**

- Modify: `src/cli/mod.rs` (wherever `run_export`/`Command::Export`'s handler
  lives — same file, different function than Task 11's materialize path)

**Interfaces:**

- Consumes: `collect_status_data` (Task 10), same write helper as Task 11.
- Produces: `llmenv export` refreshes `llmenv-status.json` in the current
  materialized folder (throttle/cache/config-staleness fields are the ones
  that change between full materializations, per the design doc's write
  triggers).

- [ ] **Step 1: Write the failing test**

Add near `run_export`'s existing tests (find them via
`rg -n "fn run_export|mod tests" src/cli/mod.rs`):

```rust
#[test]
fn export_refreshes_status_json() {
    // Implementer: mirror the existing run_export test fixture (temp config
    // + temp cache dir), run `run_export(...)`, then assert
    // `<cache_path>/llmenv-status.json` exists and its `ts` field changed
    // versus a pre-seeded stale one written before the call.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv export_refreshes_status_json -- --nocapture`

- [ ] **Step 3: Write minimal implementation**

In `run_export`'s body, after it resolves `cache_path` (mirror whatever
variable name that function already uses for the materialized folder path —
same as `materialize_with_mode`'s `rendered.path` in Task 11), add the same
`collect_status_data` + `write_owner_only_atomic` block from Task 11's Step 3
(extract it into a small private helper `fn write_status_json(cache_path, config, active, throttle, cache_root, hashing) -> anyhow::Result<()>`
in `src/cli/mod.rs` so Tasks 11 and 12 share one implementation instead of
duplicating the block — refactor Task 11's inline code into this helper as
part of this task).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv export_refreshes_status_json -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat(statusline): refresh llmenv-status.json on llmenv export

Fixes #836"
```

---

### Task 13: Write data file once at session start

**Files:**

- Modify: wherever "session start" is currently handled — **find this
  first**: `rg -n "SessionStart|session_start" src/cli/mod.rs src/hook_run/`
  (the design doc says "written once before the engine launches" — this is
  likely the same code path that calls `crate::throttle::store_active_throttle`
  during materialize, i.e. this may already be **fully covered by Task 11**
  if materialization always runs before the engine launches in this repo's
  session lifecycle, making this task a no-op check rather than new code).

**Interfaces:**

- Consumes: `write_status_json` helper (Task 12).
- Produces: confirms (or adds, if a genuine gap exists) that the data file
  exists before the first `llmenv statusline` invocation of a session.

- [ ] **Step 1: Investigate whether this is already covered**

Run: `rg -n "fn.*session.*start|SessionStart" src/cli/mod.rs src/hook_run/mod.rs`

If materialization (Task 11) already runs synchronously before the adapter
launches the engine for every session (check whichever CLI entry point the
adapters actually invoke — e.g. `llmenv run`, `llmenv exec`, or similar
launcher subcommand), **this task is a no-op**: write a one-line test
confirming the data file exists immediately after that launcher subcommand
returns, using the existing materialize test fixtures, and commit just that
test with a comment explaining why no new production code was needed.

- [ ] **Step 2: If a genuine gap exists**, write a failing test for the
specific session-start entry point found in Step 1, following the same
`write_status_json` call pattern as Task 12.

- [ ] **Step 3: Write minimal implementation** (only if Step 1 found a gap)
calling the shared `write_status_json` helper from that entry point.

- [ ] **Step 4: Run test to verify it passes**

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(statusline): ensure llmenv-status.json exists at session start

Fixes #836"
```

---

### Task 14: Claude Code adapter — default statusLine hook

**Files:**

- Modify: `src/adapter/claude_code.rs` (near the `autoMemoryEnabled` pattern,
  `claude_code.rs:1180-1184`, and before the `overlay_native` calls at line
  1204/1223)

**Interfaces:**

- Consumes: nothing new — this is a settings-map insertion, same shape as
  the existing `autoMemoryEnabled` default.
- Produces: `settings.json`'s `statusLine` key defaults to
  `{"type": "command", "command": "llmenv statusline"}` unless
  `native.claude_code.statusLine` is explicitly set (which overlays after,
  per existing `overlay_native` precedence — already covered by the
  existing `reconcile_native_passthrough_written_on_rerender` test).

- [ ] **Step 1: Write the failing test**

Add to `claude_code.rs`'s existing `#[cfg(test)] mod tests` (near the other
`reconcile_settings`/render tests):

```rust
#[test]
fn render_settings_defaults_status_line_to_llmenv_statusline() {
    let manifest = /* build a minimal manifest per this file's existing
        render-test fixture helper — find it via `rg -n "fn build_manifest|fn minimal_manifest"
        in this file's test module and reuse it, do not hand-construct a
        Manifest from scratch */;
    let tmp = tempfile::tempdir().unwrap();
    render_settings(&manifest, tmp.path()).unwrap(); // or whatever this
        // file's actual settings-render entry point is named — confirm via
        // `rg -n "^fn render_settings|^pub fn render"` in this file.
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["statusLine"]["type"], "command");
    assert_eq!(settings["statusLine"]["command"], "llmenv statusline");
}

#[test]
fn native_status_line_override_wins_over_default() {
    // Reuses the exact scenario from
    // `reconcile_native_passthrough_written_on_rerender` (this file,
    // ~line 2429) but asserts it against the *new* default-emitting code
    // path: a manifest with `native.claude_code.statusLine` set must
    // produce that value, not the "llmenv statusline" default.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin llmenv claude_code::render_settings_defaults_status_line -- --nocapture`
Expected: FAIL — no default is emitted yet, `settings["statusLine"]` is
absent.

- [ ] **Step 3: Write minimal implementation**

Add near the `autoMemoryEnabled` block (`claude_code.rs:1180-1184`), **before**
the `overlay_native` calls:

```rust
    // #836: default the statusLine hook to llmenv's own renderer. Emitted
    // before native overlays so `native.claude_code.statusLine` can still
    // override it (same precedence pattern as `autoMemoryEnabled` above).
    settings.insert(
        "statusLine".into(),
        json!({ "type": "command", "command": "llmenv statusline" }),
    );
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin llmenv claude_code:: -- --nocapture`
Expected: PASS, including the pre-existing
`reconcile_native_passthrough_written_on_rerender` test (must still pass
unmodified — confirms the override precedence didn't regress).

- [ ] **Step 5: Commit**

```bash
git add src/adapter/claude_code.rs
git commit -m "feat(statusline): wire Claude Code adapter to llmenv statusline by default

Fixes #836"
```

---

### Task 15: Document the Crush adapter gap (no code change)

**Files:**

- Modify: `docs/superpowers/specs/2026-07-15-statusline-design.md` (add a
  short "Known limitations" note, since the issue's acceptance criteria
  mentions Crush wiring but Crush has no statusline concept today)

**Interfaces:** none — documentation only.

- [ ] **Step 1: Confirm the gap is real**

Run: `rg -n "statusline|statusLine|StatusLine" src/adapter/crush.rs`
Expected: no matches (confirmed during planning — Crush's adapter comment at
`crush.rs:20` states "Crush only supports PreToolUse hooks today", and Crush
has no equivalent settings concept for an engine-invoked statusline
renderer).

- [ ] **Step 2: Add the limitation note**

Append to `docs/superpowers/specs/2026-07-15-statusline-design.md`:

```markdown
## Known limitations

Crush has no statusline concept in its adapter today (`src/adapter/crush.rs`
only supports `PreToolUse` hooks) — there is no engine-invoked renderer hook
to wire `llmenv statusline` into. Claude Code wiring (Task 14) ships in this
PR; Crush support is deferred to a follow-up issue once Crush's own config
format grows a statusline-equivalent concept.
```

- [ ] **Step 3: File the follow-up issue**

```bash
gh issue create \
  --title "feat: wire llmenv statusline into Crush adapter" \
  --body "Deferred from #836 (llmenv statusline). Crush's adapter (src/adapter/crush.rs) has no statusline-equivalent hook concept today — it only supports PreToolUse hooks. Once Crush's own config format grows an engine-invoked status renderer concept, wire \`llmenv statusline\` into it the same way Task 14 wires Claude Code's \`statusLine\` settings key." \
  --milestone "v3.6.0" \
  --label "enhancement"
```

- [ ] **Step 4: Commit the doc change**

```bash
git add docs/superpowers/specs/2026-07-15-statusline-design.md
git commit -m "docs(statusline): note Crush adapter gap, deferred to follow-up issue

Fixes #836"
```

---

### Task 16: User-facing docs

**Files:**

- Modify: `website/docs/` — find the right page via
  `rg -l -i "adapter|hooks|config.yaml" website/docs/*.md | head` and add a
  new page or section for the `statusline:` config, following whichever
  existing doc page documents a comparable config section (e.g. wherever
  `features:`/`throttle:` are documented).

**Interfaces:** none — documentation only.

- [ ] **Step 1: Find the doc structure**

Run: `ls website/docs/` and `rg -l "throttle:" website/docs/*.md` to find the
convention for documenting a top-level `config.yaml` section.

- [ ] **Step 2: Write the statusline doc page**

Add a new page (mirror the closest existing config-section doc's structure:
problem statement, config schema table, example YAML, widget reference
table) covering: the `statusline:` config shape (Task 1), all 18 widgets
(Tasks 5/5b/6) with their default formats, `icon_set` behavior (Task 7),
and the Claude Code adapter default (Task 14) — reuse the widget rendering
table straight from `docs/superpowers/specs/2026-07-15-statusline-design.md`
("Widget rendering table" section) since it's already accurate.

- [ ] **Step 3: Commit**

```bash
git add website/docs/
git commit -m "docs: document llmenv statusline config and widgets

Fixes #836"
```

---

### Task 17: CHANGELOG entry

**Files:**

- Modify: `CHANGELOG.md`

**Interfaces:** none.

- [ ] **Step 1: Add an `[Unreleased]` entry**

Under `## [Unreleased]` → `### Added` (per Keep a Changelog format —
run the `keepachangelog` skill if unsure of exact section placement):

```markdown
- `llmenv statusline` subcommand: a first-class, built-in statusline
  renderer configured via `config.yaml`'s new `statusline:` section,
  replacing the need for an external status-line binary. Renders both
  engine session data (model, context usage, budget) and llmenv-specific
  stats (active scopes, plugin/MCP counts, ICM memory stats, cache health).
  Wired into Claude Code by default; Crush support is deferred (see
  follow-up issue). (#836)
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): note llmenv statusline addition

Fixes #836"
```

---

## Self-Review Notes (from plan authoring)

**Spec coverage:** every acceptance criterion in #836 maps to a task —
subcommand (Task 8), data file + write triggers (Tasks 10-13), config
schema (Task 1), all 18 widgets (Tasks 5, 5b, 6), adapter wiring (Task 14,
with Crush explicitly scoped out in Task 15 rather than silently dropped),
never-crash contract (Tasks 2, 5, 6, 8 all degrade to empty/`None` on bad
input), default single-row config (Task 8), graceful data-file-missing
fallback (Tasks 2, 8).

**Known deviations from the design doc's illustrative examples,
documented inline at point of deviation rather than silently diverging:**

- `throttle` widget source data uses `backend`/`cooldown_secs` (from the
  real `Throttle` config struct) instead of the design doc's illustrative
  `raw`/`reason`/`icon` fields, which have no backing source today (Task 6,
  Task 10).
- Crush adapter wiring is deferred to a filed follow-up issue rather than
  implemented against a hook mechanism Crush doesn't have (Task 15).

**Two tasks (10b, 11's `config_stale` wiring, and portions of 11-13) name
the exact file/line to read rather than the exact code to write**, because
those integration points (`stale_status`'s actual booted/current hash
sources, `run_export`'s exact local variable names, `resolve_mcps`'s
`config.mcp`/`config.memory`/`config.host` argument shapes) require a live
read of code this plan's authoring pass did not fully trace — this is
flagged explicitly at each occurrence rather than guessed, per this
project's "no bug/feature left behind" standard: guessing wrong here would
produce silently-incorrect `config_stale`/`mcps` data, worse than a plan
step that says "read X first."
