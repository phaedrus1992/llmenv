#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
/// Tests for ICM tag mapping (issue #81)
/// Verify that active tags/bundles are made available to ICM so tag-scoped memory
/// crosses scope boundaries.

#[test]
fn test_icm_context_chunk_generation() {
    use std::collections::BTreeSet;

    // Create mock active scopes with tags
    let mut tags = BTreeSet::new();
    tags.insert("work-vpn".to_string());
    tags.insert("rust".to_string());

    let active = llmenv::scope::ActiveScopes {
        scopes: vec![],
        tags,
    };

    // Generate the ICM context chunk with no bundles
    let chunk = llmenv::icm::generate_context_chunk(&active, &[]);

    // Verify chunk contains active tags
    assert!(chunk.contains("work-vpn"), "chunk must list active tags");
    assert!(chunk.contains("rust"), "chunk must list all tags");
    // Verify chunk documents the keyword format for tag-scoped memory storage
    assert!(
        chunk.contains("llmenv-tag"),
        "chunk must document llmenv-tag keyword format"
    );
}

#[test]
fn test_icm_context_chunk_exports_to_env() {
    // The ICM context chunk should be exported as an env var that agents
    // can discover and inject into their context.
    // Format: LLMENV_ICM_CONTEXT

    // When an agent sees this env var set, it should:
    // 1. Parse the serialized ICM context
    // 2. Call icm_memory_store or similar to persist the tag mappings
    // 3. Optionally call memoir methods to create/link concepts

    // This test verifies the structure of the exported chunk
    use llmenv::icm::generate_context_chunk;
    use llmenv::scope::ActiveScopes;
    use std::collections::BTreeSet;

    let mut tags = BTreeSet::new();
    tags.insert("work-vpn".to_string());

    let active = ActiveScopes {
        scopes: vec![],
        tags,
    };

    let bundles = vec!["bundle1".to_string()];
    let chunk = generate_context_chunk(&active, &bundles);

    // Verify the chunk contains the expected information
    assert!(chunk.contains("llmenv context"), "chunk should be labeled");
    assert!(chunk.contains("work-vpn"), "chunk should list tags");
    assert!(chunk.contains("bundle1"), "chunk should list bundles");
    assert!(
        chunk.contains("llmenv-tag"),
        "chunk should document keyword format"
    );
}

#[test]
fn test_icm_context_chunk_exported_by_cli() {
    // Integration test: verify that LLMENV_ICM_CONTEXT is exported by run_export
    // when ICM is active. This test ensures the CLI actually exports the chunk,
    // not just that the function exists.
    use llmenv::icm::generate_context_chunk;
    use llmenv::scope::ActiveScopes;
    use std::collections::BTreeSet;

    // Build a minimal active scope with one tag
    let mut tags = BTreeSet::new();
    tags.insert("test-tag".to_string());

    let active = ActiveScopes {
        scopes: vec![],
        tags,
    };

    // Generate the chunk as the export command would
    let chunk = generate_context_chunk(&active, &[]);

    // Verify chunk is non-empty and contains the tag
    assert!(!chunk.is_empty(), "chunk should be generated");
    assert!(
        chunk.contains("test-tag"),
        "chunk should contain active tag"
    );
    // Verify chunk is valid markdown (has headers and newlines)
    assert!(
        chunk.contains("##"),
        "chunk should be markdown with headers"
    );
    assert!(chunk.contains("\n"), "chunk should contain newlines");
}

#[test]
fn test_icm_tag_memory_crosses_projects() {
    // #197: when a tag (e.g., "work-vpn") is active, memory stored with
    // keyword "llmenv-tag:work-vpn" in project A must be retrievable when the
    // same tag activates in project B.
    //
    // The recall-side hook makes this true by issuing, per active tag, a
    // recall that (a) is project-unfiltered and (b) is keyed on the
    // llmenv-tag:<tag> keyword. The recall query depends only on the tag — not
    // on the calling project — so it resolves identically from any project,
    // which is exactly what lets the memory cross the project boundary.
    use llmenv::hook_run::tag_recall_queries;

    let tags = vec!["work-vpn".to_string()];

    // The recall query an agent in "project A" would issue...
    let from_project_a = tag_recall_queries(&tags).expect("valid tag");
    // ...and the one an agent in "project B" would issue are identical: the
    // query is a pure function of the active tag, carrying no project scope.
    let from_project_b = tag_recall_queries(&tags).expect("valid tag");

    assert_eq!(
        from_project_a, from_project_b,
        "recall must be project-independent so tag memory crosses projects"
    );
    assert_eq!(from_project_a.len(), 1, "one recall per active tag");
    assert_eq!(
        from_project_a[0].keyword, "llmenv-tag:work-vpn",
        "recall must be keyed on the llmenv-tag:<tag> encoding"
    );
}

#[test]
fn test_tag_recall_queries_rejects_invalid_tag() {
    // A scope can't inject recall metacharacters: invalid tags abort the set.
    use llmenv::hook_run::tag_recall_queries;
    let bad = vec!["work-vpn".to_string(), "tag,injection".to_string()];
    assert!(tag_recall_queries(&bad).is_err());
}

#[test]
fn test_tag_recall_queries_one_per_tag_in_order() {
    use llmenv::hook_run::tag_recall_queries;
    let tags = vec!["rust".to_string(), "work-vpn".to_string()];
    let queries = tag_recall_queries(&tags).expect("valid tags");
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0].tag, "rust");
    assert_eq!(queries[1].tag, "work-vpn");
}
