//! Doctor's autocompact advice (#2345).
//!
//! `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` is a percentage of the auto-compact window, and two Claude
//! Code settings change what it does: `autoCompactEnabled: false` turns automatic compaction off,
//! and `autoCompactWindow` sets the window the percentage applies to. Design:
//! `docs/design/issue-2345-autocompact-doctor.md`.

use super::CheckLevel;
use serde_yaml::Value;

const OVERRIDE_VAR: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";
/// Above this, PreCompact hooks have too little room to run before the cut.
const PCT_MAX_RECOMMENDED: u32 = 70;
/// Leaves headroom for PreCompact cleanup while still compacting late enough to keep context.
const PCT_RECOMMENDED: u32 = 50;

/// The window the percentage applies to: the top-level `autoCompactWindow`, else the first
/// per-model value under `modelSettings` (Claude Code 2.1.288 and later).
fn autocompact_window(settings: Option<&Value>) -> Option<String> {
    let settings = settings?;
    let top = settings.get("autoCompactWindow");
    let per_model = settings
        .get("modelSettings")
        .and_then(Value::as_mapping)
        .and_then(|models| {
            models
                .values()
                .find_map(|model| model.get("autoCompactWindow"))
        });
    let window = top.or(per_model)?;
    window
        .as_str()
        .map(String::from)
        .or_else(|| window.as_u64().map(|n| n.to_string()))
}

/// The first `autoCompactWindow` (top-level, then per model) that is neither a number nor a
/// string, rendered for the message. Claude Code owns the value range, so only the type is checked.
fn invalid_window(settings: Option<&Value>) -> Option<String> {
    let settings = settings?;
    let per_model = settings
        .get("modelSettings")
        .and_then(Value::as_mapping)
        .into_iter()
        .flat_map(|models| models.values())
        .filter_map(|model| model.get("autoCompactWindow"));
    std::iter::once(settings.get("autoCompactWindow"))
        .flatten()
        .chain(per_model)
        .find(|w| !w.is_string() && w.as_u64().is_none())
        .map(render)
}

