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
/// `simple` (ASCII/Unicode, safe everywhere) when unset. Takes an injectable
/// env var provider so tests can exercise both branches without mutating
/// real process state (mirrors `should_use_color_with_env` in
/// `src/cli/style.rs`).
fn nerd_font_detected_with_env<F>(get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    get_env("LLMENV_NERD_FONT").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[must_use]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by statusline orchestrator, wired up in a follow-up task"
    )
)]
pub fn resolve_icons(
    icon_set: IconSet,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    resolve_icons_with_env(icon_set, configured, &|name| std::env::var(name).ok())
}

fn resolve_icons_with_env<F>(
    icon_set: IconSet,
    configured: &BTreeMap<String, String>,
    get_env: &F,
) -> BTreeMap<String, String>
where
    F: Fn(&str) -> Option<String>,
{
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
        IconSet::Auto if nerd_font_detected_with_env(get_env) => NERD_ICONS
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        assert_eq!(
            icons.get("config_stale").map(String::as_str),
            Some("\u{f0e7}")
        );
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
        let no_env = |_name: &str| -> Option<String> { None };
        let icons = resolve_icons_with_env(IconSet::Auto, &BTreeMap::new(), &no_env);
        assert_eq!(icons.get("config_stale").map(String::as_str), Some("~"));
    }

    #[test]
    fn auto_resolves_to_nerd_when_nerd_font_env_set() {
        let nerd_env = |name: &str| -> Option<String> {
            (name == "LLMENV_NERD_FONT").then(|| "1".to_string())
        };
        let icons = resolve_icons_with_env(IconSet::Auto, &BTreeMap::new(), &nerd_env);
        assert_eq!(
            icons.get("config_stale").map(String::as_str),
            Some("\u{f0e7}")
        );
    }
}
