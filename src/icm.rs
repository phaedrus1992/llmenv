//! ICM (Infinite Context Memory) integration.
//! Auto-generates ICM context chunks from active scopes so tag-scoped memory
//! crosses scope boundaries.

use crate::scope::ActiveScopes;

/// Generate an ICM context chunk encoding the active tags/bundles.
/// The chunk is formatted as a markdown block that agents can paste into ICM.
///
/// # Example output
/// ```text
/// ## llmenv context
/// Active scope: `work-vpn`, `rust`
/// Bundles: `bundle1`, `bundle2`
///
/// Use this when storing scope-specific memory in ICM:
/// - Store under keyword: `llmenv-tag:work-vpn`
/// - Memory will be retrieved in any project using tag `work-vpn`
/// ```
pub fn generate_context_chunk(active: &ActiveScopes, bundles: &[String]) -> String {
    let tags_str = if active.tags.is_empty() {
        "(none)".to_string()
    } else {
        active
            .tags
            .iter()
            .map(|t| format!("`{}`", t))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let bundles_str = if bundles.is_empty() {
        "(none)".to_string()
    } else {
        bundles
            .iter()
            .map(|b| format!("`{}`", b))
            .collect::<Vec<_>>()
            .join(", ")
    };

    format!(
        "## llmenv context\n\
         Active tags: {}\n\
         Bundles: {}\n\n\
         Store scope-specific memory under keyword `llmenv-tag:<tag>` \
         so it is retrievable across projects.",
        tags_str, bundles_str
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn test_context_chunk_includes_tags() {
        let mut tags = BTreeSet::new();
        tags.insert("work-vpn".to_string());
        tags.insert("rust".to_string());

        let active = ActiveScopes {
            scopes: vec![],
            tags,
        };

        let chunk = generate_context_chunk(&active, &[]);
        assert!(chunk.contains("work-vpn"));
        assert!(chunk.contains("rust"));
        assert!(chunk.contains("llmenv-tag"));
    }

    #[test]
    fn test_context_chunk_handles_no_tags() {
        let active = ActiveScopes::default();
        let chunk = generate_context_chunk(&active, &[]);
        assert!(chunk.contains("(none)"));
    }

    #[test]
    fn test_context_chunk_includes_bundles() {
        let active = ActiveScopes::default();
        let bundles = vec!["bundle1".to_string(), "bundle2".to_string()];
        let chunk = generate_context_chunk(&active, &bundles);
        assert!(chunk.contains("bundle1"));
        assert!(chunk.contains("bundle2"));
    }
}
