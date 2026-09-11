//! ICM (Infinite Context Memory) integration.
//! Auto-generates ICM context chunks from active scopes so tag-scoped memory
//! crosses scope boundaries.

use crate::scope::ActiveScopes;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing;

/// Path to the ICM state file within state_dir.
fn icm_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("icm.json")
}

/// Stored ICM tag/bundle memory.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct IcmMemory {
    tags: Vec<String>,
    bundles: Vec<String>,
}

/// Build the `## llmenv context` header shared by [`generate_minimal_chunk`]
/// and [`generate_context_chunk`]: the tags/bundles summary lines, with no
/// trailing content. Both callers append their own suffix (boilerplate
/// instruction, project info) after this.
fn context_header(active: &ActiveScopes, bundles: &[String]) -> String {
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
        "## llmenv context\nActive tags: {}\nBundles: {}",
        tags_str, bundles_str
    )
}

/// Append the active project's name/description to `chunk`, if a project
/// scope is present. Shared suffix for [`generate_minimal_chunk`] and
/// [`generate_context_chunk`].
fn append_project_info(chunk: &mut String, active: &ActiveScopes) {
    for scope in &active.scopes {
        if scope.kind == "project"
            && let Some(name) = &scope.name
        {
            chunk.push_str("\n\n**Project:** ");
            chunk.push_str(name);
            if let Some(desc) = &scope.description {
                chunk.push_str(" — ");
                chunk.push_str(desc);
            }
        }
    }
}

/// Generate a minimal ICM context chunk for storage.
/// Contains only variable parts (tags, bundles, project info) without boilerplate.
/// Reduces token waste when the same boilerplate instruction is stored per-project.
/// The full instruction text is provided once in the SessionStart hook injection.
///
/// # Example output
/// ```text
/// ## llmenv context
/// Active tags: `work-vpn`, `rust`
/// Bundles: `bundle1`, `bundle2`
///
/// **Project:** MyProject — A test project
/// ```
pub(crate) fn generate_minimal_chunk(active: &ActiveScopes, bundles: &[String]) -> String {
    let mut chunk = context_header(active, bundles);
    append_project_info(&mut chunk, active);
    chunk
}

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
    let mut chunk = context_header(active, bundles);
    chunk.push_str(
        "\n\n\
         Store scope-specific memory under keyword `llmenv-tag:<tag>` (per tag) \
         or `llmenv-bundle:<bundle>` (per bundle) so it is retrievable across \
         projects. On each turn, llmenv auto-recalls memory under these tags' \
         `llmenv-tag:<tag>` and bundles' `llmenv-bundle:<bundle>` keywords \
         across all projects.",
    );
    append_project_info(&mut chunk, active);
    chunk
}

/// Store tag/bundle memory mappings for retrieval by SessionStart hook.
/// Called during `llmenv export` to record which tags and bundles are active,
/// so the SessionStart hook can inject them into agent context via ICM.
///
/// # Errors
/// Returns an error if memory storage fails.
pub(crate) fn store_tag_memory(active: &ActiveScopes, bundles: &[String]) -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    let memory = IcmMemory {
        tags: active.tags.iter().cloned().collect::<Vec<_>>(),
        bundles: bundles.to_vec(),
    };
    write_memory(&icm_state_path(&state_dir), &memory)?;
    tracing::debug!(
        "stored ICM tag memory: tags={}, bundles={}",
        memory.tags.join(","),
        memory.bundles.join(",")
    );
    Ok(())
}

/// Write `IcmMemory` as JSON to `path` with mode 0o600. Pure I/O helper —
/// extracted so property tests can exercise the on-disk format without
/// touching the global state_dir env var.
fn write_memory(path: &Path, memory: &IcmMemory) -> anyhow::Result<()> {
    let json = serde_json::to_string(memory)?;
    crate::paths::write_owner_only_atomic(path, json.as_bytes())?;
    Ok(())
}

