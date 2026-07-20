//! Stateless widget renderers. Each function receives complete input and
//! returns a string — no side effects, no shared mutable state (per the
//! design doc's "Separation of concerns").

use serde::Deserialize;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Deserialize)]
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
    let raw = match name {
        "model" => render_model(data, cfg),
        "folder" => render_folder(data, cfg),
        "context_pct" => render_context_pct(data, cfg),
        "duration" => render_duration(data, cfg),
        "tokens" => render_tokens(data, cfg),
        "budget" => render_budget(data, cfg),
        "cache_pct" => render_cache_pct(data, cfg),
        "branch" => render_branch(data, cfg, use_color),
        "pr" => render_pr(data, cfg, use_color),
        "progress_bar" => render_progress_bar(data, cfg),
        "usage_5h" => render_usage(
            data.rate_limits.as_ref().and_then(|r| r.five_hour.as_ref()),
            now_unix(),
            FIVE_HOUR_SECS,
            cfg,
            "5h {pct}%{pace} ➡{reset}",
        ),
        "usage_7d" => render_usage(
            data.rate_limits.as_ref().and_then(|r| r.seven_day.as_ref()),
            now_unix(),
            SEVEN_DAY_SECS,
            cfg,
            "7d {pct}%{pace} ➡{reset}",
        ),
        "peak" => super::peak::render_peak(cfg),
        _ => return None,
    };
    // Threshold-colored widgets supply a dynamic style; `finish` applies it
    // unless the user set an explicit per-widget `style`.
    let user_thresholds = cfg.and_then(|c| c.thresholds);
    let dyn_style = match name {
        "progress_bar" => progress_bar_threshold_style(data, cfg),
        "pr" => pr_review_style(data),
        "usage_5h" => usage_threshold_style(
            data.rate_limits.as_ref().and_then(|r| r.five_hour.as_ref()),
            user_thresholds.unwrap_or([70, 90]),
        ),
        "usage_7d" => usage_threshold_style(
            data.rate_limits.as_ref().and_then(|r| r.seven_day.as_ref()),
            user_thresholds.unwrap_or([60, 80]),
        ),
        _ => None,
    };
    Some(super::finish(name, raw, cfg, dyn_style, use_color))
}

/// Green/yellow/red for the `progress_bar` by used-context percentage against
/// `thresholds` (`[warn, crit]`, default `[50, 80]`). `None` when there's no
/// percentage to color (widget renders empty anyway).
fn progress_bar_threshold_style(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
) -> Option<&'static str> {
    let remaining = data
        .context_window
        .as_ref()
        .and_then(|c| c.remaining_percentage)?;
    if !remaining.is_finite() {
        return None;
    }
    let used = (100.0 - remaining).clamp(0.0, 100.0);
    Some(threshold_style(
        used,
        cfg.and_then(|c| c.thresholds).unwrap_or([50, 80]),
    ))
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

/// Renders the used-context percentage. `remaining_percentage` comes from an
/// external engine's stdin JSON — untrusted. NaN/infinite values render
/// empty rather than a garbled cast result; any other value is clamped to
/// the valid `0.0..=100.0` range before the `i64` cast so a corrupt/hostile
/// float (e.g. `1e300`) can't produce a saturated, absurd display string.
fn render_context_pct(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
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
    let used = (100.0 - remaining).clamp(0.0, 100.0).round() as i64;
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{pct}%");
    format.replace("{pct}", &used.to_string())
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

fn render_cache_pct(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
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
    let pct = (cache as f64 / total as f64 * 100.0).round() as i64;
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("\u{21bb}{pct}%"); // ↻
    format.replace("{pct}", &pct.to_string())
}

/// Resolve the current branch, in precedence order:
///
/// 1. `branch.name` from stdin — forward-compat if the engine ever sends it.
/// 2. `worktree.branch` from stdin — Claude Code's only branch field, present
///    for `--worktree` sessions.
/// 3. Git, derived from `workspace.current_dir` — Claude Code sends **no
///    branch** for a regular repo (confirmed against the statusline docs), so
///    llmenv reads it from `.git/HEAD` rather than leaving the widget blank.
fn render_branch(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    // Precedence: stdin `branch.name` (forward-compat) → `worktree.branch`
    // (Claude Code worktree sessions) → git from `workspace.current_dir`.
    let name = data
        .branch
        .as_ref()
        .and_then(|b| b.name.clone())
        .or_else(|| data.worktree.as_ref().and_then(|w| w.branch.clone()))
        .or_else(|| {
            data.workspace
                .as_ref()
                .and_then(|w| w.current_dir.as_deref())
                .and_then(|dir| git_branch(Path::new(dir)))
        });
    let Some(name) = name else {
        return String::new();
    };
    let name = super::sanitize(&name); // untrusted (stdin / git ref)
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("\u{1f33f} {name}"); // 🌿
    let label = format.replace("{name}", &name);
    // Link the branch to its PR when the session carries one (OSC 8).
    let pr_url = data
        .pr
        .as_ref()
        .and_then(|p| p.url.as_deref())
        .map(super::sanitize)
        .unwrap_or_default();
    if use_color && super::valid_url(&pr_url) {
        super::hyperlink(&label, &pr_url)
    } else {
        label
    }
}

/// Current branch by reading `.git/HEAD` directly (no `git` subprocess — the
/// statusline re-renders on every UI tick, and the codebase already avoids
/// per-render forks). Returns `None` for a detached HEAD or when no repository
/// encloses `dir`.
fn git_branch(dir: &Path) -> Option<String> {
    let git_dir = find_git_dir(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// Locate the git directory enclosing `start`, walking up its ancestors.
/// Handles both a `.git` directory (normal clone) and a `.git` *file* holding
/// `gitdir: <path>` (a linked worktree or submodule).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return Some(dot_git);
        }
        // `.git` file: `gitdir: <path>` pointer (worktree/submodule).
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = content.trim().strip_prefix("gitdir: ")?;
        return Some(PathBuf::from(gitdir));
    }
    None
}

