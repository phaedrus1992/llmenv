//! Stateless widget renderers. Each function receives complete input and
//! returns a string — no side effects, no shared mutable state (per the
//! design doc's "Separation of concerns").

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod usage_state;
use usage_state::*;

mod git;
use git::*;
mod gh_cmd;
use gh_cmd::GhPrCmd;
mod gh_lookup;
use gh_lookup::{GhFailure, classify_gh_exit};

mod reset_clock;
mod usage;
use usage::render_usage_widget;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EngineData {
    pub workspace: Option<Workspace>,
    pub model: Option<ModelInfo>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    pub rate_limits: Option<RateLimits>,
    pub branch: Option<BranchInfo>,
    pub worktree: Option<Worktree>,
    pub pr: Option<PrInfo>,
    /// The engine conversation id. Claude Code sends it as `session_id` in the status line input.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Parse the status line input. Without a conversation id the status line shows every
/// conversation's sessions, so an unreadable input is logged, not silently widened (#3196).
pub fn parse_engine_data(input: &str) -> EngineData {
    serde_json::from_str(input).unwrap_or_else(|e| {
        tracing::error!(
            "the status line input is not JSON, so every conversation's sessions are shown: {e}. \
             Fix: check the engine's status line input."
        );
        EngineData::default()
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchInfo {
    pub name: Option<String>,
}

/// Present only for Claude Code `--worktree` sessions — the branch of the
/// linked worktree. Regular (non-worktree) sessions have no branch in the
/// stdin JSON at all, so `render_branch` derives it from git instead.
#[derive(Debug, Clone, Deserialize)]
pub struct Worktree {
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PrInfo {
    pub number: Option<u64>,
    pub url: Option<String>,
    pub review_state: Option<String>,
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

/// Claude.ai Pro/Max subscription usage windows, present on stdin only after
/// the first API response in a session; either window may be independently
/// absent. Rendered by the `usage_5h` / `usage_7d` widgets.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<RateLimitWindow>,
    pub seven_day: Option<RateLimitWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitWindow {
    pub used_percentage: Option<f64>,
    /// Unix epoch seconds at which this window's usage resets.
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
    // `pr` is special-cased: it needs the resolved PR for both the text
    // (`render_pr_info`) and the dynamic review-state style
    // (`pr_style_for`). Resolving once here — rather than letting each
    // helper call `resolve_pr` independently — avoids a duplicate cache
    // read + JSON deserialize (and a duplicate `gh` derivation on a cold
    // cache) on every single render.
    if name == "pr" {
        let pr = match resolve_pr(data) {
            Ok(pr) => pr,
            Err(e) => {
                if let PrError::Branch(cause) = &e {
                    tracing::error!("statusline pr cannot read the branch: {cause}");
                }
                let raw = e.label().to_string();
                return Some(super::finish(name, raw, cfg, None, use_color));
            }
        };
        let raw = pr
            .as_ref()
            .map_or_else(String::new, |pr| render_pr_info(pr, cfg, use_color));
        let dyn_style = pr.as_ref().and_then(pr_style_for);
        return Some(super::finish(name, raw, cfg, dyn_style, use_color));
    }
    let raw = match name {
        "model" => render_model(data, cfg),
        "folder" => render_folder(data, cfg),
        "duration" => render_duration(data, cfg),
        "tokens" => render_tokens(data, cfg),
        "budget" => render_budget(data, cfg),
        "cache_usage" => render_cache_usage(data, cfg, use_color),
        "branch" => render_branch(data, cfg, use_color),
        "context" => render_context(data, cfg, use_color),
        "usage_5h" | "usage_7d" => render_usage_widget(name, data, cfg, use_color),
        "peak" => super::peak::render_peak(cfg),
        _ => return None,
    };
    // Every other widget renders with `finish`'s static per-widget style
    // (`context` and the usage widgets color themselves per-cell instead).
    Some(super::finish(name, raw, cfg, None, use_color))
}

/// Map a value to a `green` / `yellow` / `red` style name by two ascending
/// thresholds: `>= crit` red, `>= warn` yellow, else green.
fn threshold_style(value: f64, thresholds: [u8; 2]) -> &'static str {
    if value >= f64::from(thresholds[1]) {
        "red"
    } else if value >= f64::from(thresholds[0]) {
        "yellow"
    } else {
        "green"
    }
}

/// Strip the parts of a model `display_name` that are neither family nor
/// version: a trailing parenthetical qualifier (e.g. `" (1M context)"`) that
/// some Claude Code builds append. Claude Code's `display_name` arrives as
/// bare `"Opus"`, `"Opus 4.8"`, or `"Opus 4.8 (1M context)"` depending on
/// build — the parenthetical is a context-window note, not the model name, and
/// left in place it leaks a stray `"context)"` token into the short name.
fn clean_display_name(display_name: &str) -> &str {
    display_name
        .split('(')
        .next()
        .unwrap_or(display_name)
        .trim()
}

/// Derive a short model family name from Claude's `display_name` (e.g.
/// `"Claude Opus 4.8 (1M context)"` -> `"Opus"`): strips the trailing
/// parenthetical, then drops a leading "claude" token (case-insensitive) and
/// any version-shaped token (containing a digit), leaving just the family
/// name(s) in between.
fn short_model_name(display_name: &str) -> String {
    clean_display_name(display_name)
        .split_whitespace()
        .filter(|tok| {
            !tok.eq_ignore_ascii_case("claude") && !tok.chars().any(|c| c.is_ascii_digit())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The version-shaped token (the first containing a digit) embedded in a
/// display name, e.g. `"4.8"` in `"Opus 4.8 (1M context)"`. Used only as a
/// fallback when the engine sends no separate `version` field — Claude Code
/// embeds the version in `display_name` and provides no `version`, so without
/// this the default `{short_name} {version}` format would always drop the
/// version and render a bare `"Opus"`.
fn version_from_display_name(display_name: &str) -> Option<String> {
    clean_display_name(display_name)
        .split_whitespace()
        .find(|tok| tok.chars().any(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

fn render_model(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(model) = &data.model else {
        return String::new();
    };
    // All three are untrusted engine strings — sanitize before interpolation.
    let display_name = model
        .display_name
        .as_deref()
        .map(super::sanitize)
        .unwrap_or_default();
    let short_name = short_model_name(&display_name);
    let version = match model.version.as_deref() {
        Some(v) => super::sanitize(v),
        None => version_from_display_name(&display_name).unwrap_or_default(),
    };
    // Precedence: an explicit `format` wins; else a named `display` mode
    // (`short`/`version`/`full`); else the default `{short_name} {version}`.
    if let Some(format) = cfg.and_then(|c| c.format.as_deref()) {
        let full_name = model
            .full_name
            .as_deref()
            .map(super::sanitize)
            .unwrap_or_default();
        return format
            .replace("{short_name}", &short_name)
            .replace("{version}", &version)
            .replace("{full_name}", &full_name)
            .trim()
            .to_string();
    }
    match cfg.and_then(|c| c.display.as_deref()) {
        Some("short") => short_name,
        Some("full") => display_name,
        // "version" and the unset default both show family + version.
        _ => format!("{short_name} {version}").trim().to_string(),
    }
}

fn render_folder(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(path) = data
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
    else {
        return String::new();
    };
    // `path` (and its basename) are untrusted — a directory name can carry an
    // escape sequence. Sanitize both before interpolation.
    let basename = super::sanitize(
        &std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let path = super::sanitize(path);
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("\u{1f4c1} {basename}"); // 📁
    format
        .replace("{basename}", &basename)
        .replace("{path}", &path)
}

/// Renders used-context progress: percent, bar, or both, depending on which
/// placeholders the configured (or default) `format` uses. `remaining_percentage`
/// comes from an external engine's stdin JSON — untrusted. NaN/infinite values
/// render empty rather than a garbled cast result; any other value is clamped
/// to `0.0..=100.0` before display so a corrupt/hostile float (e.g. `1e300`)
/// can't produce an absurd bar/percentage.
fn render_context(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let Some(remaining) = data
        .context_window
        .as_ref()
        .and_then(|c| c.remaining_percentage)
    else {
        return String::new();
    };
    if !remaining.is_finite() {
        return String::new();
    }
    let used = (100.0 - remaining).clamp(0.0, 100.0);
    render_pct_and_bar(
        used,
        cfg,
        "{pct}% {bar}",
        // self-colored (threshold), like the old `progress_bar` — see `finish`'s `default_style`
        BarStyle::Threshold {
            thresholds: [50, 80],
            marker: None,
        },
        use_color,
    )
}

fn render_duration(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(ms) = data.cost.as_ref().and_then(|c| c.total_duration_ms) else {
        return String::new();
    };
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    // A custom `format` uses placeholders (`{s}` = total seconds, for
    // backward compatibility). The default mirrors the reference tool: show
    // hours+minutes past an hour, minutes+seconds under, seconds only under a
    // minute — so a short session reads `45s`, not `0h0m`.
    if let Some(format) = cfg.and_then(|c| c.format.as_deref()) {
        return format
            .replace("{h}", &h.to_string())
            .replace("{m}", &m.to_string())
            .replace("{s}", &total_secs.to_string())
            .replace("{total_ms}", &ms.to_string());
    }
    let body = if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    };
    format!("\u{23f1} {body}") // ⏱
}

/// Sum of the three token-count fields, saturating on overflow. Each field
/// is an untrusted `u64` from the engine's stdin JSON — a plain `+` could
/// overflow-panic (debug) or wrap (release) if the engine sends
/// near-`u64::MAX` values.
fn total_tokens(usage: &TokenUsage) -> u64 {
    usage
        .input_tokens
        .unwrap_or(0)
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0))
        .saturating_add(usage.cache_read_input_tokens.unwrap_or(0))
}

fn render_tokens(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(usage) = data
        .context_window
        .as_ref()
        .and_then(|c| c.current_usage.as_ref())
    else {
        return String::new();
    };
    let total = format_token_count(total_tokens(usage));
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{total}");
    format
        .replace("{total}", &total)
        .replace(
            "{input}",
            &format_token_count(usage.input_tokens.unwrap_or(0)),
        )
        .replace(
            "{cache_read}",
            &format_token_count(usage.cache_read_input_tokens.unwrap_or(0)),
        )
        .replace(
            "{cache_create}",
            &format_token_count(usage.cache_creation_input_tokens.unwrap_or(0)),
        )
}

/// Humanize a token count: `m` suffix at a million, `k` at a thousand, bare
/// below. A trailing `.0` is dropped so round values read `"1m"` / `"200k"`
/// (not `"1.0m"` / `"200.0k"`) — the context-window max is always round, so
/// the budget's right side stays decimal-free, while a fractional used count
/// still shows one place (`"109.2k"`).
fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        trim_unit(n as f64 / 1_000_000.0, 'm')
    } else if n >= 1000 {
        trim_unit(n as f64 / 1000.0, 'k')
    } else {
        n.to_string()
    }
}

/// One-decimal value with `suffix`, dropping a redundant trailing `.0`.
fn trim_unit(value: f64, suffix: char) -> String {
    let s = format!("{value:.1}");
    format!("{}{suffix}", s.strip_suffix(".0").unwrap_or(&s))
}

fn render_budget(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(cw) = &data.context_window else {
        return String::new();
    };
    let Some(max) = cw.context_window_size else {
        return String::new();
    };
    let used = cw.current_usage.as_ref().map_or(0, total_tokens);
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{used}/{max}");
    format
        .replace("{used}", &format_token_count(used))
        .replace("{max}", &format_token_count(max))
}

/// Renders cache-hit-ratio progress: percent, bar, or both, like [`render_context`].
/// Unlike context/token usage, a *high* cache percentage is good (more cache
/// hits, cheaper), not a warning level — so this doesn't self-color by
/// threshold; it keeps its plain default appearance (icon + percent, no
/// bar), matching its behavior before `{bar}` support was added.
fn render_cache_usage(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let Some(usage) = data
        .context_window
        .as_ref()
        .and_then(|c| c.current_usage.as_ref())
    else {
        return String::new();
    };
    let cache = usage
        .cache_read_input_tokens
        .unwrap_or(0)
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
    let total = usage.input_tokens.unwrap_or(0).saturating_add(cache);
    if total == 0 {
        return String::new();
    }
    let used = cache as f64 / total as f64 * 100.0;
    render_pct_and_bar(used, cfg, "\u{21bb}{pct}%", BarStyle::Plain, use_color) // ↻
}

/// Format an already-resolved [`PrInfo`] (engine-supplied or derived via
/// [`resolve_pr`]) into the `pr` widget's text.
fn render_pr_info(
    pr: &PrInfo,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let Some(number) = pr.number else {
        return String::new();
    };
    let url = pr.url.as_deref().map(super::sanitize).unwrap_or_default();
    let review_state = pr
        .review_state
        .as_deref()
        .map(super::sanitize)
        .unwrap_or_default();
    // `display: url` shows the full URL (falling back to `#<number>` when the
    // engine didn't send one); default/`number` shows `#<number>`. An explicit
    // `format` overrides both.
    let default_format = match cfg.and_then(|c| c.display.as_deref()) {
        Some("url") if !url.is_empty() => "{url}",
        _ => "#{number}",
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or(default_format);
    let label = format
        .replace("{number}", &number.to_string())
        .replace("{url}", &url)
        .replace("{review_state}", &review_state);
    // Link the label to the PR when we have a safe URL (OSC 8), gated on color
    // so piped / non-TTY output stays plain.
    if use_color && super::valid_url(&url) {
        super::hyperlink(&label, &url)
    } else {
        label
    }
}

/// Color the `pr` widget by review state: approved green, changes-requested
/// red, pending yellow; `None` (default `bold magenta`) otherwise. Takes the
/// already-resolved [`PrInfo`] — [`render_engine_widget`] resolves it once
/// via [`resolve_pr`] and shares it with [`render_pr_info`], so a derived
/// PR's review state colors the widget exactly like an engine-supplied one
/// would, without a second lookup.
fn pr_style_for(pr: &PrInfo) -> Option<&'static str> {
    match pr.review_state.as_deref()? {
        "approved" => Some("green"),
        "changes_requested" => Some("red"),
        "pending" | "review_required" => Some("yellow"),
        _ => None,
    }
}

/// How long a derived `gh pr view` result stays cached before re-querying.
/// The statusline re-execs on every prompt — without this, an unauthenticated
/// or slow `gh` would run (or fail) on every single render. 60s is short
/// enough that opening/merging/closing a PR shows up within about a prompt or
/// two, long enough that a fast typing session doesn't repeat the subprocess
/// call on every render.
const PR_CACHE_TTL_SECS: i64 = 60;

/// Resolve the PR for the current session, in precedence order:
///
/// 1. `data.pr` from stdin — forward-compat if an engine ever sends one
///    directly (Claude Code currently doesn't).
/// 2. Derived via `gh pr view` for the branch [`resolve_branch_name`]
///    resolves, cached with a short TTL (see [`PR_CACHE_TTL_SECS`]) keyed by
///    (repo directory, branch) in the same state directory the `usage_5h`/
///    `usage_7d` delta tracking uses.
///
/// `None` whenever the derivation can't proceed or `gh` can't produce a PR —
/// no workspace directory, no resolvable branch (including detached HEAD),
/// `gh` not installed, not authenticated, no remote, or no open PR for the
/// branch. All of these degrade silently, matching [`render_branch`]'s
/// fallback.
fn resolve_pr(data: &EngineData) -> Result<Option<PrInfo>, PrError> {
    resolve_pr_with(data, GhPrCmd::new("gh"))
}

/// Why the PR widget has no answer. Each case has its own label, so a failure does not look
/// like "no open PR" (#2749, #2821).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PrError {
    /// The branch cannot be read.
    Branch(String),
    /// `gh` gave no answer.
    Gh(GhFailure),
}

impl PrError {
    /// The text the widget shows for this error.
    fn label(&self) -> &'static str {
        match self {
            Self::Branch(_) => "pr: unreadable",
            Self::Gh(failure) => failure.label(),
        }
    }
}

/// The result of one PR lookup: the PR, `None` for no open PR, or why `gh` gave no answer.
type PrLookup = Result<Option<PrInfo>, GhFailure>;

/// [`resolve_pr`]'s backend, with the `gh` invocation injectable so tests can
/// exercise the full engine-precedence + branch-resolution + derivation chain
/// against a fake `gh` instead of the real one.
///
/// A branch read error is returned, not hidden, so the widget can show it (#2749).
fn resolve_pr_with(data: &EngineData, gh: GhPrCmd<'_>) -> Result<Option<PrInfo>, PrError> {
    if let Some(pr) = &data.pr {
        return Ok(Some(pr.clone()));
    }
    let Some(dir) = data
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
    else {
        return Ok(None);
    };
    let Some(branch) = resolve_branch_name(data).map_err(PrError::Branch)? else {
        return Ok(None);
    };
    derive_pr(
        gh,
        Path::new(dir),
        &branch,
        usage_state_dir().as_deref(),
        now_unix(),
    )
    .map_err(PrError::Gh)
}

/// Cache-then-derive backend for [`resolve_pr`]'s fallback path, fully
/// parameterized (`gh`, `cache_dir`, `now`) so tests can substitute a fake
/// `gh` and a controlled clock without touching the real network or process
/// environment.
///
/// A `gh` failure is cached too, with its cause. Without the cache a timeout would stall every
/// render, and the cause keeps the failure apart from "no open PR" (#2821).
fn derive_pr(
    gh: GhPrCmd<'_>,
    repo_dir: &Path,
    branch: &str,
    cache_dir: Option<&Path>,
    now: i64,
) -> PrLookup {
    let cache_path = cache_dir.map(|dir| pr_cache_path(dir, repo_dir, branch));
    if let Some(path) = &cache_path
        && let Some(cached) = read_pr_cache(path, now)
    {
        return cached;
    }
    let fresh = gh_pr_view(gh, repo_dir, branch);
    if let Some(path) = &cache_path {
        write_pr_cache(path, &fresh, now);
    }
    fresh
}

/// Cache file path for a (repo, branch) key: `<cache_dir>/pr-cache-<hash>`.
/// Hashed (not a sanitized literal path) so an arbitrary repo directory or
/// branch name — including one with path separators or other characters
/// invalid in a filename — always yields a safe, collision-resistant filename.
fn pr_cache_path(cache_dir: &Path, repo_dir: &Path, branch: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(repo_dir.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(branch.as_bytes());
    cache_dir.join(format!("pr-cache-{}", hex::encode(hasher.finalize())))
}

/// On-disk shape of a cached PR lookup: the Unix timestamp it was written and
/// the result (`None` caches "no open PR" too, so that outcome doesn't
/// retrigger a `gh` call on every render either). `failure` names why `gh` gave
/// no answer; an entry from an older version has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PrCacheEntry {
    ts: i64,
    pr: Option<PrInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<GhFailure>,
}

/// Read a still-fresh cache entry. `None` for a missing, corrupt, unreadable, or
/// expired entry; each falls through to a fresh `gh` call in [`derive_pr`]. A read
/// error other than a missing file is logged (#2821).
fn read_pr_cache(path: &Path, now: i64) -> Option<PrLookup> {
    let contents = match crate::paths::bounded::state_text_io(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::error!(
                "pr cache {} cannot be read, so gh runs: {e}",
                path.display()
            );
            return None;
        }
    };
    let entry: PrCacheEntry = match serde_json::from_str(&contents) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::error!("pr cache {} is corrupt, so gh runs: {e}", path.display());
            return None;
        }
    };
    let lookup = match entry.failure {
        Some(failure) => Err(failure),
        None => Ok(entry.pr),
    };
    (now.saturating_sub(entry.ts) < PR_CACHE_TTL_SECS).then_some(lookup)
}

/// Best-effort cache write: a failed write only means the next render
/// re-queries `gh`, not a broken widget.
fn write_pr_cache(path: &Path, lookup: &PrLookup, now: i64) {
    if let Some(parent) = path.parent()
        && let Err(e) = crate::paths::create_dir_owner_only(parent)
    {
        tracing::error!("pr cache dir {} unavailable: {e}", parent.display());
        return;
    }
    let entry = PrCacheEntry {
        ts: now,
        pr: lookup.clone().unwrap_or_default(),
        failure: lookup.as_ref().err().copied(),
    };
    let json = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            tracing::error!(
                "pr cache entry for {} cannot be serialized: {e}",
                path.display()
            );
            return;
        }
    };
    if let Err(e) = crate::paths::write_owner_only(path, json.as_bytes()) {
        tracing::error!("pr cache write to {} failed: {e}", path.display());
    }
}

