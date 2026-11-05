//! The character rules for a tag and a bundle name in a hook memory query.

/// Validate a tag to prevent query injection. Tags must be alphanumeric with
/// hyphens and underscores only (same as bundle/scope naming).
pub(super) fn validate_tag(tag: &str) -> anyhow::Result<()> {
    if tag.is_empty() {
        return Err(anyhow::anyhow!("empty tag in recall query"));
    }
    if !crate::scope::tags::is_valid_tag_charset(tag) {
        return Err(anyhow::anyhow!(
            "tag '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            tag
        ));
    }
    Ok(())
}

/// Validate a bundle name to prevent query injection. Bundle names follow the
/// same character rules as tags: alphanumeric, hyphens, and underscores only.
pub(super) fn validate_bundle(bundle: &str) -> anyhow::Result<()> {
    if bundle.is_empty() {
        return Err(anyhow::anyhow!("empty bundle name in recall query"));
    }
    if !crate::scope::tags::is_valid_tag_charset(bundle) {
        return Err(anyhow::anyhow!(
            "bundle '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            bundle
        ));
    }
    Ok(())
}
