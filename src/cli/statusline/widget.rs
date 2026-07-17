//! Stateless widget renderers. Each function receives complete input and
//! returns a string — no side effects, no shared mutable state (per the
//! design doc's "Separation of concerns").

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EngineData {
    pub workspace: Option<Workspace>,
    pub model: Option<ModelInfo>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    #[expect(
        dead_code,
        reason = "part of the stdin contract for forward-compatibility; no widget in the design renders rate-limit data"
    )]
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
#[expect(
    dead_code,
    reason = "part of the stdin contract for forward-compatibility; no widget in the design renders rate-limit data"
)]
pub struct RateLimits {
    pub five_hour: Option<RateLimitWindow>,
    pub seven_day: Option<RateLimitWindow>,
}

#[derive(Debug, Clone, Deserialize)]
#[expect(
    dead_code,
    reason = "part of the stdin contract for forward-compatibility; no widget in the design renders rate-limit data"
)]
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
        "branch" => render_branch(data),
        "pr" => render_pr(data),
        "progress_bar" => render_progress_bar(data),
        _ => return None,
    };
    Some(super::finish(raw, cfg, use_color))
}

fn render_model(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(model) = &data.model else {
        return String::new();
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{short_name} {version}");
    format
        .replace("{short_name}", model.display_name.as_deref().unwrap_or(""))
        .replace("{version}", model.version.as_deref().unwrap_or(""))
        .replace("{full_name}", model.full_name.as_deref().unwrap_or(""))
        .trim()
        .to_string()
}

fn render_folder(data: &EngineData) -> String {
    let Some(path) = data
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
    else {
        return String::new();
    };
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Renders the used-context percentage. `remaining_percentage` comes from an
/// external engine's stdin JSON — untrusted. NaN/infinite values render
/// empty rather than a garbled cast result; any other value is clamped to
/// the valid `0.0..=100.0` range before the `i64` cast so a corrupt/hostile
/// float (e.g. `1e300`) can't produce a saturated, absurd display string.
fn render_context_pct(data: &EngineData) -> String {
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

fn render_tokens(data: &EngineData) -> String {
    let Some(usage) = data
        .context_window
        .as_ref()
        .and_then(|c| c.current_usage.as_ref())
    else {
        return String::new();
    };
    format_token_count(total_tokens(usage))
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
    let used = cw.current_usage.as_ref().map_or(0, total_tokens);
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
    let cache = usage
        .cache_read_input_tokens
        .unwrap_or(0)
        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
    let total = usage.input_tokens.unwrap_or(0).saturating_add(cache);
    if total == 0 {
        return String::new();
    }
    let pct = (cache as f64 / total as f64 * 100.0).round() as i64;
    format!("{pct}%")
}

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

/// 10-cell block bar. `used` (100 - remaining) is the displayed percentage.
///
/// `remaining_percentage` comes from an external engine's stdin JSON —
/// untrusted, same field `render_context_pct` guards. NaN survives
/// `f64::clamp` unchanged (NaN comparisons are always false), so it must be
/// rejected explicitly before the round/cast rather than relying on clamp
/// alone; infinite values are rejected for the same reason.
fn render_progress_bar(data: &EngineData) -> String {
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
    // Truncate (not round) to the filled cell count: round() bumps a
    // borderline value like 35.0 up to 4 filled cells, one more than the
    // 3-cell floor the displayed "35%" label implies.
    let filled = ((used / 10.0) as usize).min(10);
    let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
    format!("{}% {bar}", used.round() as i64)
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
            "release/3.x"
        );
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
        assert_eq!(out, "100% ██████████");
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
        assert_eq!(cache_pct, "100%");
    }
}
