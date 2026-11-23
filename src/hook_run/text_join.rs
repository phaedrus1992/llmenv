//! Joins the text pieces that one hook run prints.

/// Puts the agent notice in front of the llmenv notes, with no empty separator.
pub(super) fn join_notes(notice: Option<&str>, notes: &str) -> String {
    [notice.unwrap_or_default(), notes]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Append `section` to `out` on its own line. An empty section adds nothing.
pub(super) fn append_section(out: &mut String, section: &str) {
    if section.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(section);
}