/// Shell out to `gh pr view --json number,url,reviewDecision -- <branch>` in
/// `repo_dir` and parse the result. `Ok(None)` for no open PR, no remote, or
/// unparseable output. A missing binary, a failed login, a timeout
/// (`gh.timeout`), and a failed read or wait are failures, so the widget can name
/// them instead of showing "no PR" (#2821). This never prints or panics.
fn gh_pr_view(gh: GhPrCmd<'_>, repo_dir: &Path, branch: &str) -> PrLookup {
    // The repository is the working folder, so a bare `gh` could run a copy it holds (#2948).
    let Ok(program) = crate::path_lookup::resolve_program(gh.bin) else {
        return Err(GhFailure::Missing);
    };
    // `--` terminates option parsing so `branch` (derived from git state) is
    // always read as the positional argument, never as a `gh` flag. The flags
    // stay before `--`; only the positional goes after it.
    let build = || {
        let mut cmd = Command::new(&program);
        cmd.args([
            "pr",
            "view",
            "--json",
            "number,url,reviewDecision",
            "--",
            branch,
        ])
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        cmd
    };
    let run = || crate::process::run_traced("statusline:gh-pr-view", build(), gh.timeout);
    // One retry after a brief backoff (#1062): a failure to start under heavy
    // concurrent process load is transient, not "gh isn't installed".
    // A timeout is final, and a missing binary fails the retry too.
    let out = match run() {
        Err(e) if !matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::NotFound) => {
            // The first failure is logged here, so a retry that works still leaves a trace (#3036).
            tracing::warn!("gh pr view failed, so it is retried once: {e}");
            std::thread::sleep(Duration::from_millis(20));
            run().inspect_err(|second| {
                tracing::error!(
                    "gh pr view failed twice: {e}, then {second}. Fix: run `gh pr view` by hand \
                     in this folder to see the cause."
                );
            })
        }
        other => other,
    };
    match out {
        Ok(out) if out.status.success() => Ok(parse_gh_pr_view(&out.stdout)),
        Ok(out) => classify_gh_exit(out.status, &String::from_utf8_lossy(&out.stderr)),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            tracing::debug!("gh pr view: gh is not installed: {e}");
            Err(GhFailure::Missing)
        }
        Err(e) if e.kind() == ErrorKind::TimedOut => {
            let secs = gh.timeout.as_secs_f32();
            tracing::error!(
                "gh pr view timed out after {secs}s. Fix: run `gh pr view` by hand in this \
                 folder, and check that GitHub is reachable."
            );
            Err(GhFailure::Failed)
        }
        Err(e) => {
            tracing::error!("gh pr view: {e}");
            Err(GhFailure::Failed)
        }
    }
}

