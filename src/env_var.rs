//! Reads an environment variable as UTF-8, naming the variable when it cannot.
//!
//! A non-UTF-8 value must not look like an unset one. Treating it as unset
//! sends the caller to a default path or a default user without a word of
//! warning (#2589).

use std::env::VarError;

/// Reads `name` as UTF-8.
///
/// Returns `Ok(None)` when the variable is unset. Returns an error that names
/// the variable when its value is not valid UTF-8.
///
/// # Errors
/// Returns an error when `name` is set to a value that is not valid UTF-8.
pub(crate) fn utf8_var(name: &str) -> anyhow::Result<Option<String>> {
    utf8_value(name, std::env::var(name))
}

/// Maps the result of `std::env::var(name)` to the `utf8_var` result.
///
/// Split out so a test can supply a `NotUnicode` value without `set_var`.
fn utf8_value(name: &str, read: Result<String, VarError>) -> anyhow::Result<Option<String>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => {
            anyhow::bail!("{name} is not valid UTF-8; set {name} to a UTF-8 value and retry")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{utf8_value, utf8_var};
    use std::env::VarError;

    #[test]
    fn unset_variable_reads_as_none() {
        // A name no test environment sets, so the read is always NotPresent.
        let got = utf8_var("LLMENV_TEST_ENV_VAR_THAT_IS_NEVER_SET").expect("unset is not an error");
        assert_eq!(got, None);
    }

    #[test]
    fn present_value_reads_as_some() {
        let got = utf8_value("X", Ok("/home/me".to_string())).expect("present");
        assert_eq!(got, Some("/home/me".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_value_is_an_error_that_names_the_variable() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = std::ffi::OsString::from_vec(vec![0x66, 0x6f, 0x80]);

        let err = utf8_value("CLAUDE_CONFIG_DIR", Err(VarError::NotUnicode(raw)))
            .expect_err("non-UTF-8 must not read as unset");

        let msg = format!("{err:#}");
        assert!(msg.contains("CLAUDE_CONFIG_DIR"), "{msg}");
        assert!(msg.contains("UTF-8"), "{msg}");
    }

    proptest::proptest! {
        #[test]
        fn any_utf8_value_round_trips(name in "[A-Z_]{1,12}", value in ".*") {
            let got = utf8_value(&name, Ok(value.clone())).expect("present");

            proptest::prop_assert_eq!(got, Some(value));
        }

        #[test]
        fn an_unset_variable_is_never_an_error(name in "[A-Z_]{1,12}") {
            let got = utf8_value(&name, Err(VarError::NotPresent)).expect("unset");

            proptest::prop_assert_eq!(got, None);
        }
    }
}
