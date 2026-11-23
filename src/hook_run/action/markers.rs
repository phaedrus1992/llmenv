/// The keyword prefix under which tag-scoped memory is stored and recalled.
/// A memory written for tag `work-vpn` carries keyword `llmenv-tag:work-vpn`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
pub const TAG_KEYWORD_PREFIX: &str = "llmenv-tag:";

/// The `llmenv-tag:<tag>` keyword for a tag. The tag is assumed pre-validated
/// (see `hook_run::validate_tag`) so it contains no recall-query metacharacters.
#[must_use]
pub fn tag_keyword(tag: &str) -> String {
    format!("{TAG_KEYWORD_PREFIX}{tag}")
}

/// The keyword prefix under which bundle-scoped memory is stored and recalled.
/// A memory written for bundle `base` carries keyword `llmenv-bundle:base`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
pub const BUNDLE_KEYWORD_PREFIX: &str = "llmenv-bundle:";

/// The `llmenv-bundle:<bundle>` keyword for a bundle. The bundle name is
/// assumed pre-validated (see `hook_run::validate_bundle`) so it contains no
/// recall-query metacharacters.
#[must_use]
pub fn bundle_keyword(bundle: &str) -> String {
    format!("{BUNDLE_KEYWORD_PREFIX}{bundle}")
}

/// Internal: parse an `<!-- llmenv-<prefix>: X -->` HTML comment marker.
fn parse_marker<'a>(chunk: &'a str, prefix: &'a str) -> Option<&'a str> {
    chunk
        .split("<!--")
        .skip(1)
        .filter_map(|s| s.split("-->").next())
        .find_map(|s| s.trim().strip_prefix(prefix).map(str::trim))
        .filter(|v| !v.is_empty())
}

/// Parse `<!-- llmenv-type: X -->` marker from a context chunk (R1).
/// Returns the type value if found, `None` otherwise.
///
/// The marker is an HTML comment in the markdown context chunk:
/// ```markdown
/// <!-- llmenv-type: semantic -->
/// ```
/// Only the text between the comment delimiters is examined; nested `-->`
/// within value text is not supported (the first `-->` terminates the match).
#[must_use]
pub fn parse_type_marker(chunk: &str) -> Option<&str> {
    parse_marker(chunk, "llmenv-type:")
}

/// Parse `<!-- llmenv-importance: X -->` marker from a context chunk (R3).
/// Returns the importance value if found, `None` otherwise. Semantics and
/// performance are identical to [`parse_type_marker`].
#[must_use]
pub fn parse_importance_marker(chunk: &str) -> Option<&str> {
    parse_marker(chunk, "llmenv-importance:")
}