/// Parse `gh pr view --json number,url,reviewDecision`'s stdout into a
/// [`PrInfo`], mapping `gh`'s `reviewDecision` (`"APPROVED"` /
/// `"CHANGES_REQUESTED"` / `"REVIEW_REQUIRED"` / absent) onto the same
/// lowercase `review_state` values an engine-supplied PR uses.
fn parse_gh_pr_view(stdout: &[u8]) -> Option<PrInfo> {
    #[derive(Deserialize)]
    struct GhPrView {
        number: Option<u64>,
        url: Option<String>,
        #[serde(rename = "reviewDecision")]
        review_decision: Option<String>,
    }
    let parsed: GhPrView = match serde_json::from_slice(stdout) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                "gh pr view printed JSON that does not parse, so the PR is unknown: {e}. Fix: run `gh pr view` by hand in this folder."
            );
            return None;
        }
    };
    parsed.number?;
    Some(PrInfo {
        number: parsed.number,
        url: parsed.url,
        review_state: parsed
            .review_decision
            .as_deref()
            .and_then(map_review_decision)
            .map(str::to_string),
    })
}

/// Map `gh`'s `reviewDecision` enum values to the lowercase snake_case
/// `review_state` strings [`pr_style_for`] matches on. `gh` never emits a
/// `"pending"` decision (that value exists only for a possible future
/// engine-supplied `review_state`) — an absent/unrecognized decision maps to
/// `None`, rendering with the widget's default color.
fn map_review_decision(decision: &str) -> Option<&'static str> {
    match decision {
        "APPROVED" => Some("approved"),
        "CHANGES_REQUESTED" => Some("changes_requested"),
        "REVIEW_REQUIRED" => Some("review_required"),
        _ => None,
    }
}

