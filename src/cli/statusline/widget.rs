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
        "folder" => render_folder(data, cfg),
        "context_pct" => render_context_pct(data, cfg),
        "duration" => render_duration(data, cfg),
        "tokens" => render_tokens(data, cfg),
        "budget" => render_budget(data, cfg),
        "cache_pct" => render_cache_pct(data, cfg),
        "branch" => render_branch(data, cfg),
        "pr" => render_pr(data, cfg),
        "progress_bar" => render_progress_bar(data, cfg),
        _ => return None,
    };
    Some(super::finish(raw, cfg, use_color))
}

/// Derive a short model family name from Claude's `display_name` (e.g.
/// `"Claude Opus 4.8"` -> `"Opus"`): drops a leading "claude" token
/// (case-insensitive) and any version-shaped token (containing a digit),
/// leaving just the family name(s) in between.
fn short_model_name(display_name: &str) -> String {
    display_name
        .split_whitespace()
        .filter(|tok| {
            !tok.eq_ignore_ascii_case("claude") && !tok.chars().any(|c| c.is_ascii_digit())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_model(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(model) = &data.model else {
        return String::new();
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{short_name} {version}");
    let short_name = model
        .display_name
        .as_deref()
        .map(short_model_name)
        .unwrap_or_default();
    format
        .replace("{short_name}", &short_name)
        .replace("{version}", model.version.as_deref().unwrap_or(""))
        .replace("{full_name}", model.full_name.as_deref().unwrap_or(""))
        .trim()
        .to_string()
}

fn render_folder(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(path) = data
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.as_deref())
    else {
        return String::new();
    };
    let basename = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{basename}");
    format
        .replace("{basename}", &basename)
        .replace("{path}", path)
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
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{h}h{m}m");
    format
        .replace("{h}", &h.to_string())
        .replace("{m}", &m.to_string())
        .replace("{s}", &total_secs.to_string())
        .replace("{total_ms}", &ms.to_string())
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

fn format_token_count(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
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
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{pct}%");
    format.replace("{pct}", &pct.to_string())
}

fn render_branch(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(name) = data.branch.as_ref().and_then(|b| b.name.clone()) else {
        return String::new();
    };
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{name}");
    format.replace("{name}", &name)
}

fn render_pr(data: &EngineData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(n) = data.pr.as_ref().and_then(|p| p.number) else {
        return String::new();
    };
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("#{number}");
    format.replace("{number}", &n.to_string())
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
    // Truncate (not round) to the filled cell count: round() bumps a
    // borderline value like 35.0 up to 4 filled cells, one more than the
    // 3-cell floor the displayed "35%" label implies.
    let filled = ((used / 10.0) as usize).min(10);
    let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
    let pct = used.round() as i64;
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{pct}% {bar}");
    format
        .replace("{pct}", &pct.to_string())
        .replace("{bar}", &bar)
}

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
        // the version-shaped "4.8" token, leaving just the family name.
        let out = render_engine_widget("model", &engine_data(), None, false).unwrap();
        assert_eq!(out, "Opus");
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
    fn renders_tokens_default_format() {
        let out = render_engine_widget("tokens", &engine_data(), None, false).unwrap();
        // total_tokens = input_tokens(5000) + cache_creation_input_tokens(1000)
        // + cache_read_input_tokens(4000) = 10000; format_token_count(10000):
        // 10000 >= 1000, so k-suffix with one decimal = 10000 / 1000.0 = "10.0k".
        assert_eq!(out, "10.0k");
    }

    #[test]
    fn renders_budget_default_format() {
        let out = render_engine_widget("budget", &engine_data(), None, false).unwrap();
        // used = total_tokens(same fixture) = 10000 -> "10.0k"; max =
        // context_window_size(200_000) -> format_token_count(200_000) = "200.0k";
        // default format is "{used}/{max}".
        assert_eq!(out, "10.0k/200.0k");
    }

    #[test]
    fn renders_cache_pct_default_format() {
        let out = render_engine_widget("cache_pct", &engine_data(), None, false).unwrap();
        // cache = cache_read(4000) + cache_creation(1000) = 5000;
        // total = input(5000) + cache(5000) = 10000; pct = round(5000/10000*100) = 50.
        assert_eq!(out, "50%");
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
            "in=5.0k cr=4.0k cc=1.0k tot=10.0k"
        );
    }

    #[test]
    fn render_budget_honors_custom_format() {
        let data = engine_data(); // context_window_size 200_000, used 10_000
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{max} total, {used} used".to_string()),
            ..Default::default()
        };
        assert_eq!(render_budget(&data, Some(&cfg)), "200.0k total, 10.0k used");
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
        assert_eq!(render_branch(&data, Some(&cfg)), "on release/3.x");
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
        assert_eq!(render_pr(&data, Some(&cfg)), "PR#834");
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
        assert_eq!(render_progress_bar(&data, Some(&cfg)), "35|███░░░░░░░");
    }

    #[test]
    fn format_token_count_below_1000_renders_bare_number() {
        assert_eq!(format_token_count(42), "42");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1.0k");
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
        assert_eq!(out, "36% ███░░░░░░░");
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
        assert_eq!(cache_pct, "100%");
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
        ("pr", &["number"]),
        ("progress_bar", &["pct", "bar"]),
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
                prop_assert!(bar.chars().all(|c| c == '█' || c == '░'));
            } else {
                prop_assert_eq!(out, "");
            }
        }

        /// Numeric formatting across the full `u64` space: no panics on the
        /// division/rounding, and the `0..1000` vs `>=1000` threshold holds.
        #[test]
        fn format_token_count_respects_threshold(n in any::<u64>()) {
            let out = format_token_count(n);
            if n < 1000 {
                prop_assert_eq!(out, n.to_string());
            } else {
                prop_assert!(out.ends_with('k'));
                prop_assert_eq!(out, format!("{:.1}k", n as f64 / 1000.0));
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
            let total_secs = u128::from(ms) / 1000;
            let expected = format!(
                "{}h{}m",
                total_secs / 3600,
                (total_secs % 3600) / 60
            );
            prop_assert_eq!(out, expected);
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
                let pct: i64 = out.trim_end_matches('%').parse().unwrap();
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
