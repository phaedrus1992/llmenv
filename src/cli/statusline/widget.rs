//! Stateless widget renderers. Each function receives complete input and
//! returns a string — no side effects, no shared mutable state (per the
//! design doc's "Separation of concerns").

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// `usage_5h`/`usage_7d` dispatch: resolves the window, default thresholds,
/// and default format that differ between the two, then delegates to the
/// shared [`render_usage`] backend.
fn render_usage_widget(
    name: &str,
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let state = usage_state_dir();
    let (window, window_secs, defaults, fmt) = if name == "usage_5h" {
        (
            data.rate_limits.as_ref().and_then(|r| r.five_hour.as_ref()),
            FIVE_HOUR_SECS,
            [70, 90],
            "5h {pct}%{delta}{pace} ➡{reset}",
        )
    } else {
        (
            data.rate_limits.as_ref().and_then(|r| r.seven_day.as_ref()),
            SEVEN_DAY_SECS,
            [60, 80],
            "7d {pct}%{delta}{pace} ➡{reset}",
        )
    };
    render_usage(
        &UsageArgs {
            window,
            now: now_unix(),
            window_secs,
            thresholds: cfg.and_then(|c| c.thresholds).unwrap_or(defaults),
            state_dir: state.as_deref(),
            state_key: name,
            use_color,
        },
        cfg,
        fmt,
    )
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
        "duration" => render_duration(data, cfg),
        "tokens" => render_tokens(data, cfg),
        "budget" => render_budget(data, cfg),
        "cache_usage" => render_cache_usage(data, cfg, use_color),
        "branch" => render_branch(data, cfg, use_color),
        "pr" => render_pr(data, cfg, use_color),
        "context" => render_context(data, cfg, use_color),
        "usage_5h" | "usage_7d" => render_usage_widget(name, data, cfg, use_color),
        "peak" => super::peak::render_peak(cfg),
        _ => return None,
    };
    // `pr` colors by review state via a dynamic style; `finish` applies it
    // unless the user set an explicit per-widget `style`. (`context` and
    // the usage widgets color themselves per-cell.)
    let dyn_style = match name {
        "pr" => pr_review_style(data),
        _ => None,
    };
    Some(super::finish(name, raw, cfg, dyn_style, use_color))
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

/// Resolve the current branch, in precedence order:
///
/// 1. `branch.name` from stdin — forward-compat if the engine ever sends it.
/// 2. `worktree.branch` from stdin — Claude Code's only branch field, present
///    for `--worktree` sessions.
/// 3. Git, derived from `workspace.current_dir` — Claude Code sends **no
///    branch** for a regular repo (confirmed against the statusline docs), so
///    llmenv reads it from `.git/HEAD` rather than leaving the widget blank.
///
/// Shared by [`render_branch`] and the `pr` widget's self-resolving fallback
/// ([`resolve_pr`]) — both need "what branch is this" before they can do
/// anything engine-independent.
fn resolve_branch_name(data: &EngineData) -> Option<String> {
    data.branch
        .as_ref()
        .and_then(|b| b.name.clone())
        .or_else(|| data.worktree.as_ref().and_then(|w| w.branch.clone()))
        .or_else(|| {
            data.workspace
                .as_ref()
                .and_then(|w| w.current_dir.as_deref())
                .and_then(|dir| git_branch(Path::new(dir)))
        })
}