/// Resolve a widget's bar cell width: the configured `width`, or
/// `default_width` when unset. Shared by every percentage-based widget
/// (`context`, `cache_usage`, `usage_5h`, `usage_7d`) so `width:` behaves
/// identically everywhere a bar renders.
fn resolve_bar_width(cfg: Option<&llmenv_config::WidgetConfig>, default_width: u8) -> usize {
    cfg.and_then(|c| c.width).unwrap_or(default_width) as usize
}

/// Styling for [`pct_and_bar`]'s percent+bar rendering, bundling the trio of
/// knobs that only ever vary together. `Threshold` colors both the percent
/// text and filled bar cells by `thresholds` (empty cells dim) — the caller
/// must exclude this widget's name from `finish`'s `default_style` to avoid
/// double-coloring — with an optional bright pace-target `marker` column
/// (`usage_5h`/`usage_7d` only, via [`colored_bar`]; every other caller uses
/// `None`). `Plain` renders an unstyled percent and a plain [`block_bar`],
/// leaving `finish`'s static per-widget style as the only coloring applied.
enum BarStyle {
    Plain,
    Threshold {
        thresholds: [u8; 2],
        marker: Option<usize>,
    },
}

/// Shared percent+bar backend for every percentage-based widget (`context`,
/// `cache_usage`, `usage_5h`, `usage_7d`): returns a `(pct_str, bar)` pair
/// for a `0.0..=100.0` used-percentage at the given (already-resolved, see
/// [`resolve_bar_width`]) `width`. `used` must already be a finite, clamped
/// `0.0..=100.0` value — callers guard their own NaN/infinite/missing-data
/// cases before calling this.
fn pct_and_bar(used: f64, width: usize, style: &BarStyle, use_color: bool) -> (String, String) {
    let pct = used.round() as i64;
    match *style {
        BarStyle::Threshold { thresholds, marker } => {
            let color = threshold_style(used, thresholds);
            (
                crate::cli::style::apply_style(&pct.to_string(), color, use_color),
                colored_bar(used, width, color, marker, use_color),
            )
        }
        BarStyle::Plain => (pct.to_string(), block_bar(used, width)),
    }
}

