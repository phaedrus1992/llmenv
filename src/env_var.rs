//! Reads an environment variable as UTF-8, naming the variable when it cannot.
//!
//! A non-UTF-8 value must not look like an unset one. Treating it as unset
//! sends the caller to a default path or a default user without a word of
//! warning (#2589).

/// Reads `name` as UTF-8.
///
/// Returns `Ok(None)` when the variable is unset. Returns an error that names
/// the variable when its value is not valid UTF-8.
///
/// # Errors
/// Returns an error when `name` is set to a value that is not valid UTF-8.
pub(crate) fn utf8_var(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{name} is not valid UTF-8; set {name} to a UTF-8 value and retry")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::utf8_var;

    #[test]
    fn unset_variable_reads_as_none() {
        // A name no test environment sets, so the read is always NotPresent.
        let got = utf8_var("LLMENV_TEST_ENV_VAR_THAT_IS_NEVER_SET").expect("unset is not an error");
        assert_eq!(got, None);
    }
}