fn render_branch(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let Some(name) = resolve_branch_name(data) else {
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
    let Some(pr) = resolve_pr(data) else {
        return String::new();
    };
    render_pr_info(&pr, cfg, use_color)
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
/// red, pending yellow; `None` (default `bold magenta`) otherwise. Uses the
/// same resolved PR (engine-supplied or derived) as [`render_pr`], so a
/// derived PR's review state colors the widget exactly like an engine-supplied
/// one would.
fn pr_review_style(data: &EngineData) -> Option<&'static str> {
    match resolve_pr(data)?.review_state.as_deref()? {
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

/// How long to wait for `gh pr view` before giving up and degrading to no PR.
/// `gh` hits the GitHub API over the network; a stalled connection must not
/// hang the statusline render. 3s is generous for a healthy connection and
/// short enough that a bad one doesn't stall the prompt.
const GH_PR_TIMEOUT_SECS: u64 = 3;

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
fn resolve_pr(data: &EngineData) -> Option<PrInfo> {
    resolve_pr_with(data, "gh")
}

/// [`resolve_pr`]'s backend, with the `gh` binary injectable so tests can
/// exercise the full engine-precedence + branch-resolution + derivation chain
/// against a fake `gh` instead of the real one.
fn resolve_pr_with(data: &EngineData, gh_cmd: &str) -> Option<PrInfo> {
    if let Some(pr) = &data.pr {
        return Some(pr.clone());
    }
    let dir = data
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())?;
    let branch = resolve_branch_name(data)?;
    derive_pr(
        gh_cmd,
        Path::new(dir),
        &branch,
        usage_state_dir().as_deref(),
        now_unix(),
    )
}

/// Cache-then-derive backend for [`resolve_pr`]'s fallback path, fully
/// parameterized (`gh_cmd`, `cache_dir`, `now`) so tests can substitute a
/// fake `gh` and a controlled clock without touching the real network or
/// process environment.
fn derive_pr(
    gh_cmd: &str,
    repo_dir: &Path,
    branch: &str,
    cache_dir: Option<&Path>,
    now: i64,
) -> Option<PrInfo> {
    let cache_path = cache_dir.map(|dir| pr_cache_path(dir, repo_dir, branch));
    if let Some(path) = &cache_path
        && let Some(cached) = read_pr_cache(path, now)
    {
        return cached;
    }
    let fresh = gh_pr_view(gh_cmd, repo_dir, branch);
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
/// retrigger a `gh` call on every render either).
#[derive(Serialize, Deserialize)]
struct PrCacheEntry {
    ts: i64,
    pr: Option<PrInfo>,
}

/// Read a still-fresh cache entry. `None` for a missing/corrupt file *or* an
/// expired one — both cases fall through to a fresh `gh` call in [`derive_pr`].
fn read_pr_cache(path: &Path, now: i64) -> Option<Option<PrInfo>> {
    let contents = std::fs::read_to_string(path).ok()?;
    let entry: PrCacheEntry = match serde_json::from_str(&contents) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::debug!("pr cache: unparseable entry (non-fatal, treating as miss): {e}");
            return None;
        }
    };
    (now - entry.ts < PR_CACHE_TTL_SECS).then_some(entry.pr)
}

/// Best-effort cache write: a failed write only means the next render
/// re-queries `gh`, not a broken widget.
fn write_pr_cache(path: &Path, pr: &Option<PrInfo>, now: i64) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::debug!("pr cache dir unavailable (non-fatal): {e}");
        return;
    }
    let Ok(json) = serde_json::to_string(&PrCacheEntry {
        ts: now,
        pr: pr.clone(),
    }) else {
        return;
    };
    if let Err(e) = std::fs::write(path, json) {
        tracing::debug!("pr cache write failed (non-fatal): {e}");
    }
}

/// Shell out to `gh pr view --json number,url,reviewDecision -- <branch>` in
/// `repo_dir` and parse the result. `None` on any failure — spawn error (`gh`
/// not installed), non-zero exit (not authenticated, no remote, no open PR
/// for the branch), a timeout ([`GH_PR_TIMEOUT_SECS`]), or unparseable
/// output. Every failure is logged at `debug` only — this must degrade
/// silently to the statusline, never print or panic.
fn gh_pr_view(gh_cmd: &str, repo_dir: &Path, branch: &str) -> Option<PrInfo> {
    let mut cmd = Command::new(gh_cmd);
    // `--` terminates option parsing so `branch` (derived from git state) is
    // always read as the positional argument, never as a `gh` flag — a
    // branch named e.g. `--json` (or starting with `-`) can't be
    // misinterpreted. The flags stay before `--`; only the positional goes
    // after it.
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
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::debug!("gh pr view: spawn failed (non-fatal): {e}");
            return None;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(GH_PR_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                let mut stderr = Vec::new();
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                if !status.success() {
                    let first_line = String::from_utf8_lossy(&stderr);
                    let first_line = first_line.lines().next().unwrap_or("");
                    tracing::debug!("gh pr view exited {status} (non-fatal): {first_line}");
                    return None;
                }
                return parse_gh_pr_view(&stdout);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!("gh pr view timed out after {GH_PR_TIMEOUT_SECS}s (non-fatal)");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::debug!("gh pr view: wait failed (non-fatal): {e}");
                return None;
            }
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
            tracing::debug!("gh pr view: unparseable JSON (non-fatal): {e}");
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
/// `review_state` strings [`pr_review_style`] matches on. `gh` never emits a
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
/// Inputs for a usage-window widget, bundled to keep the argument count sane.
struct UsageArgs<'a> {
    window: Option<&'a RateLimitWindow>,
    now: i64,
    window_secs: i64,
    thresholds: [u8; 2],
    /// Directory for delta state files (`None` disables delta tracking).
    state_dir: Option<&'a Path>,
    state_key: &'a str,
    use_color: bool,
}