/// `{pct}`/`{bar}`-placeholder rendering for [`render_context`] and
/// [`render_cache_usage`]: substitutes both into `format` (falling back to
/// `default_format` when the widget has no custom `format` configured), so a
/// custom format can show either placeholder alone or both together. Built
/// on the same [`pct_and_bar`] backend `usage_5h`/`usage_7d` use.
/// `default_style`'s `thresholds` (if `Threshold`) are overridden by a
/// configured `thresholds:`; its `marker` is always `None` here — only
/// `render_usage` ever sets one.
fn render_pct_and_bar(
    used: f64,
    cfg: Option<&llmenv_config::WidgetConfig>,
    default_format: &str,
    default_style: BarStyle,
    use_color: bool,
) -> String {
    let width = resolve_bar_width(cfg, DEFAULT_BAR_WIDTH);
    let style = match default_style {
        BarStyle::Threshold { thresholds, marker } => BarStyle::Threshold {
            thresholds: cfg.and_then(|c| c.thresholds).unwrap_or(thresholds),
            marker,
        },
        BarStyle::Plain => BarStyle::Plain,
    };
    let (pct_str, bar) = pct_and_bar(used, width, &style, use_color);
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or(default_format);
    format.replace("{pct}", &pct_str).replace("{bar}", &bar)
}

