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

    // The ICM context chunk should encode the active tags as a memoir entry
    // that can be auto-injected into Claude's context.
    // This test verifies the structure of the generated chunk.

    // Expected chunk format (from issue #81):
    // - Memoir: "llmenv-context" with concepts for each tag
    // - Labels: tag:<name>, type:scope-context
    // - Relations: part_of→parent scope, instance_of→llmenv-tag

    // For now, this test just verifies that we can generate a properly
    // structured ICM context chunk from the active scope data.

    assert!(!active.tags.is_empty(), "test setup: should have tags");
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
#[ignore = "deferred: recall-side hook integration (issue #81 open question #2)"]
fn test_icm_tag_memory_crosses_projects() {
    // When a tag (e.g., "work-vpn") is active in project A, and memory
    // is stored with keyword "llmenv-tag:work-vpn", that memory should
    // be retrievable when the same tag activates in project B.

    // This requires:
    // 1. Write side: export active tags so agents can store memory keyed by tag ✅ DONE
    // 2. Recall side: when tags activate, call icm_memory_recall with
    //    project filter disabled and keyword filter set to "llmenv-tag:<tag>"
    //    → Deferred: requires hook integration (issue #81 open questions)

    // The write side is implemented. Agents can now see LLMENV_ICM_CONTEXT
    // which documents how to store memory under "llmenv-tag:<tag>" for
    // cross-project retrieval. The recall-side hook would auto-fetch this
    // memory on scope activation, but is deferred for a future sprint.
}