/// Render a Claude.ai usage window: threshold-colored percentage, a per-cell
/// bar (filled = threshold color, empty dim) with a bright pace-target marker,
/// reset countdown, over/under-pace indicator, and a delta from the last
/// render. `{pct}`/`{bar}`/`{reset}`/`{pace}`/`{delta}` are the placeholders.
fn render_usage(
    args: &UsageArgs,
    cfg: Option<&llmenv_config::WidgetConfig>,
    default_format: &str,
) -> String {
    let Some(window) = args.window else {
        return String::new();
    };
    let Some(raw) = window.used_percentage else {
        return String::new();
    };
    if !raw.is_finite() {
        return String::new();
    }
    let pct = raw.clamp(0.0, 100.0);
    let width = resolve_bar_width(cfg, DEFAULT_BAR_WIDTH);
    let reset = window
        .resets_at
        .map(|r| humanize_until(r, args.now))
        .unwrap_or_default();
    let (pace, marker) = match window.resets_at {
        Some(r) => pace_and_target(pct, r, args.window_secs, args.now, width),
        None => (String::new(), None),
    };
    let style = BarStyle::Threshold {
        thresholds: args.thresholds,
        marker,
    };
    let (pct_str, bar) = pct_and_bar(pct, width, &style, args.use_color);
    let delta = usage_delta(args.state_dir, args.state_key, pct, args.now);
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or(default_format);
    format
        .replace("{pct}", &pct_str)
        .replace("{bar}", &bar)
        .replace("{reset}", &reset)
        .replace("{pace}", &pace)
        .replace("{delta}", &delta)
}

/// The over/under-pace indicator plus the bar column of the linear-pace
/// target. `⇡N%` when usage is ahead of the elapsed window time (burning too
/// fast), `⇣N%` when behind, empty within ±0.5% (with a leading space so it
/// drops out cleanly). The marker column is `None` at the bar extremes, where
/// a marker reads oddly.
fn pace_and_target(
    pct: f64,
    resets_at: i64,
    window_secs: i64,
    now: i64,
    width: usize,
) -> (String, Option<usize>) {
    if window_secs <= 0 {
        return (String::new(), None);
    }
    let remaining = (resets_at - now).clamp(0, window_secs);
    let target = (window_secs - remaining) as f64 / window_secs as f64 * 100.0;
    let over = pct - target;
    let abs = over.abs().round() as i64;
    let indicator = if over > 0.5 {
        format!(" \u{21e1}{abs}%") // ⇡
    } else if over < -0.5 {
        format!(" \u{21e3}{abs}%") // ⇣
    } else {
        String::new()
    };
    let pos = (target / 100.0 * width as f64).round() as i64;
    let marker = (pos > 0 && (pos as usize) < width).then_some(pos as usize);
    (indicator, marker)
}

/// Delta from the previously-recorded used percentage for `key`, e.g.
/// `" (+4.5)"`. Best-effort: reads/writes a small `<state_dir>/<key>` file
/// (`"<pct> <unix_ts>"`), rewriting at most once a minute so the delta
/// reflects real change rather than per-render noise. Empty when there's no
/// state dir, no prior sample, or the change rounds to zero.
fn usage_delta(state_dir: Option<&Path>, key: &str, pct: f64, now: i64) -> String {
    let Some(dir) = state_dir else {
        return String::new();
    };
    let path = dir.join(key);
    let prev = read_usage_state(&path);
    if prev.is_none_or(|(_, ts)| now - ts >= 60) {
        write_usage_state(&path, pct, now);
    }
    match prev {
        Some((prev_pct, _)) => format_delta(pct - prev_pct),
        None => String::new(),
    }
}

fn read_usage_state(path: &Path) -> Option<(f64, i64)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut it = contents.split_whitespace();
    let pct = it.next()?.parse().ok()?;
    let ts = it.next()?.parse().ok()?;
    Some((pct, ts))
}

fn write_usage_state(path: &Path, pct: f64, now: i64) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::debug!("usage delta state dir unavailable (non-fatal): {e}");
        return;
    }
    // Best-effort: a failed write only means the next render's `{delta}`
    // placeholder is empty, not a broken widget — but still worth a trace.
    if let Err(e) = std::fs::write(path, format!("{pct} {now}")) {
        tracing::debug!("usage delta state write failed (non-fatal): {e}");
    }
}