/// A `width`-cell block bar (filled `▓`, empty `░`) for a `0..=100`
/// percentage. Truncates (not rounds) the filled cell count: rounding would
/// bump a borderline value like 35.0 up to one more filled cell than the
/// displayed "35%" label implies.
fn block_bar(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0 * width as f64) as usize).min(width);
    "\u{2593}".repeat(filled) + &"\u{2591}".repeat(width - filled)
}

/// Default `context`/`cache_usage`/usage-bar width in cells.
const DEFAULT_BAR_WIDTH: u8 = 10;

/// A `width`-cell bar with each cell independently colored: filled cells in
/// `filled_style`, empty cells dim, and an optional bright pace-target marker
/// (`│`) at `marker`. Each cell carries its own SGR reset so colors don't
/// bleed (matching the reference tool). With `use_color` off and no marker,
/// degrades to the plain [`block_bar`] glyphs.
fn colored_bar(
    pct: f64,
    width: usize,
    filled_style: &str,
    marker: Option<usize>,
    use_color: bool,
) -> String {
    if !use_color && marker.is_none() {
        return block_bar(pct, width);
    }
    let filled = ((pct / 100.0 * width as f64) as usize).min(width);
    (0..width)
        .map(|i| {
            if marker == Some(i) {
                crate::cli::style::apply_style("\u{2502}", "bold white", use_color) // │
            } else if i < filled {
                crate::cli::style::apply_style("\u{2593}", filled_style, use_color)
            } else {
                crate::cli::style::apply_style("\u{2591}", "dim", use_color)
            }
        })
        .collect()
}

/// Current wall-clock time as Unix epoch seconds, for computing time-until a
/// rate-limit window resets. Falls back to 0 on a pre-epoch clock (which just
/// yields a "reset now" reading, never a panic).
fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod gh_tests;

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[cfg(test)]
mod tests;
