//! The character rules for a tag and a bundle name in a hook memory query.

/// Validate a tag to prevent query injection. Tags must be alphanumeric with
/// hyphens and underscores only (same as bundle/scope naming).
pub(super) fn validate_tag(tag: &str) -> anyhow::Result<()> {
    validate_name("tag", tag)
}

/// Validate a bundle name to prevent query injection. Bundle names follow the
/// same character rules as tags: alphanumeric, hyphens, and underscores only.
pub(super) fn validate_bundle(bundle: &str) -> anyhow::Result<()> {
    validate_name("bundle", bundle)
}

/// `kind` names the thing in the error, such as `tag` or `bundle`.
fn validate_name(kind: &str, name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        let what = if kind == "tag" { "tag" } else { "bundle name" };
        return Err(anyhow::anyhow!("empty {what} in recall query"));
    }
    if !crate::scope::tags::is_valid_tag_charset(name) {
        return Err(anyhow::anyhow!(
            "{kind} '{name}' contains invalid characters (only alphanumeric, -, _ allowed)"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::{validate_bundle, validate_tag};

    #[test]
    fn an_empty_tag_and_an_empty_bundle_name_each_name_their_kind() {
        let tag = validate_tag("").unwrap_err().to_string();
        assert_eq!(tag, "empty tag in recall query");
        let bundle = validate_bundle("").unwrap_err().to_string();
        assert_eq!(bundle, "empty bundle name in recall query");
    }

    #[test]
    fn a_bad_character_names_the_kind_and_the_value() {
        let tag = validate_tag("a b").unwrap_err().to_string();
        assert!(
            tag.starts_with("tag 'a b' contains invalid characters"),
            "{tag}"
        );
        let bundle = validate_bundle("x;y").unwrap_err().to_string();
        assert!(
            bundle.starts_with("bundle 'x;y' contains invalid"),
            "{bundle}"
        );
    }

    #[test]
    fn a_plain_name_passes() {
        assert!(validate_tag("proj-1_a").is_ok());
        assert!(validate_bundle("base").is_ok());
    }
}