fn render_pr(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let Some(pr) = &data.pr else {
        return String::new();
    };
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
/// red, pending yellow; `None` (default `bold magenta`) otherwise.
fn pr_review_style(data: &EngineData) -> Option<&'static str> {
    match data.pr.as_ref().and_then(|p| p.review_state.as_deref())? {
        "approved" => Some("green"),
        "changes_requested" => Some("red"),
        "pending" | "review_required" => Some("yellow"),
        _ => None,
    }
}

/// 10-cell block bar. `used` (100 - remaining) is the displayed percentage.
///
/// `remaining_percentage` comes from an external engine's stdin JSON —
/// untrusted, same field `render_context_pct` guards. NaN survives
/// `f64::clamp` unchanged (NaN comparisons are always false), so it must be
/// rejected explicitly before the round/cast rather than relying on clamp
/// alone; infinite values are rejected for the same reason.
fn render_progress_bar(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
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
    let width = cfg.and_then(|c| c.width).unwrap_or(DEFAULT_BAR_WIDTH);
    let bar = block_bar(used, width as usize);
    let pct = used.round() as i64;
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{pct}% {bar}");
    format
        .replace("{pct}", &pct.to_string())
        .replace("{bar}", &bar)
}

/// A `width`-cell block bar (filled `▓`, empty `░`) for a `0..=100`
/// percentage. Truncates (not rounds) the filled cell count: rounding would
/// bump a borderline value like 35.0 up to one more filled cell than the
/// displayed "35%" label implies.
fn block_bar(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0 * width as f64) as usize).min(width);
    "\u{2593}".repeat(filled) + &"\u{2591}".repeat(width - filled)
}

/// Default `progress_bar` / usage-bar width in cells.
const DEFAULT_BAR_WIDTH: u8 = 10;

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

