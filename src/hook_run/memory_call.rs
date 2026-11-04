//! What a memory call knows about the hook event that makes it, and the bundles that the endpoint
//! skipped (#3218).

use super::memory_target::MemoryEndpoint;

/// What a memory call needs to know about the hook event that makes it.
pub(super) struct MemoryCallContext<'a, E> {
    /// The hook event, for notice text.
    pub(super) event: E,
    /// The llmenv state folder, if known.
    pub(super) state_dir: Option<&'a std::path::Path>,
    /// The engine conversation id from the hook input, if any.
    pub(super) session_id: Option<&'a str>,
}

/// The bundles that fired without a content folder, when the endpoint is `NotDeclared`.
pub(super) fn skipped_bundles(endpoint: &MemoryEndpoint) -> Vec<String> {
    match endpoint {
        MemoryEndpoint::NotDeclared { skipped_bundles } => skipped_bundles.clone(),
        _ => Vec::new(),
    }
}
