//! Tests for the scope evaluation.

use super::*;

/// A content scope whose folder cannot be read is unknown, not inactive, so `status scopes`
/// can say why it did not match (#3793).
#[cfg(unix)]
#[test]
#[expect(
    clippy::expect_used,
    reason = "fixture setup failure should panic the test"
)]
fn an_unreadable_subdirectory_marks_an_unmatched_content_scope_unknown() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temp dir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("fixture");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("fixture");
    let mut cfg = Config::default();
    cfg.scope.content.push(crate::config::ContentScope {
        id: "md".to_string(),
        r#match: crate::config::ContentMatch {
            glob: "*.md".to_string(),
            depth: None,
        },
        tags: Vec::new(),
    });
    let env = Env {
        cwd: dir.path().display().to_string(),
        ..Default::default()
    };
    let active = evaluate(&cfg, &env);
    // Restore the mode so the temp dir can be removed.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("fixture");
    if std::fs::read_dir(&locked).is_ok() {
        return; // running as root: the mode does not block the read
    }
    assert!(active.unknown_content.contains("md"));
    assert!(active.scopes.iter().all(|s| s.id != "md"));
}

#[test]
fn non_project_tags_excludes_project_scope_tags() {
    let active = ActiveScopes {
        scopes: vec![
            ActiveScope {
                id: "my-host".into(),
                kind: "host",
                tags: vec!["infra".into(), "web".into()],
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            },
            ActiveScope {
                id: "my-project".into(),
                kind: "project",
                tags: vec!["lang-typescript".into(), "agent-coding".into()],
                project_root: Some("/project".into()),
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: Some("project".into()),
                description: None,
                unknown_fields: Vec::new(),
            },
        ],
        // Full union: infra, web, lang-typescript, agent-coding
        tags: BTreeSet::from([
            "infra".into(),
            "web".into(),
            "lang-typescript".into(),
            "agent-coding".into(),
        ]),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    assert!(host_tags.contains("infra"));
    assert!(host_tags.contains("web"));
    assert!(!host_tags.contains("lang-typescript"));
    assert!(!host_tags.contains("agent-coding"));
}

#[test]
fn non_project_tags_no_project_returns_all_tags() {
    let active = ActiveScopes {
        scopes: vec![ActiveScope {
            id: "my-host".into(),
            kind: "host",
            tags: vec!["infra".into(), "web".into()],
            project_root: None,
            enable_bundles: Vec::new(),
            disable_bundles: Vec::new(),
            name: None,
            description: None,
            unknown_fields: Vec::new(),
        }],
        tags: BTreeSet::from(["infra".into(), "web".into()]),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    assert!(host_tags.contains("infra"));
    assert!(host_tags.contains("web"));
    assert_eq!(host_tags.len(), 2);
}

#[test]
fn non_project_tags_preserves_os_tag() {
    let active = ActiveScopes {
        scopes: vec![ActiveScope {
            id: "my-project".into(),
            kind: "project",
            tags: vec!["lang-typescript".into()],
            project_root: Some("/project".into()),
            enable_bundles: Vec::new(),
            disable_bundles: Vec::new(),
            name: Some("p".into()),
            description: None,
            unknown_fields: Vec::new(),
        }],
        // OS tag added by evaluate(), not from any scope
        tags: BTreeSet::from(["lang-typescript".into(), "macos".into()]),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    assert!(!host_tags.contains("lang-typescript"));
    assert!(host_tags.contains("macos"));
}

#[test]
fn non_project_tags_multiple_projects_excludes_all() {
    let active = ActiveScopes {
        scopes: vec![
            ActiveScope {
                id: "p1".into(),
                kind: "project",
                tags: vec!["tag-a".into()],
                project_root: Some("/p1".into()),
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: Some("p1".into()),
                description: None,
                unknown_fields: Vec::new(),
            },
            ActiveScope {
                id: "p2".into(),
                kind: "project",
                tags: vec!["tag-b".into()],
                project_root: Some("/p2".into()),
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: Some("p2".into()),
                description: None,
                unknown_fields: Vec::new(),
            },
            ActiveScope {
                id: "h1".into(),
                kind: "host",
                tags: vec!["shared-tag".into()],
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            },
        ],
        tags: BTreeSet::from(["tag-a".into(), "tag-b".into(), "shared-tag".into()]),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    assert!(!host_tags.contains("tag-a"));
    assert!(!host_tags.contains("tag-b"));
    assert!(host_tags.contains("shared-tag"));
}

#[test]
fn non_project_tags_only_project_scopes_returns_only_os_tag() {
    let active = ActiveScopes {
        scopes: vec![ActiveScope {
            id: "my-project".into(),
            kind: "project",
            tags: vec!["lang-typescript".into(), "agent-coding".into()],
            project_root: Some("/project".into()),
            enable_bundles: Vec::new(),
            disable_bundles: Vec::new(),
            name: Some("p".into()),
            description: None,
            unknown_fields: Vec::new(),
        }],
        tags: BTreeSet::from([
            "lang-typescript".into(),
            "agent-coding".into(),
            "macos".into(),
        ]),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    assert!(!host_tags.contains("lang-typescript"));
    assert!(!host_tags.contains("agent-coding"));
    assert!(host_tags.contains("macos"));
    assert_eq!(host_tags.len(), 1);
}

#[test]
fn non_project_tags_empty_scopes() {
    let active = ActiveScopes {
        scopes: Vec::new(),
        tags: BTreeSet::new(),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    assert!(host_tags.is_empty());
}

#[test]
fn non_project_tags_preserves_tag_shared_with_non_project() {
    // A tag name that appears in BOTH a project and a non-project
    // scope should be preserved — the non-project scope contributed
    // it as an environment tag, which is the correct input for
    // host-level decisions.
    let active = ActiveScopes {
        scopes: vec![
            ActiveScope {
                id: "h1".into(),
                kind: "host",
                tags: vec!["infra".into(), "web".into()],
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            },
            ActiveScope {
                id: "p1".into(),
                kind: "project",
                tags: vec!["infra".into(), "lang-typescript".into()],
                project_root: Some("/p1".into()),
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: Some("p1".into()),
                description: None,
                unknown_fields: Vec::new(),
            },
        ],
        tags: BTreeSet::from(["infra".into(), "web".into(), "lang-typescript".into()]),
        ..Default::default()
    };

    let host_tags = active.non_project_tags();
    // "infra" appears in both host and project — preserved (host contributed it)
    assert!(host_tags.contains("infra"));
    assert!(host_tags.contains("web"));
    // "lang-typescript" is project-only — excluded
    assert!(!host_tags.contains("lang-typescript"));
}

#[test]
fn non_project_tags_excludes_extra_tags() {
    // $LLMENV_EXTRA_TAGS substitutes for a committed .llmenv.yaml, so it
    // must be excluded from host-level resolution just like project
    // tags are (#696, #1020) — not treated as an environment tag like
    // the OS tag.
    let active = ActiveScopes {
        unknown_content: Default::default(),
        scopes: vec![ActiveScope {
            id: "h1".into(),
            kind: "host",
            tags: vec!["infra".into()],
            project_root: None,
            enable_bundles: Vec::new(),
            disable_bundles: Vec::new(),
            name: None,
            description: None,
            unknown_fields: Vec::new(),
        }],
        tags: BTreeSet::from(["infra".into(), "extra-tag".into(), "macos".into()]),
        extra_tags: BTreeSet::from(["extra-tag".into()]),
    };

    let host_tags = active.non_project_tags();
    assert!(host_tags.contains("infra"));
    assert!(host_tags.contains("macos"), "OS tag still preserved");
    assert!(
        !host_tags.contains("extra-tag"),
        "extra_tags must not reach host-level resolution"
    );
}
