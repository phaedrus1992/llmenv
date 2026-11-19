//! The `pr` widget: resolving the PR of the session and rendering it. The lookup and its cache
//! are in `pr_cache` and `pr_refresh`. Design: docs/design/statusline-pr-lookup.md

use std::path::Path;

use super::gh_cmd::GhPrCmd;
use super::gh_lookup::GhFailure;
use super::pr_cache::derive_pr;
use super::{EngineData, PrInfo, now_unix, resolve_branch_name, usage_state_dir};

/// The `pr` widget for a resolved PR: the text (`render_pr_info`) and the style, which is the grey
/// of a stale PR, or else the review-state color.
pub(super) fn render_pr_widget(
    view: Result<(Option<PrInfo>, bool), PrError>,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let (pr, stale) = match view {
        Ok(view) => view,
        Err(e) => {
            // The format stays and the marker takes the value (#3745), with the cause after it
            // (#3889). The fix goes to the log.
            tracing::error!(
                "statusline pr has no answer: {}. Fix: {}.",
                e.label(),
                e.fix()
            );
            let raw = render_pr_unknown(cfg, e.cause());
            return crate::cli::statusline::finish("pr", raw, cfg, None, use_color);
        }
    };
    let raw = pr
        .as_ref()
        .map_or_else(String::new, |pr| render_pr_info(pr, cfg, use_color));
    // A stale PR drops the review-decision color, because the decision may have changed.
    let dyn_style = if stale {
        Some(STALE_PR_STYLE)
    } else {
        pr.as_ref().and_then(pr_style_for)
    };
    crate::cli::statusline::finish("pr", raw, cfg, dyn_style, use_color)
}

/// Format an already-resolved [`PrInfo`] (engine-supplied or derived via
/// [`resolve_pr`]) into the `pr` widget's text.
pub(super) fn render_pr_info(
    pr: &PrInfo,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let Some(number) = pr.number else {
        return String::new();
    };
    let url = pr
        .url
        .as_deref()
        .map(crate::cli::statusline::sanitize)
        .unwrap_or_default();
    let review_state = pr
        .review_state
        .as_deref()
        .map(crate::cli::statusline::sanitize)
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
    if use_color && crate::cli::statusline::valid_url(&url) {
        crate::cli::statusline::hyperlink(&label, &url)
    } else {
        label
    }
}

/// The grey of a PR that the widget shows from the cache in place of a fresh lookup. It is the
/// grey of the `scopes` widget, and it keeps the yellow of a pending review free (#3889).
const STALE_PR_STYLE: &str = "color-248";

/// The `pr` widget when the lookup failed: the configured format with the marker in place of each
/// value (#3745), then the short cause when there is one (#3889).
fn render_pr_unknown(cfg: Option<&llmenv_config::WidgetConfig>, cause: Option<&str>) -> String {
    let default_format = match cfg.and_then(|c| c.display.as_deref()) {
        Some("url") => "{url}",
        _ => "#{number}",
    };
    let text = crate::cli::statusline::fill_unknown(
        cfg,
        default_format,
        &["{number}", "{url}", "{review_state}"],
    );
    cause.map_or(text.clone(), |cause| format!("{text} {cause}"))
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
///
/// The `bool` is `true` for the last PR that a lookup returned, shown in place of a fresh answer
/// (#3889).
pub(super) fn resolve_pr(data: &EngineData) -> Result<(Option<PrInfo>, bool), PrError> {
    resolve_pr_view(data, GhPrCmd::new("gh"))
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

    /// What to do about this error, for the log.
    fn fix(&self) -> String {
        match self {
            Self::Branch(cause) => {
                format!("the branch cannot be read ({cause}). Check the repository")
            }
            Self::Gh(GhFailure::Missing) => "install gh and put it on the PATH".to_string(),
            Self::Gh(GhFailure::Unauthenticated) => "run `gh auth login`".to_string(),
            Self::Gh(_) => "run `gh pr view` in the folder to see the cause".to_string(),
        }
    }

    /// The short cause that follows the marker. A branch read error has none, because the
    /// branch widget shows it (#3889).
    fn cause(&self) -> Option<&'static str> {
        match self {
            Self::Branch(_) => None,
            Self::Gh(failure) => Some(failure.cause()),
        }
    }
}

/// The result of one PR lookup: the PR, `None` for no open PR, or why `gh` gave no answer.
pub(super) type PrLookup = Result<Option<PrInfo>, GhFailure>;

/// [`resolve_pr`]'s backend, with the `gh` invocation injectable so tests can
/// exercise the full engine-precedence + branch-resolution + derivation chain
/// against a fake `gh` instead of the real one.
///
/// A branch read error is returned, not hidden, so the widget can show it (#2749).
pub(super) fn resolve_pr_with(
    data: &EngineData,
    gh: GhPrCmd<'_>,
) -> Result<Option<PrInfo>, PrError> {
    resolve_pr_view(data, gh).map(|(pr, _)| pr)
}

/// [`resolve_pr_with`] with the stale flag of [`derive_pr`].
fn resolve_pr_view(data: &EngineData, gh: GhPrCmd<'_>) -> Result<(Option<PrInfo>, bool), PrError> {
    if let Some(pr) = &data.pr {
        return Ok((Some(pr.clone()), false));
    }
    let Some(dir) = data
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
    else {
        return Ok((None, false));
    };
    let Some(branch) = resolve_branch_name(data).map_err(PrError::Branch)? else {
        return Ok((None, false));
    };
    let (lookup, stale) = derive_pr(
        gh,
        Path::new(dir),
        &branch,
        usage_state_dir().as_deref(),
        now_unix(),
    );
    lookup.map(|pr| (pr, stale)).map_err(PrError::Gh)
}
