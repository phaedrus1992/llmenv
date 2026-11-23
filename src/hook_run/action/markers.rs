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
