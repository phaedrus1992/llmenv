//! Validation of the Claude Code advisor model setting (#2409).
//! Design: docs/design/issue-2409-advisor-model.md

use crate::effort::is_canonical_model_id;
use crate::schema::Capabilities;
use crate::validate::ValidateError;

/// Aliases Claude Code resolves to its current default model of that family.
pub const ADVISOR_ALIASES: &[&str] = &["fable", "opus", "sonnet"];

/// Validate the advisor fields of one capability source.
///
/// `context` names the source in the error, such as `"config.yaml: capabilities"`. The check is
/// on the shape of the value only. Claude Code and the API own the rule that the advisor must
/// rank at or above the main model, and it changes with each release.
///
/// # Errors
/// Returns an error when the removed `advisor_size` field is still set, or when `advisor_model`
/// is neither an alias nor a canonical model ID.
pub fn validate_advisor(context: &str, caps: &Capabilities) -> Result<(), ValidateError> {
    if caps.advisor_size.is_some() {
        return Err(ValidateError::AdvisorSizeRemoved {
            context: context.to_string(),
        });
    }
    if let Some(model) = &caps.advisor_model
        && !ADVISOR_ALIASES.contains(&model.as_str())
        && !is_canonical_model_id(model)
    {
        return Err(ValidateError::AdvisorModelInvalid {
            context: context.to_string(),
            value: model.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn caps(model: &str) -> Capabilities {
        Capabilities {
            advisor_model: Some(model.to_string()),
            ..Capabilities::default()
        }
    }

    #[test]
    fn accepts_aliases_and_canonical_ids() {
        for ok in [
            "fable",
            "opus",
            "sonnet",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
        ] {
            assert!(validate_advisor("ctx", &caps(ok)).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_everything_else_and_names_the_value() {
        for bad in [
            "small",
            "Opus",
            "claude-",
            "claude-opus-5-5 ",
            "claude-opus-5-5\n",
            "gpt-5",
            "",
        ] {
            let err = validate_advisor("ctx", &caps(bad)).unwrap_err();
            assert!(
                matches!(&err, ValidateError::AdvisorModelInvalid { value, .. } if value == bad),
                "{bad:?}"
            );
            let text = err.to_string();
            assert!(
                text.contains("advisor_model") && text.contains("fable"),
                "{text}"
            );
        }
    }

    #[test]
    fn unset_is_valid() {
        assert!(validate_advisor("ctx", &Capabilities::default()).is_ok());
    }

    #[test]
    fn removed_advisor_size_names_the_replacement() {
        let caps = Capabilities {
            advisor_size: Some("medium".to_string()),
            ..Capabilities::default()
        };
        let err = validate_advisor("ctx", &caps).unwrap_err();
        assert!(matches!(err, ValidateError::AdvisorSizeRemoved { .. }));
        let text = err.to_string();
        assert!(
            text.contains("advisor_size") && text.contains("advisor_model"),
            "{text}"
        );
    }

    proptest! {
        #[test]
        fn model_id_shape_is_accepted(rest in "[a-z0-9]{1,6}(-[a-z0-9]{1,6}){0,3}") {
            let id = format!("claude-{rest}");
            prop_assert!(validate_advisor("ctx", &caps(&id)).is_ok());
        }

        #[test]
        fn a_character_outside_the_id_alphabet_is_rejected(
            head in "[a-z0-9]{0,4}",
            bad in "[A-Z _./:]",
            tail in "[a-z0-9]{0,4}",
        ) {
            let id = format!("claude-opus-{head}{bad}{tail}");
            prop_assert!(validate_advisor("ctx", &caps(&id)).is_err());
        }
    }
}