fn render(value: &Value) -> String {
    serde_yaml::to_string(value)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(super) fn autocompact_check(
    settings: Option<&Value>,
    override_value: Option<String>,
) -> (CheckLevel, String) {
    let enabled_value = settings.and_then(|s| s.get("autoCompactEnabled"));
    if let Some(value) = enabled_value
        && value.as_bool().is_none()
    {
        return (
            CheckLevel::Warn,
            format!(
                "autoCompactEnabled must be true or false, got {}",
                render(value)
            ),
        );
    }
    if enabled_value.and_then(Value::as_bool) == Some(false) {
        let tail = if override_value.is_some() {
            "; the override can be removed"
        } else {
            ""
        };
        return (
            CheckLevel::Info,
            format!(
                "autoCompactEnabled is false; automatic compaction is off and {OVERRIDE_VAR} has \
                 no effect{tail}"
            ),
        );
    }
    if let Some(bad) = invalid_window(settings) {
        return (
            CheckLevel::Warn,
            format!("autoCompactWindow must be a number of tokens or \"auto\", got {bad}"),
        );
    }
    let window = autocompact_window(settings);
    let of_window = window
        .as_deref()
        .map(|w| format!(" of autoCompactWindow {w}"))
        .unwrap_or_default();
    match override_value.map(|v| v.parse::<u32>().map_err(|_| v)) {
        Some(Ok(0)) => (
            CheckLevel::Warn,
            format!("{OVERRIDE_VAR}=0 is outside the 1-100 range Claude Code accepts"),
        ),
        Some(Ok(pct)) if pct <= PCT_MAX_RECOMMENDED => {
            (CheckLevel::Pass, format!("{OVERRIDE_VAR}={pct}{of_window}"))
        }
        Some(Ok(pct)) => (
            CheckLevel::Warn,
            format!(
                "{OVERRIDE_VAR}={pct}{of_window} (recommend ≤{PCT_MAX_RECOMMENDED} for PreCompact \
                 cleanup)"
            ),
        ),
        Some(Err(raw)) => (
            CheckLevel::Warn,
            format!("{OVERRIDE_VAR} has invalid (non-numeric) value {raw:?}"),
        ),
        None => (CheckLevel::Warn, unset_message(window.as_deref())),
    }
}

fn unset_message(window: Option<&str>) -> String {
    match window {
        Some(w) => format!(
            "{OVERRIDE_VAR} not set (recommend {PCT_RECOMMENDED}; applies to autoCompactWindow {w})"
        ),
        None => format!(
            "{OVERRIDE_VAR} not set (recommend {PCT_RECOMMENDED} for PreCompact headroom; only \
             matters in sessions that compact before the model's limit)"
        ),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn settings(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).expect("test yaml parses")
    }

    fn check(yaml: Option<&str>, over: Option<&str>) -> (CheckLevel, String) {
        let value = yaml.map(settings);
        autocompact_check(value.as_ref(), over.map(String::from))
    }

    #[test]
    fn unset_everything_recommends_50_and_scopes_the_advice() {
        let (level, text) = check(Some("{}"), None);
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("recommend 50"), "{text}");
        assert!(text.contains("compact before the model's limit"), "{text}");
    }

    #[test]
    fn override_at_or_below_70_passes() {
        assert_eq!(check(Some("{}"), Some("50")).0, CheckLevel::Pass);
        assert_eq!(check(Some("{}"), Some("70")).0, CheckLevel::Pass);
    }

    #[test]
    fn override_above_70_warns() {
        let (level, text) = check(Some("{}"), Some("85"));
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("≤70"), "{text}");
    }

    #[test]
    fn non_numeric_override_warns_with_the_value() {
        let (level, text) = check(Some("{}"), Some("abc"));
        assert_eq!(level, CheckLevel::Warn);
        assert!(
            text.contains("non-numeric") && text.contains("abc"),
            "{text}"
        );
    }

    #[test]
    fn disabled_compaction_is_info_with_no_recommendation() {
        let (level, text) = check(Some("autoCompactEnabled: false"), None);
        assert_eq!(level, CheckLevel::Info);
        assert!(!text.contains("recommend"), "{text}");
        assert!(!text.contains("can be removed"), "{text}");
    }

    #[test]
    fn disabled_compaction_with_an_override_says_it_can_go() {
        let (level, text) = check(Some("autoCompactEnabled: false"), Some("50"));
        assert_eq!(level, CheckLevel::Info);
        assert!(text.contains("can be removed"), "{text}");
    }

    #[test]
    fn explicit_true_behaves_like_unset() {
        assert_eq!(
            check(Some("autoCompactEnabled: true"), None).0,
            CheckLevel::Warn
        );
    }

    #[test]
    fn window_is_named_in_the_pass_message() {
        let (level, text) = check(Some("autoCompactWindow: 200000"), Some("50"));
        assert_eq!(level, CheckLevel::Pass);
        assert!(text.contains("autoCompactWindow 200000"), "{text}");
    }

    #[test]
    fn auto_window_is_named_when_the_override_is_unset() {
        let (level, text) = check(Some("autoCompactWindow: auto"), None);
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("autoCompactWindow auto"), "{text}");
    }

    #[test]
    fn per_model_window_counts_as_set() {
        let yaml = "modelSettings:\n  claude-opus-5:\n    autoCompactWindow: 400000";
        let (_, text) = check(Some(yaml), None);
        assert!(text.contains("autoCompactWindow 400000"), "{text}");
    }

    #[test]
    fn top_level_window_wins_over_a_per_model_one() {
        let yaml = "autoCompactWindow: 300000\nmodelSettings:\n  m:\n    autoCompactWindow: 400000";
        let (_, text) = check(Some(yaml), Some("50"));
        assert!(text.contains("autoCompactWindow 300000"), "{text}");
    }

    #[test]
    fn zero_override_warns() {
        let (level, text) = check(Some("{}"), Some("0"));
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("1-100"), "{text}");
    }

    #[test]
    fn non_boolean_enabled_warns_with_the_value() {
        let (level, text) = check(Some("autoCompactEnabled: \"false\""), Some("50"));
        assert_eq!(level, CheckLevel::Warn);
        assert!(
            text.contains("autoCompactEnabled") && text.contains("false"),
            "{text}"
        );
    }

    #[test]
    fn malformed_window_warns_instead_of_reading_as_unset() {
        for yaml in [
            "autoCompactWindow: true",
            "autoCompactWindow: -5",
            "autoCompactWindow: 1.5",
            "modelSettings:\n  m:\n    autoCompactWindow: [1]",
        ] {
            let (level, text) = check(Some(yaml), Some("50"));
            assert_eq!(level, CheckLevel::Warn, "{yaml}");
            assert!(text.contains("autoCompactWindow must be"), "{yaml}: {text}");
        }
    }

    #[test]
    fn missing_settings_behave_as_all_unset() {
        assert_eq!(check(None, None), check(Some("{}"), None));
        assert_eq!(check(None, Some("50")), check(Some("{}"), Some("50")));
    }

    proptest! {
        #[test]
        fn disabled_compaction_is_always_info(
            over in proptest::option::of("[ -~]{0,6}"),
            window in proptest::option::of(100_000u64..=1_000_000),
        ) {
            let mut yaml = String::from("autoCompactEnabled: false\n");
            if let Some(w) = window {
                yaml.push_str(&format!("autoCompactWindow: {w}\n"));
            }
            let value = settings(&yaml);
            let (level, _) = autocompact_check(Some(&value), over);
            prop_assert_eq!(level, CheckLevel::Info);
        }
    }
}