/// Humanize the seconds from `now` until `resets_at`: `"3h04m"` past an hour,
/// `"23m"` under. A reset already in the past clamps to `"0m"`.
fn humanize_until(resets_at: i64, now: i64) -> String {
    let secs = resets_at.saturating_sub(now).max(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

/// Render a Claude.ai usage window (`five_hour` / `seven_day`). Empty when the
/// window is absent (not a subscriber, or before the first API response) or
/// carries a non-finite percentage. `{pct}` is the clamped, rounded used
/// percentage, `{bar}` a 10-cell fill of it, `{reset}` the time until the
/// window resets.
fn render_usage(
    window: Option<&RateLimitWindow>,
    now: i64,
    window_secs: i64,
    cfg: Option<&llmenv_config::WidgetConfig>,
    default_format: &str,
) -> String {
    let Some(window) = window else {
        return String::new();
    };
    let Some(pct) = window.used_percentage else {
        return String::new();
    };
    if !pct.is_finite() {
        return String::new();
    }
    let pct = pct.clamp(0.0, 100.0);
    let reset = window
        .resets_at
        .map(|r| humanize_until(r, now))
        .unwrap_or_default();
    let pace = window
        .resets_at
        .map(|r| pace_indicator(pct, r, window_secs, now))
        .unwrap_or_default();
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or(default_format);
    format
        .replace("{pct}", &(pct.round() as i64).to_string())
        .replace("{bar}", &block_bar(pct, DEFAULT_BAR_WIDTH as usize))
        .replace("{reset}", &reset)
        .replace("{pace}", &pace)
}

/// Over/under-pace indicator for a usage window: `⇡N%` when usage is ahead of
/// the time elapsed in the window (burning too fast), `⇣N%` when behind,
/// empty within ±0.5%. Each carries a leading space so it drops out cleanly
/// when absent. `target` is the percentage you'd be at if usage were linear
/// across the window.
fn pace_indicator(current_pct: f64, resets_at: i64, window_secs: i64, now: i64) -> String {
    if window_secs <= 0 {
        return String::new();
    }
    let remaining = (resets_at - now).clamp(0, window_secs);
    let target = (window_secs - remaining) as f64 / window_secs as f64 * 100.0;
    let over = current_pct - target;
    let abs = over.abs().round() as i64;
    if over > 0.5 {
        format!(" \u{21e1}{abs}%") // ⇡
    } else if over < -0.5 {
        format!(" \u{21e3}{abs}%") // ⇣
    } else {
        String::new()
    }
}

/// Threshold color for a usage window by its used percentage.
fn usage_threshold_style(
    window: Option<&RateLimitWindow>,
    thresholds: [u8; 2],
) -> Option<&'static str> {
    let pct = window.and_then(|w| w.used_percentage)?;
    if !pct.is_finite() {
        return None;
    }
    Some(threshold_style(pct.clamp(0.0, 100.0), thresholds))
}

/// Window durations in seconds for the pace calculation.
const FIVE_HOUR_SECS: i64 = 5 * 3600;
const SEVEN_DAY_SECS: i64 = 7 * 86_400;

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
            },
            "branch": { "name": "main" },
            "pr": { "number": 42 }
        }))
        .unwrap()
    }

    #[test]
    fn renders_model_default_format() {
        // engine_data()'s fixture display_name is "Claude Opus 4.8" with no
        // separate version field — short_name strips the "Claude" prefix and
        // the "4.8" token, and the version falls back to that same "4.8" token
        // parsed out of display_name, so the default format shows both.
        let out = render_engine_widget("model", &engine_data(), None, false).unwrap();
        assert_eq!(out, "Opus 4.8");
    }

    #[test]
    fn renders_model_strips_trailing_parenthetical() {
        // Regression: a "(1M context)" suffix used to leak a "context)" token
        // into the short name ("Opus context)"). The parenthetical is stripped
        // and the version parsed from display_name (no separate version field).
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "model": { "display_name": "Opus 4.8 (1M context)" }
        }))
        .unwrap();
        let out = render_engine_widget("model", &data, None, false).unwrap();
        assert_eq!(out, "Opus 4.8");
    }

    #[test]
    fn short_model_name_and_version_split_a_parenthetical_display_name() {
        assert_eq!(short_model_name("Opus 4.8 (1M context)"), "Opus");
        assert_eq!(
            version_from_display_name("Opus 4.8 (1M context)").as_deref(),
            Some("4.8")
        );
    }

    #[test]
    fn renders_model_default_format_includes_version_field() {
        // Isolates {version} specifically: renders_model_default_format's
        // fixture has no separate `version` field, so a mutant swapping which
        // field feeds {short_name} vs {version} would go uncaught there.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "model": { "display_name": "Claude Opus 4.8", "version": "4.8" }
        }))
        .unwrap();
        let out = render_engine_widget("model", &data, None, false).unwrap();
        assert_eq!(out, "Opus 4.8");
    }

    #[test]
    fn short_model_name_strips_claude_prefix_and_version() {
        assert_eq!(short_model_name("Claude Opus 4.8"), "Opus");
        assert_eq!(short_model_name("Claude Sonnet 5"), "Sonnet");
        assert_eq!(short_model_name("GPT-Z"), "GPT-Z");
        assert_eq!(short_model_name(""), "");
    }

    #[test]
    fn renders_folder_from_workspace_basename() {
        let out = render_engine_widget("folder", &engine_data(), None, false).unwrap();
        assert_eq!(out, "\u{1f4c1} llmenv");
    }

    #[test]
    fn renders_context_pct() {
        let out = render_engine_widget("context_pct", &engine_data(), None, false).unwrap();
        assert_eq!(out, "35%"); // 100 - remaining_percentage(65) = 35% used
    }

    #[test]
    fn renders_duration_hms() {
        let out = render_engine_widget("duration", &engine_data(), None, false).unwrap();
        assert_eq!(out, "\u{23f1} 3h 42m"); // ⏱ 3h42m (13_320_000 ms)
    }

    #[test]
    fn renders_duration_minutes_and_seconds_under_an_hour() {
        let data = EngineData {
            cost: Some(Cost {
                total_duration_ms: Some(150_000), // 2m30s
            }),
            ..Default::default()
        };
        assert_eq!(
            render_engine_widget("duration", &data, None, false).unwrap(),
            "\u{23f1} 2m 30s"
        );
    }

    #[test]
    fn renders_duration_seconds_only_under_a_minute() {
        let data = EngineData {
            cost: Some(Cost {
                total_duration_ms: Some(45_000), // 45s
            }),
            ..Default::default()
        };
        assert_eq!(
            render_engine_widget("duration", &data, None, false).unwrap(),
            "\u{23f1} 45s"
        );
    }

    #[test]
    fn model_display_mode_short_and_full() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "model": { "display_name": "Claude Opus 4.8 (1M context)" }
        }))
        .unwrap();
        let short = llmenv_config::WidgetConfig {
            display: Some("short".to_string()),
            ..Default::default()
        };
        let full = llmenv_config::WidgetConfig {
            display: Some("full".to_string()),
            ..Default::default()
        };
        assert_eq!(render_model(&data, Some(&short)), "Opus");
        assert_eq!(
            render_model(&data, Some(&full)),
            "Claude Opus 4.8 (1M context)"
        );
        // Unset display → default family + version.
        assert_eq!(render_model(&data, None), "Opus 4.8");
    }

    #[test]
    fn renders_tokens_default_format() {
        let out = render_engine_widget("tokens", &engine_data(), None, false).unwrap();
        // total_tokens = input_tokens(5000) + cache_creation_input_tokens(1000)
        // + cache_read_input_tokens(4000) = 10000; format_token_count(10000):
        // 10000 / 1000 = 10.0, trailing ".0" dropped -> "10k".
        assert_eq!(out, "10k");
    }

    #[test]
    fn renders_budget_default_format() {
        let out = render_engine_widget("budget", &engine_data(), None, false).unwrap();
        // used = total_tokens(same fixture) = 10000 -> "10k"; max =
        // context_window_size(200_000) -> format_token_count(200_000) = "200k"
        // (round values drop the trailing ".0"); default format is "{used}/{max}".
        assert_eq!(out, "10k/200k");
    }

    #[test]
    fn renders_budget_uses_m_suffix_for_million_context_window() {
        // A 1M context window renders "1m", not "1000.0k".
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": {
                "context_window_size": 1_000_000,
                "current_usage": { "input_tokens": 9200, "cache_read_input_tokens": 100_000 }
            }
        }))
        .unwrap();
        let out = render_engine_widget("budget", &data, None, false).unwrap();
        assert_eq!(out, "109.2k/1m");
    }

    #[test]
    fn renders_cache_pct_default_format() {
        let out = render_engine_widget("cache_pct", &engine_data(), None, false).unwrap();
        // cache = cache_read(4000) + cache_creation(1000) = 5000;
        // total = input(5000) + cache(5000) = 10000; pct = round(5000/10000*100) = 50.
        assert_eq!(out, "\u{21bb}50%");
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

    #[test]
    fn context_pct_clamps_absurdly_large_remaining_percentage() {
        // A corrupt/hostile engine sending remaining_percentage: 1e300 must
        // not produce a saturated i64-cast garbage string.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 1e300 }
        }))
        .unwrap();
        let out = render_engine_widget("context_pct", &data, None, false).unwrap();
        assert_eq!(out, "0%");
    }

    #[test]
    fn context_pct_clamps_absurdly_negative_remaining_percentage() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": -1e300 }
        }))
        .unwrap();
        let out = render_engine_widget("context_pct", &data, None, false).unwrap();
        assert_eq!(out, "100%");
    }

    #[test]
    fn context_pct_renders_empty_for_nan_and_infinite() {
        // serde_json can't represent NaN/Infinity literally, so build the
        // struct directly rather than round-tripping through JSON.
        let nan_data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(f64::NAN),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("context_pct", &nan_data, None, false).unwrap();
        assert_eq!(out, "");

        let inf_data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(f64::INFINITY),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("context_pct", &inf_data, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn renders_branch_name() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "branch": { "name": "release/3.x" }
        }))
        .unwrap();
        assert_eq!(
            render_engine_widget("branch", &data, None, false).unwrap(),
            "\u{1f33f} release/3.x"
        );
    }

    #[test]
    fn branch_falls_back_to_worktree_branch_when_no_branch_field() {
        // Claude Code `--worktree` sessions carry `worktree.branch` but no
        // top-level `branch`.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "worktree": { "branch": "feat/thing" }
        }))
        .unwrap();
        assert_eq!(
            render_engine_widget("branch", &data, None, false).unwrap(),
            "\u{1f33f} feat/thing"
        );
    }

    #[test]
    fn branch_derived_from_git_head_when_engine_sends_none() {
        // A regular Claude Code session sends no branch at all — only
        // workspace.current_dir. llmenv reads `.git/HEAD` to fill the widget.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git").join("HEAD"),
            "ref: refs/heads/release/3.x\n",
        )
        .unwrap();
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "workspace": { "current_dir": dir.path().to_string_lossy() }
        }))
        .unwrap();
        assert_eq!(
            render_engine_widget("branch", &data, None, false).unwrap(),
            "\u{1f33f} release/3.x"
        );
    }

    #[test]
    fn branch_empty_for_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        // Detached HEAD: a raw sha, no `ref:` line.
        std::fs::write(
            dir.path().join(".git").join("HEAD"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();
        assert_eq!(git_branch(dir.path()), None);
    }

    #[test]
    fn find_git_dir_follows_gitfile_pointer() {
        // A linked worktree has a `.git` *file* pointing at the real gitdir.
        let dir = tempfile::tempdir().unwrap();
        let real_gitdir = dir.path().join("real-gitdir");
        std::fs::create_dir(&real_gitdir).unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(
            work.join(".git"),
            format!("gitdir: {}\n", real_gitdir.display()),
        )
        .unwrap();
        assert_eq!(find_git_dir(&work), Some(real_gitdir));
    }

    #[test]
    fn renders_pr_number() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 834 }
        }))
        .unwrap();
        assert_eq!(
            render_engine_widget("pr", &data, None, false).unwrap(),
            "#834"
        );
    }

    #[test]
    fn pr_url_display_mode_and_number_fallback() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 834, "url": "https://github.com/o/r/pull/834" }
        }))
        .unwrap();
        let url_mode = llmenv_config::WidgetConfig {
            display: Some("url".to_string()),
            ..Default::default()
        };
        assert_eq!(
            render_pr(&data, Some(&url_mode), false),
            "https://github.com/o/r/pull/834"
        );
        // url mode but no url → falls back to #number.
        let no_url: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 834 }
        }))
        .unwrap();
        assert_eq!(render_pr(&no_url, Some(&url_mode), false), "#834");
    }

    #[test]
    fn pr_review_state_colors_the_widget() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 1, "review_state": "approved" }
        }))
        .unwrap();
        let out = render_engine_widget("pr", &data, None, true).unwrap();
        assert!(
            out.starts_with("\x1b[32m"),
            "expected green (approved): {out:?}"
        );
    }

    #[test]
    fn pr_url_becomes_osc8_hyperlink_under_color() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 5, "url": "https://github.com/o/r/pull/5" }
        }))
        .unwrap();
        let out = render_pr(&data, None, true);
        assert!(
            out.contains("\x1b]8;;https://github.com/o/r/pull/5\x1b\\"),
            "expected OSC 8 link: {out:?}"
        );
        assert!(out.contains("#5"));
        // A non-http(s) URL must NOT be linked (injection guard).
        let bad: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 5, "url": "javascript:alert(1)" }
        }))
        .unwrap();
        assert!(!render_pr(&bad, None, true).contains("\x1b]8"));
        // No hyperlink when color is off.
        assert!(!render_pr(&data, None, false).contains("\x1b]8"));
    }

    #[test]
    fn branch_links_to_pr_url_under_color() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "branch": { "name": "feat/x" },
            "pr": { "number": 5, "url": "https://github.com/o/r/pull/5" }
        }))
        .unwrap();
        let out = render_branch(&data, None, true);
        assert!(
            out.contains("\x1b]8;;https://github.com/o/r/pull/5"),
            "expected branch→PR link: {out:?}"
        );
    }

    #[test]
    fn untrusted_fields_are_sanitized_end_to_end() {
        // A directory name carrying an escape sequence must not leak into the
        // terminal now that finish() no longer strips.
        let folder: EngineData = serde_json::from_value(serde_json::json!({
            "workspace": { "current_dir": "/tmp/ev\u{1b}[31mil" }
        }))
        .unwrap();
        let out = render_engine_widget("folder", &folder, None, false).unwrap();
        assert!(
            !out.contains('\u{1b}'),
            "escape leaked from folder: {out:?}"
        );

        let model: EngineData = serde_json::from_value(serde_json::json!({
            "model": { "display_name": "Op\u{1b}[31mus 4.8" }
        }))
        .unwrap();
        let out = render_engine_widget("model", &model, None, false).unwrap();
        assert!(!out.contains('\u{1b}'), "escape leaked from model: {out:?}");
    }

    #[test]
    fn renders_progress_bar_from_context_pct() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 65.0 }
        }))
        .unwrap();
        let out = render_engine_widget("progress_bar", &data, None, false).unwrap();
        assert_eq!(out, "35% ▓▓▓░░░░░░░");
    }

    #[test]
    fn progress_bar_threshold_colors_by_usage() {
        assert_eq!(threshold_style(30.0, [50, 80]), "green");
        assert_eq!(threshold_style(60.0, [50, 80]), "yellow");
        assert_eq!(threshold_style(90.0, [50, 80]), "red");
        // End-to-end: 90% used (remaining 10) → red SGR wrap under color.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 10.0 }
        }))
        .unwrap();
        let out = render_engine_widget("progress_bar", &data, None, true).unwrap();
        assert!(out.starts_with("\x1b[31m"), "expected red wrap: {out:?}");
    }

    #[test]
    fn progress_bar_width_configurable() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 50.0 }
        }))
        .unwrap();
        let cfg = llmenv_config::WidgetConfig {
            width: Some(4),
            ..Default::default()
        };
        // 50% of 4 cells = 2 filled.
        assert_eq!(render_progress_bar(&data, Some(&cfg)), "50% ▓▓░░");
    }

    #[test]
    fn missing_branch_and_pr_render_empty() {
        let empty = EngineData::default();
        assert_eq!(
            render_engine_widget("branch", &empty, None, false).unwrap(),
            ""
        );
        assert_eq!(render_engine_widget("pr", &empty, None, false).unwrap(), "");
        assert_eq!(
            render_engine_widget("progress_bar", &empty, None, false).unwrap(),
            ""
        );
    }

    #[test]
    fn renders_usage_5h_minutes_reset() {
        // now=1000, resets_at=now+1380 (23m), used 8% → default "5h {pct}% ➡{reset}".
        let window = RateLimitWindow {
            used_percentage: Some(8.0),
            resets_at: Some(1000 + 1380),
        };
        assert_eq!(
            render_usage(
                Some(&window),
                1000,
                FIVE_HOUR_SECS,
                None,
                "5h {pct}% ➡{reset}"
            ),
            "5h 8% ➡23m"
        );
    }

    #[test]
    fn renders_usage_bar_and_hours_reset() {
        let window = RateLimitWindow {
            used_percentage: Some(100.0),
            resets_at: Some(10_980), // 3h03m from epoch 0
        };
        assert_eq!(
            render_usage(
                Some(&window),
                0,
                FIVE_HOUR_SECS,
                None,
                "{bar} {pct}% ➡{reset}"
            ),
            "▓▓▓▓▓▓▓▓▓▓ 100% ➡3h03m"
        );
    }

    #[test]
    fn usage_widget_via_dispatcher_reads_five_hour_window() {
        // engine_data()'s fixture rate_limits.five_hour.used_percentage = 24.5,
        // which rounds to 25. resets_at is in the past → "➡0m".
        let out = render_engine_widget("usage_5h", &engine_data(), None, false).unwrap();
        assert!(out.starts_with("5h 25%"), "usage_5h: {out}");
    }

    #[test]
    fn usage_widgets_empty_when_windows_absent() {
        let empty = EngineData::default();
        assert_eq!(
            render_engine_widget("usage_5h", &empty, None, false).unwrap(),
            ""
        );
        assert_eq!(
            render_engine_widget("usage_7d", &empty, None, false).unwrap(),
            ""
        );
    }

    #[test]
    fn usage_pace_indicator_over_and_under() {
        // now=1000, window=18000s. resets_at = now + (18000 - elapsed).
        // Under pace: only 10% used but 50% of the window elapsed → ⇣.
        let under = RateLimitWindow {
            used_percentage: Some(10.0),
            resets_at: Some(1000 + 9000), // 9000s remaining = 50% elapsed
        };
        assert_eq!(
            render_usage(Some(&under), 1000, 18_000, None, "{pace}"),
            " \u{21e3}40%" // ⇣ 50 - 10 = 40 under
        );
        // Over pace: 80% used but only 25% elapsed → ⇡.
        let over = RateLimitWindow {
            used_percentage: Some(80.0),
            resets_at: Some(1000 + 13_500), // 25% elapsed
        };
        assert_eq!(
            render_usage(Some(&over), 1000, 18_000, None, "{pace}"),
            " \u{21e1}55%" // ⇡ 80 - 25 = 55 over
        );
    }

    #[test]
    fn usage_threshold_color_applied_end_to_end() {
        // 95% of the 5h window (threshold crit 90) → red wrap.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "rate_limits": { "five_hour": { "used_percentage": 95.0 } }
        }))
        .unwrap();
        let out = render_engine_widget("usage_5h", &data, None, true).unwrap();
        assert!(out.starts_with("\x1b[31m"), "expected red: {out:?}");
    }

    #[test]
    fn peak_widget_renders_symbol_and_label() {
        let out = render_engine_widget("peak", &EngineData::default(), None, false).unwrap();
        // Always one of the two forms, with a label.
        assert!(
            out.contains("peak") || out.contains("off-peak"),
            "peak widget: {out}"
        );
        assert!(out.starts_with('\u{25b3}') || out.starts_with('\u{25bd}'));
    }

    #[test]
    fn humanize_until_formats_and_clamps() {
        assert_eq!(humanize_until(500, 1000), "0m"); // past → clamp
        assert_eq!(humanize_until(1000 + 1380, 1000), "23m");
        assert_eq!(humanize_until(10_980, 0), "3h03m");
    }

    #[test]
    fn missing_workspace_and_context_window_render_empty() {
        // Covers the "no data" guard for the 5 engine widgets not exercised
        // by missing_field_renders_empty_not_panic / missing_branch_and_pr_render_empty.
        let empty = EngineData::default();
        for name in ["folder", "context_pct", "duration", "tokens", "budget"] {
            assert_eq!(
                render_engine_widget(name, &empty, None, false).unwrap(),
                "",
                "widget {name} should render empty on missing data"
            );
        }
    }

    #[test]
    fn render_budget_empty_when_context_window_size_absent() {
        // render_budget has two guards: no context_window at all (covered
        // above), and context_window present but context_window_size unset —
        // this test isolates the second guard specifically.
        let data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(50.0),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("budget", &data, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn progress_bar_renders_empty_for_nan_and_infinite() {
        // Same untrusted-input hazard as render_context_pct: NaN survives
        // f64::clamp unchanged (NaN comparisons are always false), so this
        // must be checked explicitly rather than relying on clamp alone.
        let nan_data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(f64::NAN),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("progress_bar", &nan_data, None, false).unwrap();
        assert_eq!(out, "");

        let inf_data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(f64::INFINITY),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("progress_bar", &inf_data, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn progress_bar_full_at_zero_remaining() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 0.0 }
        }))
        .unwrap();
        let out = render_engine_widget("progress_bar", &data, None, false).unwrap();
        assert_eq!(out, "100% ▓▓▓▓▓▓▓▓▓▓");
    }

    #[test]
    fn progress_bar_empty_at_full_remaining() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 100.0 }
        }))
        .unwrap();
        let out = render_engine_widget("progress_bar", &data, None, false).unwrap();
        assert_eq!(out, "0% ░░░░░░░░░░");
    }

    #[test]
    fn render_context_pct_honors_custom_format() {
        let mut data = engine_data();
        data.context_window = Some(ContextWindow {
            remaining_percentage: Some(65.0),
            ..data.context_window.unwrap()
        });
        let cfg = llmenv_config::WidgetConfig {
            format: Some("used {pct} percent".to_string()),
            ..Default::default()
        };
        assert_eq!(render_context_pct(&data, Some(&cfg)), "used 35 percent");
    }

    #[test]
    fn render_folder_honors_custom_format() {
        let data = engine_data(); // workspace.current_dir = "/home/user/llmenv"
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{path}/{basename}".to_string()),
            ..Default::default()
        };
        assert_eq!(render_folder(&data, Some(&cfg)), "/home/user/llmenv/llmenv");
    }

    #[test]
    fn render_duration_honors_custom_format() {
        let data = engine_data(); // cost.total_duration_ms = 13_320_000
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{s}s total, {total_ms}ms".to_string()),
            ..Default::default()
        };
        assert_eq!(
            render_duration(&data, Some(&cfg)),
            "13320s total, 13320000ms"
        );
    }

    #[test]
    fn render_tokens_honors_custom_format() {
        let data = engine_data(); // input 5000, cache_create 1000, cache_read 4000
        let cfg = llmenv_config::WidgetConfig {
            format: Some("in={input} cr={cache_read} cc={cache_create} tot={total}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            render_tokens(&data, Some(&cfg)),
            "in=5k cr=4k cc=1k tot=10k"
        );
    }

    #[test]
    fn render_budget_honors_custom_format() {
        let data = engine_data(); // context_window_size 200_000, used 10_000
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{max} total, {used} used".to_string()),
            ..Default::default()
        };
        assert_eq!(render_budget(&data, Some(&cfg)), "200k total, 10k used");
    }

    #[test]
    fn render_cache_pct_honors_custom_format() {
        let data = engine_data(); // cache 5000 / total 10000 = 50%
        let cfg = llmenv_config::WidgetConfig {
            format: Some("cache={pct}%".to_string()),
            ..Default::default()
        };
        assert_eq!(render_cache_pct(&data, Some(&cfg)), "cache=50%");
    }

    #[test]
    fn render_branch_honors_custom_format() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "branch": { "name": "release/3.x" }
        }))
        .unwrap();
        let cfg = llmenv_config::WidgetConfig {
            format: Some("on {name}".to_string()),
            ..Default::default()
        };
        assert_eq!(render_branch(&data, Some(&cfg), false), "on release/3.x");
    }

    #[test]
    fn render_pr_honors_custom_format() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "pr": { "number": 834 }
        }))
        .unwrap();
        let cfg = llmenv_config::WidgetConfig {
            format: Some("PR#{number}".to_string()),
            ..Default::default()
        };
        assert_eq!(render_pr(&data, Some(&cfg), false), "PR#834");
    }

    #[test]
    fn render_progress_bar_honors_custom_format() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 65.0 }
        }))
        .unwrap();
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{pct}|{bar}".to_string()),
            ..Default::default()
        };
        assert_eq!(render_progress_bar(&data, Some(&cfg)), "35|▓▓▓░░░░░░░");
    }

    #[test]
    fn format_token_count_thresholds_and_trimmed_decimals() {
        assert_eq!(format_token_count(42), "42");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1k"); // trailing ".0" dropped
        assert_eq!(format_token_count(1500), "1.5k");
        assert_eq!(format_token_count(109_200), "109.2k");
        assert_eq!(format_token_count(200_000), "200k");
        assert_eq!(format_token_count(1_000_000), "1m");
        assert_eq!(format_token_count(1_500_000), "1.5m");
    }

    #[test]
    fn render_context_pct_rounds_fractional_remaining() {
        let data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(64.5),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        // used = 100 - 64.5 = 35.5, rounds up to 36.
        let out = render_engine_widget("context_pct", &data, None, false).unwrap();
        assert_eq!(out, "36%");
    }

    #[test]
    fn render_progress_bar_rounds_fractional_remaining_pct_but_truncates_bar_fill() {
        let data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(64.5),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        // used = 35.5: displayed pct rounds to 36, but fill truncates to 3
        // cells (35.5 / 10.0 = 3.55 -> 3), matching the doc comment's stated
        // "truncate, not round" bar-fill behavior.
        let out = render_engine_widget("progress_bar", &data, None, false).unwrap();
        assert_eq!(out, "36% ▓▓▓░░░░░░░");
    }

    #[test]
    fn render_cache_pct_empty_when_total_tokens_zero_but_context_window_present() {
        let data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: None,
                context_window_size: Some(200_000),
                current_usage: Some(TokenUsage {
                    input_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    cache_read_input_tokens: Some(0),
                }),
            }),
            ..Default::default()
        };
        let out = render_engine_widget("cache_pct", &data, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn tokens_and_cache_pct_saturate_instead_of_overflowing() {
        let data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: None,
                context_window_size: None,
                current_usage: Some(TokenUsage {
                    input_tokens: Some(u64::MAX),
                    cache_creation_input_tokens: Some(u64::MAX),
                    cache_read_input_tokens: Some(u64::MAX),
                }),
            }),
            ..Default::default()
        };
        // Must not panic (debug overflow) and must not wrap into a bogus
        // small number (release overflow).
        let tokens = render_engine_widget("tokens", &data, None, false).unwrap();
        assert!(!tokens.is_empty());
        let cache_pct = render_engine_widget("cache_pct", &data, None, false).unwrap();
        assert_eq!(cache_pct, "\u{21bb}100%");
    }

    fn data_with_remaining(remaining: f64) -> EngineData {
        EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(remaining),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        }
    }

    /// (name, declared placeholders) for every engine widget that builds its
    /// output via a chained `format.replace()` call.
    const ENGINE_WIDGET_PLACEHOLDERS: &[(&str, &[&str])] = &[
        ("model", &["short_name", "version", "full_name"]),
        ("folder", &["basename", "path"]),
        ("context_pct", &["pct"]),
        ("duration", &["h", "m", "s", "total_ms"]),
        ("tokens", &["total", "input", "cache_read", "cache_create"]),
        ("budget", &["used", "max"]),
        ("cache_pct", &["pct"]),
        ("branch", &["name"]),
        ("pr", &["number", "url", "review_state"]),
        ("progress_bar", &["pct", "bar"]),
        ("usage_5h", &["pct", "bar", "reset", "pace"]),
        ("usage_7d", &["pct", "bar", "reset", "pace"]),
        ("peak", &["symbol", "label", "countdown"]),
    ];

    proptest! {
        /// The format string comes from user config — untrusted-ish. No
        /// arbitrary text should ever make a `.replace()` chain panic.
        #[test]
        fn engine_widget_never_panics_on_arbitrary_format_string(
            idx in 0..ENGINE_WIDGET_PLACEHOLDERS.len(),
            format in ".{0,200}",
        ) {
            let (name, _) = ENGINE_WIDGET_PLACEHOLDERS[idx];
            let cfg = llmenv_config::WidgetConfig {
                format: Some(format),
                ..Default::default()
            };
            let _ = render_engine_widget(name, &engine_data(), Some(&cfg), false);
        }

        /// Every placeholder a widget declares (present in its default format
        /// string) must be fully consumed by the `.replace()` chain — none
        /// should survive into the rendered output.
        #[test]
        fn engine_widget_consumes_all_declared_placeholders(junk in "[^{}]{0,10}") {
            let data = engine_data();
            for (name, placeholders) in ENGINE_WIDGET_PLACEHOLDERS {
                let mut format = junk.clone();
                for p in *placeholders {
                    format.push('{');
                    format.push_str(p);
                    format.push('}');
                    format.push_str(&junk);
                }
                let cfg = llmenv_config::WidgetConfig {
                    format: Some(format),
                    ..Default::default()
                };
                let out = render_engine_widget(name, &data, Some(&cfg), false).unwrap();
                for p in *placeholders {
                    let token = format!("{{{p}}}");
                    prop_assert!(
                        !out.contains(&token),
                        "widget {name} left placeholder {token} unconsumed in {out:?}"
                    );
                }
            }
        }

        /// `remaining_percentage` is untrusted external input — must never
        /// panic across the full f64 space (NaN, +/-inf, denormals, extremes)
        /// and must always stay within the documented output contract.
        #[test]
        fn render_context_pct_never_panics_and_stays_in_contract(remaining in any::<f64>()) {
            let data = data_with_remaining(remaining);
            let out = render_engine_widget("context_pct", &data, None, false).unwrap();
            if remaining.is_finite() {
                let pct: i64 = out.trim_end_matches('%').parse().unwrap();
                prop_assert!((0..=100).contains(&pct));
            } else {
                prop_assert_eq!(out, "");
            }
        }

        #[test]
        fn render_progress_bar_never_panics_and_stays_in_contract(remaining in any::<f64>()) {
            let data = data_with_remaining(remaining);
            let out = render_engine_widget("progress_bar", &data, None, false).unwrap();
            if remaining.is_finite() {
                let (pct_str, bar) = out.split_once(' ').unwrap();
                let pct: i64 = pct_str.trim_end_matches('%').parse().unwrap();
                prop_assert!((0..=100).contains(&pct));
                prop_assert_eq!(bar.chars().count(), 10);
                prop_assert!(bar.chars().all(|c| c == '▓' || c == '░'));
            } else {
                prop_assert_eq!(out, "");
            }
        }

        /// Numeric formatting across the full `u64` space: no panics on the
        /// division/rounding, the m/k/bare thresholds hold, and a trailing
        /// ".0" is always dropped (never surfaces in output).
        #[test]
        fn format_token_count_respects_threshold(n in any::<u64>()) {
            let out = format_token_count(n);
            if n < 1000 {
                prop_assert_eq!(out, n.to_string());
            } else if n < 1_000_000 {
                prop_assert!(out.ends_with('k'), "expected k suffix: {out}");
                prop_assert!(!out.contains(".0"), "trailing .0 not trimmed: {out}");
            } else {
                prop_assert!(out.ends_with('m'), "expected m suffix: {out}");
                prop_assert!(!out.contains(".0"), "trailing .0 not trimmed: {out}");
            }
        }

        /// `ms -> h/m` conversion across the full `u64` space: must never
        /// panic (division/modulo only, no overflow-prone arithmetic) and
        /// must reproduce the same breakdown via independent u128 math.
        #[test]
        fn render_duration_never_panics_and_matches_independent_calc(ms in any::<u64>()) {
            let data = EngineData {
                cost: Some(Cost {
                    total_duration_ms: Some(ms),
                }),
                ..Default::default()
            };
            let out = render_engine_widget("duration", &data, None, false).unwrap();
            let total_secs = ms / 1000;
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            let s = total_secs % 60;
            let body = if h > 0 {
                format!("{h}h {m}m")
            } else if m > 0 {
                format!("{m}m {s}s")
            } else {
                format!("{s}s")
            };
            prop_assert_eq!(out, format!("\u{23f1} {body}"));
        }

        /// Token counts are untrusted `u64`s from the engine's stdin JSON —
        /// must never panic (saturating arithmetic) and, when a total exists,
        /// the cache percentage must stay within the documented `0..=100`
        /// contract, mirroring `render_context_pct`'s guarantee.
        #[test]
        fn render_cache_pct_never_panics_and_stays_in_contract(
            input in any::<u64>(),
            cache_creation in any::<u64>(),
            cache_read in any::<u64>(),
        ) {
            let data = EngineData {
                context_window: Some(ContextWindow {
                    remaining_percentage: None,
                    context_window_size: None,
                    current_usage: Some(TokenUsage {
                        input_tokens: Some(input),
                        cache_creation_input_tokens: Some(cache_creation),
                        cache_read_input_tokens: Some(cache_read),
                    }),
                }),
                ..Default::default()
            };
            let out = render_engine_widget("cache_pct", &data, None, false).unwrap();
            let cache = cache_read.saturating_add(cache_creation);
            let total = input.saturating_add(cache);
            if total == 0 {
                prop_assert_eq!(out, "");
            } else {
                let pct: i64 = out
                    .trim_start_matches('↻')
                    .trim_end_matches('%')
                    .parse()
                    .unwrap();
                prop_assert!((0..=100).contains(&pct));
            }
        }

        /// `short_model_name` only ever removes tokens (the "claude" literal
        /// and any version-shaped token) — every surviving token must be one
        /// of the original whitespace-split tokens, in original order.
        #[test]
        fn short_model_name_only_removes_claude_and_version_tokens(
            tokens in prop::collection::vec("[a-zA-Z0-9]{1,8}", 0..6),
        ) {
            let display_name = tokens.join(" ");
            let out = short_model_name(&display_name);
            let kept: Vec<&str> = out.split_whitespace().collect();
            let mut remaining = tokens.iter().map(String::as_str);
            for k in &kept {
                prop_assert!(
                    !k.eq_ignore_ascii_case("claude") && !k.chars().any(|c| c.is_ascii_digit()),
                    "kept token {k:?} should have been filtered"
                );
                prop_assert!(
                    remaining.by_ref().any(|t| t == *k),
                    "kept token {k:?} not found in original order in {tokens:?}"
                );
            }
        }
    }
}
