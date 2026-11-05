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