fn format_delta(delta: f64) -> String {
    if delta.abs() < 0.05 {
        String::new()
    } else if delta > 0.0 {
        format!(" (+{delta:.1})")
    } else {
        format!(" ({delta:.1})")
    }
}

/// Directory for usage delta state, alongside the materialized session data
/// (`$CLAUDE_CONFIG_DIR/statusline-state`). `None` when the engine didn't
/// export `CLAUDE_CONFIG_DIR`, which disables delta tracking.
fn usage_state_dir() -> Option<PathBuf> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|d| PathBuf::from(d).join("statusline-state"))
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
    fn renders_cache_usage_default_format() {
        let out = render_engine_widget("cache_usage", &engine_data(), None, false).unwrap();
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
    fn context_clamps_absurdly_large_remaining_percentage() {
        // A corrupt/hostile engine sending remaining_percentage: 1e300 must
        // not produce a saturated i64-cast garbage string.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 1e300 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, false).unwrap();
        assert_eq!(
            out,
            "0% \u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}"
        );
    }

    #[test]
    fn context_clamps_absurdly_negative_remaining_percentage() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": -1e300 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, false).unwrap();
        assert_eq!(
            out,
            "100% \u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}"
        );
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
    fn resolve_pr_none_when_no_workspace_dir() {
        assert_eq!(resolve_pr(&EngineData::default()), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_pr_with_derives_pr_when_engine_sends_none() {
        // Full chain: no engine-supplied `pr`, but a resolvable branch and a
        // (faked) gh that has an open PR for it.
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo_dir.path().join(".git")).unwrap();
        std::fs::write(
            repo_dir.path().join(".git").join("HEAD"),
            "ref: refs/heads/feat/x\n",
        )
        .unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\necho '{\"number\":834,\"url\":\"https://github.com/o/r/pull/834\",\"reviewDecision\":\"APPROVED\"}'\n",
        );
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "workspace": { "current_dir": repo_dir.path().to_string_lossy() }
        }))
        .unwrap();
        assert_eq!(
            resolve_pr_with(&data, &gh.to_string_lossy()),
            Some(PrInfo {
                number: Some(834),
                url: Some("https://github.com/o/r/pull/834".to_string()),
                review_state: Some("approved".to_string()),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_pr_prefers_engine_supplied_over_derivation() {
        // gh would return a *different* PR (#999) if invoked — proves the
        // engine-supplied value wins without ever consulting derivation.
        let bin_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git").join("HEAD"),
            "ref: refs/heads/feat/x\n",
        )
        .unwrap();
        let gh = write_fake_gh(bin_dir.path(), "#!/bin/sh\necho '{\"number\":999}'\n");
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "workspace": { "current_dir": dir.path().to_string_lossy() },
            "pr": { "number": 5, "url": "https://github.com/o/r/pull/5" }
        }))
        .unwrap();
        assert_eq!(
            resolve_pr_with(&data, &gh.to_string_lossy()),
            Some(PrInfo {
                number: Some(5),
                url: Some("https://github.com/o/r/pull/5".to_string()),
                review_state: None,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_pr_none_for_detached_head_never_calls_gh() {
        // A gh that would succeed if invoked — proves detached HEAD stops
        // derivation before ever reaching gh, not that gh happened to fail.
        let bin_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git").join("HEAD"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();
        let gh = write_fake_gh(bin_dir.path(), "#!/bin/sh\necho '{\"number\":1}'\n");
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "workspace": { "current_dir": dir.path().to_string_lossy() }
        }))
        .unwrap();
        assert_eq!(resolve_pr_with(&data, &gh.to_string_lossy()), None);
    }

    #[test]
    fn map_review_decision_matches_gh_values() {
        assert_eq!(map_review_decision("APPROVED"), Some("approved"));
        assert_eq!(
            map_review_decision("CHANGES_REQUESTED"),
            Some("changes_requested")
        );
        assert_eq!(
            map_review_decision("REVIEW_REQUIRED"),
            Some("review_required")
        );
        assert_eq!(map_review_decision(""), None);
        assert_eq!(map_review_decision("SOMETHING_NEW"), None);
    }

    #[test]
    fn parse_gh_pr_view_maps_fields_and_review_decision() {
        let json = br#"{"number":834,"url":"https://github.com/o/r/pull/834","reviewDecision":"APPROVED"}"#;
        assert_eq!(
            parse_gh_pr_view(json),
            Some(PrInfo {
                number: Some(834),
                url: Some("https://github.com/o/r/pull/834".to_string()),
                review_state: Some("approved".to_string()),
            })
        );
    }

    #[test]
    fn parse_gh_pr_view_none_for_missing_number_or_malformed_json() {
        assert_eq!(parse_gh_pr_view(br#"{"url":"https://x"}"#), None);
        assert_eq!(parse_gh_pr_view(b"not json"), None);
    }

    #[cfg(unix)]
    fn write_fake_gh(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn gh_pr_view_derived_path_renders_expected_widget() {
        // Full derived-PR chain: a fake `gh` stands in for the real
        // subprocess, its JSON is parsed into a PrInfo, and that PrInfo
        // renders exactly like an engine-supplied one would.
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\necho '{\"number\":834,\"url\":\"https://github.com/o/r/pull/834\",\"reviewDecision\":\"CHANGES_REQUESTED\"}'\n",
        );
        let pr = gh_pr_view(&gh.to_string_lossy(), repo_dir.path(), "feat/x").unwrap();
        assert_eq!(pr.number, Some(834));
        assert_eq!(render_pr_info(&pr, None, false), "#834");
        assert_eq!(pr.review_state.as_deref(), Some("changes_requested"));
    }

    #[cfg(unix)]
    #[test]
    fn gh_pr_view_terminates_options_before_a_dash_prefixed_branch() {
        // A branch name that looks like a flag (e.g. from a hostile or
        // unusual git ref) must land after `--` so `gh` reads it as the
        // positional branch argument, not an option. The fake `gh` dumps its
        // argv so the test can check `--` immediately precedes the branch.
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > argv.log\necho '{\"number\":1}'\n",
        );
        let branch = "--json";
        let _ = gh_pr_view(&gh.to_string_lossy(), repo_dir.path(), branch);
        let argv = std::fs::read_to_string(repo_dir.path().join("argv.log")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        let dash_dash = args
            .iter()
            .position(|a| *a == "--")
            .expect("no -- terminator in argv");
        assert_eq!(
            args.get(dash_dash + 1),
            Some(&branch),
            "branch must immediately follow -- in argv: {args:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn gh_pr_view_none_when_binary_missing() {
        // Simulates "gh isn't installed": spawn itself fails.
        let repo_dir = tempfile::tempdir().unwrap();
        let missing = repo_dir.path().join("no-such-gh-binary");
        assert_eq!(
            gh_pr_view(&missing.to_string_lossy(), repo_dir.path(), "feat/x"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn gh_pr_view_none_when_gh_exits_nonzero() {
        // Covers "not authenticated" / "no remote" / "no open PR" — from
        // gh_pr_view's perspective these are indistinguishable: a non-zero
        // exit degrades silently to no PR either way.
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\necho 'no pull requests found' >&2\nexit 1\n",
        );
        assert_eq!(
            gh_pr_view(&gh.to_string_lossy(), repo_dir.path(), "feat/x"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn derive_pr_cache_miss_calls_gh_and_writes_cache() {
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\necho called >> gh-calls.log\necho '{\"number\":1,\"url\":null,\"reviewDecision\":null}'\n",
        );
        let pr = derive_pr(
            &gh.to_string_lossy(),
            repo_dir.path(),
            "feat/x",
            Some(cache_dir.path()),
            1_000,
        );
        assert_eq!(pr.as_ref().and_then(|p| p.number), Some(1));
        let calls = std::fs::read_to_string(repo_dir.path().join("gh-calls.log")).unwrap();
        assert_eq!(calls.lines().count(), 1, "gh should be called on a miss");
        let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
        assert!(cache_path.exists(), "cache miss must write a cache entry");
    }

    #[cfg(unix)]
    #[test]
    fn derive_pr_cache_hit_skips_gh() {
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\necho called >> gh-calls.log\necho '{\"number\":1,\"url\":null,\"reviewDecision\":null}'\n",
        );
        let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
        write_pr_cache(
            &cache_path,
            &Some(PrInfo {
                number: Some(999),
                url: None,
                review_state: None,
            }),
            1_000,
        );
        // now=1_030 is within the 60s TTL of the ts=1_000 cache entry.
        let pr = derive_pr(
            &gh.to_string_lossy(),
            repo_dir.path(),
            "feat/x",
            Some(cache_dir.path()),
            1_030,
        );
        assert_eq!(
            pr.as_ref().and_then(|p| p.number),
            Some(999),
            "must return the cached value, not gh's"
        );
        assert!(
            !repo_dir.path().join("gh-calls.log").exists(),
            "a cache hit must not shell out to gh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn derive_pr_cache_expiry_calls_gh_again() {
        let bin_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            bin_dir.path(),
            "#!/bin/sh\necho called >> gh-calls.log\necho '{\"number\":2,\"url\":null,\"reviewDecision\":null}'\n",
        );
        let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
        write_pr_cache(
            &cache_path,
            &Some(PrInfo {
                number: Some(999),
                url: None,
                review_state: None,
            }),
            1_000,
        );
        // now=1_000+PR_CACHE_TTL_SECS+1 is past the cache entry's TTL.
        let now = 1_000 + PR_CACHE_TTL_SECS + 1;
        let pr = derive_pr(
            &gh.to_string_lossy(),
            repo_dir.path(),
            "feat/x",
            Some(cache_dir.path()),
            now,
        );
        assert_eq!(
            pr.as_ref().and_then(|p| p.number),
            Some(2),
            "expiry must re-query gh, not reuse the stale cache"
        );
        let calls = std::fs::read_to_string(repo_dir.path().join("gh-calls.log")).unwrap();
        assert_eq!(calls.lines().count(), 1, "gh should be called after expiry");
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
    fn renders_context_percent_and_bar_by_default() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 65.0 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, false).unwrap();
        assert_eq!(out, "35% ▓▓▓░░░░░░░");
    }

    #[test]
    fn context_threshold_colors_by_usage() {
        assert_eq!(threshold_style(30.0, [50, 80]), "green");
        assert_eq!(threshold_style(60.0, [50, 80]), "yellow");
        assert_eq!(threshold_style(90.0, [50, 80]), "red");
        // End-to-end: 90% used (remaining 10) → red SGR wrap under color.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 10.0 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, true).unwrap();
        assert!(out.starts_with("\x1b[31m"), "expected red wrap: {out:?}");
    }

    #[test]
    fn context_dims_empty_cells_per_cell_under_color() {
        // 35% used → 3 filled ▓ + 7 dim ░, each cell independently colored.
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 65.0 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, true).unwrap();
        assert!(
            out.contains("\x1b[2m░\x1b[0m"),
            "empty cells should be dim: {out:?}"
        );
        assert!(out.contains('▓'), "filled cells present: {out:?}");
    }

    #[test]
    fn context_width_configurable() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 50.0 }
        }))
        .unwrap();
        let cfg = llmenv_config::WidgetConfig {
            width: Some(4),
            ..Default::default()
        };
        // 50% of 4 cells = 2 filled.
        assert_eq!(render_context(&data, Some(&cfg), false), "50% ▓▓░░");
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
            render_engine_widget("context", &empty, None, false).unwrap(),
            ""
        );
    }

    fn usage_args(window: &RateLimitWindow, now: i64, window_secs: i64) -> UsageArgs<'_> {
        UsageArgs {
            window: Some(window),
            now,
            window_secs,
            thresholds: [70, 90],
            state_dir: None, // delta disabled in tests
            state_key: "t",
            use_color: false,
        }
    }

    #[test]
    fn renders_usage_5h_minutes_reset() {
        // now=1000, resets_at=now+1380 (23m), used 8%. Format has no {bar}, so
        // the pace marker doesn't show.
        let window = RateLimitWindow {
            used_percentage: Some(8.0),
            resets_at: Some(1000 + 1380),
        };
        assert_eq!(
            render_usage(
                &usage_args(&window, 1000, FIVE_HOUR_SECS),
                None,
                "5h {pct}% ➡{reset}"
            ),
            "5h 8% ➡23m"
        );
    }

    #[test]
    fn renders_usage_bar_with_pace_marker() {
        // now=0, resets_at=10980 (3h03m), window 5h → 39% elapsed, so the
        // pace-target marker │ lands at bar column 4; used 100% fills the rest.
        let window = RateLimitWindow {
            used_percentage: Some(100.0),
            resets_at: Some(10_980),
        };
        assert_eq!(
            render_usage(
                &usage_args(&window, 0, FIVE_HOUR_SECS),
                None,
                "{bar} {pct}% ➡{reset}"
            ),
            "▓▓▓▓│▓▓▓▓▓ 100% ➡3h03m"
        );
    }

    #[test]
    fn usage_bar_width_configurable() {
        // 50% of a 4-cell bar = 2 filled, matching `context`/`cache_usage`'s
        // `width` override (#905 unified percent/bar rendering).
        let window = RateLimitWindow {
            used_percentage: Some(50.0),
            resets_at: None,
        };
        let cfg = llmenv_config::WidgetConfig {
            width: Some(4),
            ..Default::default()
        };
        assert_eq!(
            render_usage(
                &usage_args(&window, 0, FIVE_HOUR_SECS),
                Some(&cfg),
                "{pct}% {bar}"
            ),
            "50% ▓▓░░"
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
            render_usage(&usage_args(&under, 1000, 18_000), None, "{pace}"),
            " \u{21e3}40%" // ⇣ 50 - 10 = 40 under
        );
        // Over pace: 80% used but only 25% elapsed → ⇡.
        let over = RateLimitWindow {
            used_percentage: Some(80.0),
            resets_at: Some(1000 + 13_500), // 25% elapsed
        };
        assert_eq!(
            render_usage(&usage_args(&over, 1000, 18_000), None, "{pace}"),
            " \u{21e1}55%" // ⇡ 80 - 25 = 55 over
        );
    }

    #[test]
    fn usage_delta_tracks_change_across_renders() {
        let dir = tempfile::tempdir().unwrap();
        let window1 = RateLimitWindow {
            used_percentage: Some(20.0),
            resets_at: None,
        };
        let args1 = UsageArgs {
            window: Some(&window1),
            now: 1_000_000,
            window_secs: FIVE_HOUR_SECS,
            thresholds: [70, 90],
            state_dir: Some(dir.path()),
            state_key: "usage_5h",
            use_color: false,
        };
        // First render: no prior sample → no delta, but state is written.
        assert_eq!(render_usage(&args1, None, "{delta}"), "");
        // Second render 120s later at 24.5% → +4.5.
        let window2 = RateLimitWindow {
            used_percentage: Some(24.5),
            resets_at: None,
        };
        let args2 = UsageArgs {
            window: Some(&window2),
            now: 1_000_120,
            ..args1
        };
        assert_eq!(render_usage(&args2, None, "{delta}"), " (+4.5)");
    }

    #[test]
    fn usage_threshold_color_applied_end_to_end() {
        // 95% of the 5h window (threshold crit 90) → the pct is red-wrapped
        // (self-colored, so it's inside the "5h " prefix rather than leading).
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "rate_limits": { "five_hour": { "used_percentage": 95.0 } }
        }))
        .unwrap();
        let out = render_engine_widget("usage_5h", &data, None, true).unwrap();
        assert!(
            out.contains("\x1b[31m95\x1b[0m"),
            "expected red pct: {out:?}"
        );
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
        for name in ["folder", "context", "duration", "tokens", "budget"] {
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
    fn context_renders_empty_for_nan_and_infinite() {
        // Same untrusted-input hazard as elsewhere: NaN survives f64::clamp
        // unchanged (NaN comparisons are always false), so this must be
        // checked explicitly rather than relying on clamp alone.
        let nan_data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(f64::NAN),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("context", &nan_data, None, false).unwrap();
        assert_eq!(out, "");

        let inf_data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: Some(f64::INFINITY),
                context_window_size: None,
                current_usage: None,
            }),
            ..Default::default()
        };
        let out = render_engine_widget("context", &inf_data, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn context_full_at_zero_remaining() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 0.0 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, false).unwrap();
        assert_eq!(out, "100% ▓▓▓▓▓▓▓▓▓▓");
    }

    #[test]
    fn context_empty_at_full_remaining() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 100.0 }
        }))
        .unwrap();
        let out = render_engine_widget("context", &data, None, false).unwrap();
        assert_eq!(out, "0% ░░░░░░░░░░");
    }

    #[test]
    fn render_context_honors_custom_format() {
        let mut data = engine_data();
        data.context_window = Some(ContextWindow {
            remaining_percentage: Some(65.0),
            ..data.context_window.unwrap()
        });
        let cfg = llmenv_config::WidgetConfig {
            format: Some("used {pct} percent".to_string()),
            ..Default::default()
        };
        assert_eq!(render_context(&data, Some(&cfg), false), "used 35 percent");
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
    fn render_cache_usage_honors_custom_format() {
        let data = engine_data(); // cache 5000 / total 10000 = 50%
        let cfg = llmenv_config::WidgetConfig {
            format: Some("cache={pct}%".to_string()),
            ..Default::default()
        };
        assert_eq!(render_cache_usage(&data, Some(&cfg), false), "cache=50%");
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
    fn render_context_honors_custom_format_with_bar() {
        let data: EngineData = serde_json::from_value(serde_json::json!({
            "context_window": { "remaining_percentage": 65.0 }
        }))
        .unwrap();
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{pct}|{bar}".to_string()),
            ..Default::default()
        };
        assert_eq!(render_context(&data, Some(&cfg), false), "35|▓▓▓░░░░░░░");
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
    fn render_context_rounds_fractional_remaining_pct_but_truncates_bar_fill() {
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
        let out = render_engine_widget("context", &data, None, false).unwrap();
        assert_eq!(out, "36% ▓▓▓░░░░░░░");
    }

    #[test]
    fn render_cache_usage_empty_when_total_tokens_zero_but_context_window_present() {
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
        let out = render_engine_widget("cache_usage", &data, None, false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn tokens_and_cache_usage_saturate_instead_of_overflowing() {
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
        let cache_usage = render_engine_widget("cache_usage", &data, None, false).unwrap();
        assert_eq!(cache_usage, "\u{21bb}100%");
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
        ("duration", &["h", "m", "s", "total_ms"]),
        ("tokens", &["total", "input", "cache_read", "cache_create"]),
        ("budget", &["used", "max"]),
        ("cache_usage", &["pct", "bar"]),
        ("branch", &["name"]),
        ("pr", &["number", "url", "review_state"]),
        ("context", &["pct", "bar"]),
        ("usage_5h", &["pct", "bar", "reset", "pace", "delta"]),
        ("usage_7d", &["pct", "bar", "reset", "pace", "delta"]),
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
        fn render_context_never_panics_and_stays_in_contract(remaining in any::<f64>()) {
            let data = data_with_remaining(remaining);
            let out = render_engine_widget("context", &data, None, false).unwrap();
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
        /// contract, mirroring `render_context`'s guarantee.
        #[test]
        fn render_cache_usage_never_panics_and_stays_in_contract(
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
            let out = render_engine_widget("cache_usage", &data, None, false).unwrap();
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

        /// A bar's filled-cell count never exceeds its width, and its total
        /// glyph count always equals `width` exactly — shared by every
        /// percentage-based widget via `pct_and_bar`, so a single mutation
        /// in this math would otherwise silently propagate to all of them.
        #[test]
        fn block_bar_filled_never_exceeds_width_and_total_len_matches(
            pct in 0.0f64..=100.0,
            width in 0usize..=50,
        ) {
            let bar = block_bar(pct, width);
            let filled = bar.chars().filter(|&c| c == '\u{2593}').count();
            prop_assert!(filled <= width);
            prop_assert_eq!(bar.chars().count(), width);
        }

        /// Monotonicity: a higher percentage never yields fewer filled cells
        /// at the same width.
        #[test]
        fn block_bar_is_monotonic_in_pct(
            lo in 0.0f64..=100.0,
            hi in 0.0f64..=100.0,
            width in 1usize..=50,
        ) {
            let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            let filled_lo = block_bar(lo, width).chars().filter(|&c| c == '\u{2593}').count();
            let filled_hi = block_bar(hi, width).chars().filter(|&c| c == '\u{2593}').count();
            prop_assert!(filled_lo <= filled_hi);
        }

        /// Boundaries: 0% is always empty, 100% is always fully filled, at
        /// any width.
        #[test]
        fn block_bar_boundaries(width in 0usize..=50) {
            let empty = block_bar(0.0, width);
            prop_assert_eq!(empty.chars().filter(|&c| c == '\u{2593}').count(), 0);
            let full = block_bar(100.0, width);
            prop_assert_eq!(full.chars().filter(|&c| c == '\u{2593}').count(), width);
        }

        /// `colored_bar` with `use_color: false` and no marker must degrade
        /// to exactly `block_bar`'s output — it's documented as doing so.
        #[test]
        fn colored_bar_matches_block_bar_when_uncolored_and_unmarked(
            pct in 0.0f64..=100.0,
            width in 0usize..=50,
        ) {
            prop_assert_eq!(colored_bar(pct, width, "yellow", None, false), block_bar(pct, width));
        }

        /// `clean_display_name` is idempotent — applying it a second time
        /// never changes the result further.
        #[test]
        fn clean_display_name_is_idempotent(display_name in ".{0,60}") {
            let once = clean_display_name(&display_name);
            let twice = clean_display_name(once);
            prop_assert_eq!(once, twice);
        }
    }
}
