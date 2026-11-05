//! Checks on the env vars that `llmenv export` emits, so the `export NAME=...` contract holds
//! for every emission path.

/// Reject any var in `env` whose name isn't a valid shell identifier, or whose value holds NUL.
/// Applied to adapter-returned env vars before they propagate.
///
/// # Errors
/// A name or a value fails [`validate_var_name`] or [`validate_var_value`].
pub(super) fn reject_invalid_var_names(env: &[(String, String)]) -> anyhow::Result<()> {
    for (name, value) in env {
        validate_var_name(name)?;
        validate_var_value(value)?;
    }
    Ok(())
}

/// A shell variable name must match `[A-Za-z_][A-Za-z0-9_]*`.
///
/// # Errors
/// The name is empty, starts with a digit, or holds another character.
pub(super) fn validate_var_name(name: &str) -> anyhow::Result<()> {
    let Some(first) = name.chars().next() else {
        anyhow::bail!("Variable name cannot be empty");
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        anyhow::bail!(
            "Variable name '{}' must start with letter or underscore",
            name
        );
    }
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            anyhow::bail!(
                "Variable name '{}' contains invalid character '{}'",
                name,
                ch
            );
        }
    }
    Ok(())
}

/// # Errors
/// The value holds a NUL byte.
pub(super) fn validate_var_value(value: &str) -> anyhow::Result<()> {
    // Only NUL is rejected: it can't survive in a C-string env var and would
    // truncate the export. Newlines/CR are safe because every emission path
    // single-quotes the value (`util::single_quote`), and values like
    // LLMENV_ICM_CONTEXT are legitimately multiline. (#469)
    if value.contains('\0') {
        anyhow::bail!("variable value contains forbidden NUL byte");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_var_name_accepts_valid_names() {
        assert!(validate_var_name("MY_VAR").is_ok());
        assert!(validate_var_name("_private").is_ok());
        assert!(validate_var_name("var123").is_ok());
        assert!(validate_var_name("x").is_ok());
    }

    #[test]
    fn validate_var_name_rejects_invalid_names() {
        assert!(validate_var_name("").is_err());
        assert!(validate_var_name("123var").is_err());
        assert!(validate_var_name("my-var").is_err());
        assert!(validate_var_name("my var").is_err());
        assert!(validate_var_name("my$var").is_err());
        assert!(validate_var_name("éVAR").is_err());
    }

    #[test]
    fn reject_invalid_var_names_passes_valid_and_fails_invalid() {
        let ok = vec![
            ("CLAUDE_CONFIG_DIR".to_string(), "/x".to_string()),
            ("_PRIVATE".to_string(), "y".to_string()),
        ];
        assert!(reject_invalid_var_names(&ok).is_ok());

        let bad = vec![("bad-name".to_string(), "v".to_string())];
        assert!(reject_invalid_var_names(&bad).is_err());

        let bad_leading_digit = vec![("1ABC".to_string(), "v".to_string())];
        assert!(reject_invalid_var_names(&bad_leading_digit).is_err());
    }

    #[test]
    fn reject_invalid_var_names_allows_multiline_values() {
        // NUL truncates the C-string env var — still rejected.
        let with_nul = vec![("VALID_NAME".to_string(), "value\0malicious".to_string())];
        assert!(reject_invalid_var_names(&with_nul).is_err());

        // Newlines/CR are safe — emission always single-quotes via shell_escape. (#469)
        let with_newline = vec![(
            "VALID_NAME".to_string(),
            "## context\nActive tags: `foo`".to_string(),
        )];
        assert!(reject_invalid_var_names(&with_newline).is_ok());

        let with_cr = vec![("VALID_NAME".to_string(), "value\rmore".to_string())];
        assert!(reject_invalid_var_names(&with_cr).is_ok());
    }
}
