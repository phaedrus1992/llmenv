//! Validation of Claude Code effort settings (#2144).
//! Design: docs/design/issue-2144-per-model-effort.md

use crate::schema::Capabilities;
use crate::validate::ValidateError;

/// Values Claude Code accepts for `effortLevel` in a settings file.
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Values Claude Code accepts for `maxEffortLevel`. `max` means no cap.
pub const MAX_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Validate every effort field of one capability source.
///
/// `context` names the source in the error, such as `"config.yaml: capabilities"`.
///
/// # Errors
/// Returns the first invalid value: an effort level outside the allowed set, a
/// `model_effort` key that is not a canonical Claude model ID, or a
/// `model_effort` entry with no field set.
pub fn validate_effort(context: &str, caps: &Capabilities) -> Result<(), ValidateError> {
    if let Some(level) = &caps.effort_level {
        check_level(context, "capabilities.effort_level", level, EFFORT_LEVELS)?;
    }
    let slippage = caps.features.as_ref().and_then(|f| f.slippage.as_ref());
    if let Some(level) = slippage.and_then(|s| s.effort_level.as_ref()) {
        check_level(
            context,
            "features.slippage.effort_level",
            level,
            EFFORT_LEVELS,
        )?;
    }
    for (model, entry) in &caps.model_effort {
        check_model_id(context, model)?;
        if entry.effort_level.is_none() && entry.max_effort_level.is_none() {
            return Err(ValidateError::ModelEffortEmpty {
                context: context.to_string(),
                model: model.clone(),
            });
        }
        if let Some(level) = &entry.effort_level {
            let field = format!("model_effort.{model}.effort_level");
            check_level(context, &field, level, EFFORT_LEVELS)?;
        }
        if let Some(level) = &entry.max_effort_level {
            let field = format!("model_effort.{model}.max_effort_level");
            check_level(context, &field, level, MAX_EFFORT_LEVELS)?;
        }
    }
    Ok(())
}

fn check_level(
    context: &str,
    field: &str,
    value: &str,
    allowed: &[&str],
) -> Result<(), ValidateError> {
    if allowed.contains(&value) {
        return Ok(());
    }
    let hint = match value {
        "max" => {
            " max applies to one session only; set CLAUDE_CODE_EFFORT_LEVEL=max in \
             native.claude_code.env instead."
        }
        "ultracode" => " set native.claude_code.ultracode: true instead.",
        _ => "",
    };
    Err(ValidateError::InvalidEffortLevel {
        context: context.to_string(),
        field: field.to_string(),
        value: value.to_string(),
        allowed: allowed.join(", "),
        hint: hint.to_string(),
    })
}

/// True for a canonical Claude model ID: `claude-` and then lowercase ASCII words joined by `-`.
pub(crate) fn is_canonical_model_id(model: &str) -> bool {
    model.strip_prefix("claude-").is_some_and(|rest| {
        rest.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
    })
}

