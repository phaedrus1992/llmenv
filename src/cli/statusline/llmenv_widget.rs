//! llmenv-sourced widget renderers — same stateless contract as
//! `widget.rs`'s engine-sourced renderers, reading from `StatusData` instead
//! of stdin.

use super::data::StatusData;
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
    Some(super::finish(raw, cfg, use_color))
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
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("MCP {total}");
    format
        .replace("{total}", &mcps.total.to_string())
        .replace("{errors}", &mcps.errors.to_string())
}

fn render_icm(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(icm) = &data.icm else {
        return String::new();
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("M{memories}");
    format
        .replace("{memories}", &icm.memories.to_string())
        .replace("{concepts}", &icm.concepts.to_string())
}

fn render_cache(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(cache) = &data.cache else {
        return String::new();
    };
    let humanized = humanize_bytes(cache.prunable_bytes);
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{prunable}");
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
    let icon = icons
        .get("config_stale")
        .cloned()
        .unwrap_or_else(|| "◌".to_string());
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{stale_icon}");
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
    let icon = icons
        .get("session_log")
        .cloned()
        .unwrap_or_else(|| "📝".to_string());
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{icon} {entries}");
    format
        .replace("{icon}", &icon)
        .replace("{entries}", &entries.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
            scopes: Some(ScopesData {
                tags: vec!["dev".into(), "rust".into()],
            }),
            ..Default::default()
        };
        let out = render_llmenv_widget("scopes", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "║ dev · rust");
    }

    #[test]
    fn renders_plugins_total() {
        let data = StatusData {
            plugins: Some(CountData {
                total: 12,
                errors: 0,
            }),
            ..Default::default()
        };
        let out = render_llmenv_widget("plugins", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "◇ 12");
    }

    #[test]
    fn renders_icm_memories() {
        let data = StatusData {
            icm: Some(IcmData {
                memories: 142,
                concepts: 47,
            }),
            ..Default::default()
        };
        let out = render_llmenv_widget("icm", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "M142");
    }

    #[test]
    fn renders_cache_prunable_bytes_humanized() {
        let data = StatusData {
            cache: Some(CacheData {
                prunable_bytes: 15_728_640,
            }),
            ..Default::default()
        };
        let out = render_llmenv_widget("cache", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "15 MB");
    }

    #[test]
    fn renders_config_stale_icon() {
        let data = StatusData {
            config_stale: Some(true),
            ..Default::default()
        };
        let out = render_llmenv_widget("config_stale", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "◌");
    }

    #[test]
    fn missing_data_renders_empty() {
        let data = StatusData::default();
        for name in [
            "scopes",
            "plugins",
            "mcps",
            "icm",
            "cache",
            "config_stale",
            "throttle",
            "session_log",
        ] {
            assert_eq!(
                render_llmenv_widget(name, &data, None, &icons(), false).unwrap(),
                "",
                "widget {name} should render empty on missing data"
            );
        }
    }

    #[test]
    fn unknown_widget_renders_none() {
        assert!(
            render_llmenv_widget("not_real", &StatusData::default(), None, &icons(), false)
                .is_none()
        );
    }
}