/// Read `IcmMemory` from `path`. Counterpart to `write_memory` — used by
/// property tests to verify on-disk roundtrip.
#[cfg(test)]
fn read_memory(path: &Path) -> anyhow::Result<IcmMemory> {
    let body = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn test_minimal_chunk_omits_boilerplate() {
        let mut tags = BTreeSet::new();
        tags.insert("work-vpn".to_string());
        tags.insert("rust".to_string());

        let active = ActiveScopes {
            scopes: vec![],
            tags,
            ..Default::default()
        };

        let minimal = generate_minimal_chunk(&active, &[]);
        let full = generate_context_chunk(&active, &[]);

        // Minimal chunk should be much shorter (no boilerplate instruction).
        assert!(minimal.len() < full.len());

        // Minimal chunk includes tags and project info.
        assert!(minimal.contains("work-vpn"));
        assert!(minimal.contains("rust"));
        assert!(minimal.contains("## llmenv context"));

        // But not the instruction paragraph (boilerplate in full chunk).
        assert!(!minimal.contains("Store scope-specific memory"));
        assert!(full.contains("Store scope-specific memory"));
    }

    #[test]
    fn test_context_chunk_includes_tags() {
        let mut tags = BTreeSet::new();
        tags.insert("work-vpn".to_string());
        tags.insert("rust".to_string());

        let active = ActiveScopes {
            scopes: vec![],
            tags,
            ..Default::default()
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
        assert!(
            chunk.contains("llmenv-bundle:"),
            "chunk must document llmenv-bundle keyword format for agents"
        );
    }

    #[test]
    fn test_context_chunk_includes_project_description() {
        use crate::scope::ActiveScope;
        let active = ActiveScopes {
            scopes: vec![ActiveScope {
                id: "myproj".into(),
                kind: "project",
                tags: vec![],
                project_root: Some(std::path::PathBuf::from("/tmp/myproj")),
                enable_bundles: vec![],
                disable_bundles: vec![],
                name: Some("MyProject".into()),
                description: Some("A test project".into()),
                unknown_fields: vec![],
            }],
            tags: BTreeSet::new(),
            ..Default::default()
        };
        let chunk = generate_context_chunk(&active, &[]);
        assert!(chunk.contains("MyProject"), "name must appear");
        assert!(chunk.contains("A test project"), "description must appear");
    }

    #[test]
    fn test_context_chunk_omits_description_when_absent() {
        use crate::scope::ActiveScope;
        let active = ActiveScopes {
            scopes: vec![ActiveScope {
                id: "myproj".into(),
                kind: "project",
                tags: vec![],
                project_root: Some(std::path::PathBuf::from("/tmp/myproj")),
                enable_bundles: vec![],
                disable_bundles: vec![],
                name: Some("MyProject".into()),
                description: None,
                unknown_fields: vec![],
            }],
            tags: BTreeSet::new(),
            ..Default::default()
        };
        let chunk = generate_context_chunk(&active, &[]);
        assert!(chunk.contains("MyProject"));
        // No em-dash from the "name — description" separator.
        assert!(
            !chunk.contains("MyProject —"),
            "no separator when description absent"
        );
    }

    #[test]
    fn test_store_tag_memory_succeeds() {
        let mut tags = BTreeSet::new();
        tags.insert("work".to_string());
        tags.insert("rust".to_string());

        let active = ActiveScopes {
            scopes: vec![],
            tags,
            ..Default::default()
        };

        let bundles = vec!["bundle1".to_string(), "bundle2".to_string()];
        let result = store_tag_memory(&active, &bundles);
        assert!(result.is_ok());
    }

    // ===== Property tests for #145, #146 =====

    use proptest::prelude::*;

    proptest! {
        // #145: IcmMemory serde roundtrip — deserialize(serialize(x)) == x
        // for arbitrary tag/bundle lists including special characters.
        #[test]
        fn icm_memory_serde_roundtrip(
            tags in proptest::collection::vec(r"\PC{0,30}", 0..10),
            bundles in proptest::collection::vec(r"\PC{0,30}", 0..10),
        ) {
            let memory = IcmMemory { tags, bundles };
            let json = serde_json::to_string(&memory).expect("serialize");
            let decoded: IcmMemory = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(memory, decoded);
        }

        // #145: Empty vectors roundtrip correctly.
        #[test]
        fn icm_memory_empty_roundtrip(_unit in any::<()>()) {
            let memory = IcmMemory { tags: vec![], bundles: vec![] };
            let json = serde_json::to_string(&memory).expect("serialize");
            let decoded: IcmMemory = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(memory, decoded);
        }

        // #1792: generate_minimal_chunk includes every input tag and bundle,
        // each wrapped in backticks, and never contains the boilerplate
        // instruction paragraph reserved for generate_context_chunk.
        #[test]
        fn minimal_chunk_includes_all_tags_and_bundles(
            tags in proptest::collection::vec("[a-zA-Z0-9_-]{1,20}", 0..8),
            bundles in proptest::collection::vec("[a-zA-Z0-9_-]{1,20}", 0..8),
        ) {
            let active = ActiveScopes {
                scopes: vec![],
                tags: tags.iter().cloned().collect(),
                ..Default::default()
            };
            let chunk = generate_minimal_chunk(&active, &bundles);

            for tag in &tags {
                let wrapped = format!("`{}`", tag);
                prop_assert!(chunk.contains(&wrapped));
            }
            for bundle in &bundles {
                let wrapped = format!("`{}`", bundle);
                prop_assert!(chunk.contains(&wrapped));
            }
            prop_assert!(!chunk.contains("Store scope-specific memory"));
            prop_assert!(chunk.starts_with("## llmenv context"));
        }

        // #146: store + recall via filesystem preserves data integrity.
        #[test]
        fn store_recall_filesystem_roundtrip(
            tags in proptest::collection::vec(r"[\w \-:,]{0,30}", 0..8),
            bundles in proptest::collection::vec(r"[\w \-:,]{0,30}", 0..8),
        ) {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let path = temp.path().join("icm.json");
            let memory = IcmMemory { tags, bundles };
            write_memory(&path, &memory).expect("write");
            let recalled = read_memory(&path).expect("read");
            prop_assert_eq!(memory, recalled);
        }

        // #146: Multi-cycle idempotence — repeated store/recall preserves data
        // exactly. Each overwrite must produce identical bytes given identical
        // input.
        #[test]
        fn store_recall_idempotent(
            tags in proptest::collection::vec(r"[a-zA-Z0-9_-]{1,20}", 0..5),
        ) {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let path = temp.path().join("icm.json");
            let memory = IcmMemory { tags, bundles: vec![] };
            for _ in 0..3 {
                write_memory(&path, &memory).expect("write");
                let recalled = read_memory(&path).expect("read");
                prop_assert_eq!(&memory, &recalled);
            }
        }

        // #146: File permissions remain mode 0o600 after store. Critical
        // security property — prevents information disclosure on shared hosts.
        #[test]
        fn store_writes_owner_only_permissions(
            tags in proptest::collection::vec(r"[a-z]{1,10}", 0..3),
        ) {
            use std::os::unix::fs::PermissionsExt;
            let temp = tempfile::TempDir::new().expect("tempdir");
            let path = temp.path().join("icm.json");
            let memory = IcmMemory { tags, bundles: vec![] };
            write_memory(&path, &memory).expect("write");
            let mode = std::fs::metadata(&path).expect("metadata").permissions().mode();
            // Only owner bits should be set in the low 9 bits.
            prop_assert_eq!(mode & 0o077, 0, "group/other bits set: {:o}", mode);
        }
    }
}
