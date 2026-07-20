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
    Some(super::finish(name, raw, cfg, None, use_color))
}

fn render_scopes(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(scopes) = &data.scopes else {
        return String::new();
    };
    if scopes.tags.is_empty() {
        return String::new();
    }
    // Tags come from config / the status file — sanitize before display.
    let tags = scopes
        .tags
        .iter()
        .map(|t| super::sanitize(t))
        .collect::<Vec<_>>()
        .join(" · ");
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{tags}");
    format.replace("{tags}", &tags)
}

fn render_plugins(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(plugins) = &data.plugins else {
        return String::new();
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("🔌 {total}");
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
        .unwrap_or("🧠 {memories}");
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
    // `{stale_icon}` resolves from the icon set (mirrors `render_session_log`'s
    // `{icon}`) so a custom `statusline.icons.config_stale` override applies
    // to the default format, not only a custom `format:` string. Defaults to
    // a gear emoji + "stale" label (matching the 🧠/🔌 widget defaults) so the
    // indicator is legible rather than a cryptic `~`. It means the loaded
    // config is out of date — relaunch to pick up changes.
    let icon = icons
        .get("config_stale")
        .cloned()
        .unwrap_or_else(|| "\u{2699}\u{fe0f}".to_string()); // ⚙️
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{stale_icon} stale");
    format.replace("{stale_icon}", &icon)
}

fn render_throttle(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(throttle) = &data.throttle else {
        return String::new();
    };
    let backend = super::sanitize(&throttle.backend); // untrusted (config)
    let raw = format!("{}: {}s", backend, throttle.cooldown_secs);
    let format = cfg.and_then(|c| c.format.as_deref()).unwrap_or("{raw}");
    format
        .replace("{raw}", &raw)
        .replace("{cooldown_secs}", &throttle.cooldown_secs.to_string())
        .replace("{reason}", &backend)
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
    use crate::cli::statusline::data::{
        CacheData, CountData, IcmData, ScopesData, StatusData, ThrottleData,
    };
    use proptest::prelude::*;
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
        assert_eq!(out, "dev · rust");
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
        assert_eq!(out, "🔌 12");
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
        assert_eq!(out, "🧠 142");
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
        // No icon-set override for `config_stale` here (unlike the shared
        // `icons()` fixture) — this tests the true hardcoded fallback, not a
        // configured glyph.
        let data = StatusData {
            config_stale: Some(true),
            ..Default::default()
        };
        let out =
            render_llmenv_widget("config_stale", &data, None, &BTreeMap::new(), false).unwrap();
        assert_eq!(out, "\u{2699}\u{fe0f} stale"); // ⚙️ stale (default)
    }

    #[test]
    fn renders_mcps_total() {
        let data = StatusData {
            mcps: Some(CountData {
                total: 7,
                errors: 0,
            }),
            ..Default::default()
        };
        let out = render_llmenv_widget("mcps", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "MCP 7");
    }

    #[test]
    fn renders_throttle_backend_and_cooldown() {
        let data = StatusData {
            throttle: Some(ThrottleData {
                backend: "icm".to_string(),
                cooldown_secs: 45,
            }),
            ..Default::default()
        };
        let out = render_llmenv_widget("throttle", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "icm: 45s");
    }

    #[test]
    fn renders_session_log_entry_count() {
        let data = StatusData {
            session_log: Some(8),
            ..Default::default()
        };
        let out = render_llmenv_widget("session_log", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "📝 8");
    }

    #[test]
    fn renders_scopes_empty_when_tags_list_is_empty() {
        let data = StatusData {
            scopes: Some(ScopesData { tags: vec![] }),
            ..Default::default()
        };
        let out = render_llmenv_widget("scopes", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn renders_cache_prunable_bytes_at_kb_and_sub_kb() {
        let data = StatusData {
            cache: Some(CacheData {
                prunable_bytes: 2048,
            }),
            ..Default::default()
        };
        assert_eq!(
            render_llmenv_widget("cache", &data, None, &icons(), false).unwrap(),
            "2 KB"
        );

        let data = StatusData {
            cache: Some(CacheData {
                prunable_bytes: 512,
            }),
            ..Default::default()
        };
        assert_eq!(
            render_llmenv_widget("cache", &data, None, &icons(), false).unwrap(),
            "512 B"
        );
    }

    #[test]
    fn renders_config_stale_empty_when_explicitly_not_stale() {
        let data = StatusData {
            config_stale: Some(false),
            ..Default::default()
        };
        let out = render_llmenv_widget("config_stale", &data, None, &icons(), false).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn config_stale_stale_icon_placeholder_falls_back_when_icon_missing() {
        // A custom `{stale_icon}` format resolves from the icon set, falling
        // back to the default gear glyph when the icon map has no entry.
        let data = StatusData {
            config_stale: Some(true),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{stale_icon}".to_string()),
            ..Default::default()
        };
        let empty_icons = BTreeMap::new();
        let out =
            render_llmenv_widget("config_stale", &data, Some(&cfg), &empty_icons, false).unwrap();
        assert_eq!(out, "\u{2699}\u{fe0f}"); // ⚙️
    }

    #[test]
    fn render_scopes_honors_custom_format() {
        let data = StatusData {
            scopes: Some(ScopesData {
                tags: vec!["dev".into()],
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("tags={tags}".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("scopes", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "tags=dev");
    }

    #[test]
    fn render_plugins_honors_custom_format() {
        let data = StatusData {
            plugins: Some(CountData {
                total: 3,
                errors: 1,
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{total}/{errors}".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("plugins", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "3/1");
    }

    #[test]
    fn render_mcps_honors_custom_format() {
        let data = StatusData {
            mcps: Some(CountData {
                total: 5,
                errors: 2,
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{total}/{errors}".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("mcps", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "5/2");
    }

    #[test]
    fn render_icm_honors_custom_format() {
        let data = StatusData {
            icm: Some(IcmData {
                memories: 10,
                concepts: 4,
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{memories}c{concepts}".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("icm", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "10c4");
    }

    #[test]
    fn render_cache_honors_custom_format() {
        let data = StatusData {
            cache: Some(CacheData {
                prunable_bytes: 2048,
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{prunable} ({prunable_raw}B)".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("cache", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "2 KB (2048B)");
    }

    #[test]
    fn render_config_stale_honors_custom_format() {
        let data = StatusData {
            config_stale: Some(true),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("STALE:{stale_icon}".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("config_stale", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "STALE:◌");
    }

    #[test]
    fn render_throttle_honors_custom_format() {
        let data = StatusData {
            throttle: Some(ThrottleData {
                backend: "icm".to_string(),
                cooldown_secs: 45,
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{reason} for {cooldown_secs}s".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("throttle", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "icm for 45s");
    }

    #[test]
    fn render_session_log_honors_custom_format() {
        let data = StatusData {
            session_log: Some(8),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("entries={entries}".to_string()),
            ..Default::default()
        };
        let out = render_llmenv_widget("session_log", &data, Some(&cfg), &icons(), false).unwrap();
        assert_eq!(out, "entries=8");
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

    fn full_status_data() -> StatusData {
        StatusData {
            scopes: Some(ScopesData {
                tags: vec!["dev".into(), "rust".into()],
            }),
            plugins: Some(CountData {
                total: 12,
                errors: 1,
            }),
            mcps: Some(CountData {
                total: 7,
                errors: 0,
            }),
            icm: Some(IcmData {
                memories: 142,
                concepts: 47,
            }),
            cache: Some(CacheData {
                prunable_bytes: 15_728_640,
            }),
            config_stale: Some(true),
            throttle: Some(ThrottleData {
                backend: "icm".to_string(),
                cooldown_secs: 45,
            }),
            session_log: Some(8),
        }
    }

    /// (name, declared placeholders) for every llmenv widget that builds its
    /// output via a chained `format.replace()` call.
    const LLMENV_WIDGET_PLACEHOLDERS: &[(&str, &[&str])] = &[
        ("scopes", &["tags"]),
        ("plugins", &["total", "errors"]),
        ("mcps", &["total", "errors"]),
        ("icm", &["memories", "concepts"]),
        ("cache", &["prunable", "prunable_raw"]),
        ("config_stale", &["stale_icon"]),
        ("throttle", &["raw", "cooldown_secs", "reason"]),
        ("session_log", &["icon", "entries"]),
    ];

    proptest! {
        /// The format string comes from user config — untrusted-ish. No
        /// arbitrary text should ever make a `.replace()` chain panic.
        #[test]
        fn llmenv_widget_never_panics_on_arbitrary_format_string(
            idx in 0..LLMENV_WIDGET_PLACEHOLDERS.len(),
            format in ".{0,200}",
        ) {
            let (name, _) = LLMENV_WIDGET_PLACEHOLDERS[idx];
            let cfg = llmenv_config::WidgetConfig {
                format: Some(format),
                ..Default::default()
            };
            let _ = render_llmenv_widget(name, &full_status_data(), Some(&cfg), &icons(), false);
        }

        /// Every placeholder a widget declares (present in its default format
        /// string) must be fully consumed by the `.replace()` chain — none
        /// should survive into the rendered output.
        #[test]
        fn llmenv_widget_consumes_all_declared_placeholders(junk in "[^{}]{0,10}") {
            let data = full_status_data();
            for (name, placeholders) in LLMENV_WIDGET_PLACEHOLDERS {
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
                let out =
                    render_llmenv_widget(name, &data, Some(&cfg), &icons(), false).unwrap();
                for p in *placeholders {
                    let token = format!("{{{p}}}");
                    prop_assert!(
                        !out.contains(&token),
                        "widget {name} left placeholder {token} unconsumed in {out:?}"
                    );
                }
            }
        }

        /// Boundary behavior at 1024 / 1024^2 across the full `u64` space —
        /// previously only unit-tested at fixed values.
        #[test]
        fn humanize_bytes_respects_unit_thresholds(bytes in any::<u64>()) {
            const KB: u64 = 1024;
            const MB: u64 = 1024 * 1024;
            let out = humanize_bytes(bytes);
            if bytes >= MB {
                prop_assert_eq!(out, format!("{} MB", bytes / MB));
            } else if bytes >= KB {
                prop_assert_eq!(out, format!("{} KB", bytes / KB));
            } else {
                prop_assert_eq!(out, format!("{bytes} B"));
            }
        }
    }
}