/// Claude Code matches an alias (`opus`), a `[1m]` suffix, or a provider ID to
/// the canonical model ID entry itself, so only the canonical ID is a valid key.
/// A canonical ID is `claude-` and then lowercase ASCII words joined by `-`.
fn check_model_id(context: &str, model: &str) -> Result<(), ValidateError> {
    if is_canonical_model_id(model) {
        return Ok(());
    }
    Err(ValidateError::ModelEffortKey {
        context: context.to_string(),
        model: model.to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::schema::{Features, ModelEffort, SlippageControl};

    fn caps_with_level(level: &str) -> Capabilities {
        Capabilities {
            effort_level: Some(level.to_string()),
            ..Capabilities::default()
        }
    }

    fn caps_with_model(model: &str, effort: Option<&str>, max: Option<&str>) -> Capabilities {
        let entry = ModelEffort {
            effort_level: effort.map(str::to_string),
            max_effort_level: max.map(str::to_string),
        };
        Capabilities {
            model_effort: [(model.to_string(), entry)].into_iter().collect(),
            ..Capabilities::default()
        }
    }

    #[test]
    fn effort_level_accepts_the_settings_values() {
        for level in EFFORT_LEVELS {
            validate_effort("t", &caps_with_level(level)).expect(level);
        }
    }

    #[test]
    fn effort_level_rejects_other_values() {
        for level in ["", "max", "ultracode", "HIGH", "extreme"] {
            let err = validate_effort("t", &caps_with_level(level)).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("capabilities.effort_level"), "{msg}");
            assert!(msg.contains("low, medium, high, xhigh"), "{msg}");
        }
    }

    #[test]
    fn max_and_ultracode_name_the_right_setting() {
        let max = validate_effort("t", &caps_with_level("max")).unwrap_err();
        assert!(
            max.to_string().contains("CLAUDE_CODE_EFFORT_LEVEL=max"),
            "{max}"
        );
        let ultra = validate_effort("t", &caps_with_level("ultracode")).unwrap_err();
        assert!(ultra.to_string().contains("ultracode: true"), "{ultra}");
    }

    #[test]
    fn slippage_effort_level_is_checked() {
        let caps = Capabilities {
            features: Some(Features {
                slippage: Some(SlippageControl {
                    effort_level: Some("max".to_string()),
                    ..SlippageControl::default()
                }),
                ..Features::default()
            }),
            ..Capabilities::default()
        };
        let err = validate_effort("t", &caps).unwrap_err();
        assert!(
            err.to_string().contains("features.slippage.effort_level"),
            "{err}"
        );
    }

    #[test]
    fn model_effort_levels_are_checked() {
        validate_effort(
            "t",
            &caps_with_model("claude-opus-5-5", Some("xhigh"), Some("max")),
        )
        .expect("valid entry");
        let err = validate_effort("t", &caps_with_model("claude-opus-5-5", Some("max"), None))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("model_effort.claude-opus-5-5.effort_level"),
            "{err}"
        );
        let err = validate_effort("t", &caps_with_model("claude-opus-5-5", None, Some("huge")))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("max_effort_level"), "{msg}");
        assert!(msg.contains("xhigh, max"), "{msg}");
    }

    #[test]
    fn model_effort_entry_needs_a_field() {
        let err =
            validate_effort("t", &caps_with_model("claude-opus-5-5", None, None)).unwrap_err();
        assert!(
            err.to_string()
                .contains("set effort_level, max_effort_level, or both"),
            "{err}"
        );
    }

    #[test]
    fn model_effort_key_must_be_a_canonical_id() {
        for key in [
            "opus",
            "claude-opus-5-5[1m]",
            "gpt-5",
            "",
            "default",
            "claude-",
            "claude-Opus-5-5",
            "claude-opus-5-5 ",
            "claude-opus--5",
            "claude-opus-5-",
        ] {
            let err = validate_effort("t", &caps_with_model(key, Some("high"), None)).unwrap_err();
            assert!(
                err.to_string().contains("canonical model ID"),
                "{key}: {err}"
            );
        }
        validate_effort(
            "t",
            &caps_with_model("claude-fable-5-1", Some("high"), None),
        )
        .expect("canonical ID");
    }

    use proptest::prelude::*;

    proptest! {
        // A value passes exactly when it is in the allowed set, for every field.
        #[test]
        fn level_check_matches_the_allowed_set(value in "[a-z]{0,8}|max|xhigh|ultracode") {
            let start_ok = EFFORT_LEVELS.contains(&value.as_str());
            let cap_ok = MAX_EFFORT_LEVELS.contains(&value.as_str());
            prop_assert_eq!(validate_effort("t", &caps_with_level(&value)).is_ok(), start_ok);
            let start = caps_with_model("claude-opus-5-5", Some(&value), None);
            prop_assert_eq!(validate_effort("t", &start).is_ok(), start_ok);
            let cap = caps_with_model("claude-opus-5-5", None, Some(&value));
            prop_assert_eq!(validate_effort("t", &cap).is_ok(), cap_ok);
        }

        // ModelEffort survives a YAML round trip, the format of config.yaml.
        #[test]
        fn model_effort_yaml_round_trip(
            effort in proptest::option::of(prop::sample::select(EFFORT_LEVELS)),
            max in proptest::option::of(prop::sample::select(MAX_EFFORT_LEVELS)),
        ) {
            let entry = ModelEffort {
                effort_level: effort.map(str::to_string),
                max_effort_level: max.map(str::to_string),
            };
            let yaml = serde_yaml::to_string(&entry).unwrap();
            let back: ModelEffort = serde_yaml::from_str(&yaml).unwrap();
            prop_assert_eq!(back, entry);
        }
    }
}
