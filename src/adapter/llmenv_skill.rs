//! Materializes the built-in `llmenv` skill (thin router + per-feature
//! reference files) directly into an adapter's `out/skills/llmenv/`, shared
//! across all 3 adapters. Unlike `super::skills::write_first_class_skills`
//! (which copies a user-configured `SkillSource` from an existing on-disk
//! directory), this skill's content is embedded Rust string constants — the
//! same pattern `src/cli/setup.rs`'s `SETUP_SKILL_SOURCE` uses for the
//! one-shot setup wizard skill, applied here to a skill materialized on every
//! `export`/`regenerate` instead.
//!
//! Replaces the old `TASK_TRACKER_FRAGMENT` CLAUDE.md fragment entirely
//! (Claude-Code-only, hand-appended text) with a cross-engine skill covering
//! all 4 first-party features, materialized only for the ones enabled.

use std::path::{Path, PathBuf};

use crate::config::Features;

const SKILL_ROUTER: &str = include_str!("../../skills/llmenv/SKILL.md");
const TASK_TRACKER_REF: &str = include_str!("../../skills/llmenv/references/task-tracker.md");
const MEMORY_REF: &str = include_str!("../../skills/llmenv/references/memory.md");
const CONTEXT_MODE_REF: &str = include_str!("../../skills/llmenv/references/context-mode.md");
const CODEBASE_MEMORY_REF: &str = include_str!("../../skills/llmenv/references/codebase-memory.md");

/// Write `out/skills/llmenv/SKILL.md` plus one reference file per enabled
/// feature. Writes nothing (returns an empty vec, no directory created) when
/// none of the 4 features are enabled.
///
/// # Errors
/// Propagates any I/O error writing the skill files.
pub(crate) fn materialize_llmenv_skill(
    out: &Path,
    features: &Features,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut refs: Vec<(&str, &str)> = Vec::new();
    if features.task_tracker.as_ref().is_some_and(|t| t.enabled) {
        refs.push(("task-tracker.md", TASK_TRACKER_REF));
    }
    if !features.memory.is_empty() {
        refs.push(("memory.md", MEMORY_REF));
    }
    if features.context_mode.as_ref().is_some_and(|c| c.enabled) {
        refs.push(("context-mode.md", CONTEXT_MODE_REF));
    }
    if !features.codebase_memory.is_empty() {
        refs.push(("codebase-memory.md", CODEBASE_MEMORY_REF));
    }
    if refs.is_empty() {
        return Ok(Vec::new());
    }

    let skill_dir = out.join("skills").join("llmenv");
    let refs_dir = skill_dir.join("references");
    std::fs::create_dir_all(&refs_dir)?;

    let mut owned = Vec::new();
    crate::paths::write_owner_only(&skill_dir.join("SKILL.md"), SKILL_ROUTER.as_bytes())?;
    owned.push(PathBuf::from("skills/llmenv/SKILL.md"));

    for (name, content) in refs {
        crate::paths::write_owner_only(&refs_dir.join(name), content.as_bytes())?;
        owned.push(PathBuf::from("skills/llmenv/references").join(name));
    }
    Ok(owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::{ContextMode, Features, TaskTracker};
    use tempfile::TempDir;

    fn features_with(
        task_tracker: bool,
        memory: bool,
        context_mode: bool,
        codebase_memory: bool,
    ) -> Features {
        Features {
            task_tracker: task_tracker.then_some(TaskTracker { enabled: true }),
            memory: if memory {
                vec![serde_yaml::from_str("server_host: h\nport: 1").unwrap()]
            } else {
                Vec::new()
            },
            context_mode: context_mode.then_some(ContextMode { enabled: true }),
            codebase_memory: if codebase_memory {
                vec![serde_yaml::from_str("{}").unwrap()]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    #[test]
    fn no_features_enabled_materializes_nothing() {
        let dir = TempDir::new().expect("test");
        let owned =
            materialize_llmenv_skill(dir.path(), &features_with(false, false, false, false))
                .expect("test");
        assert!(owned.is_empty());
        assert!(!dir.path().join("skills/llmenv").exists());
    }

    #[test]
    fn task_tracker_only_writes_router_and_one_reference() {
        let dir = TempDir::new().expect("test");
        materialize_llmenv_skill(dir.path(), &features_with(true, false, false, false))
            .expect("test");
        assert!(dir.path().join("skills/llmenv/SKILL.md").exists());
        assert!(
            dir.path()
                .join("skills/llmenv/references/task-tracker.md")
                .exists()
        );
        assert!(
            !dir.path()
                .join("skills/llmenv/references/memory.md")
                .exists()
        );
        assert!(
            !dir.path()
                .join("skills/llmenv/references/context-mode.md")
                .exists()
        );
        assert!(
            !dir.path()
                .join("skills/llmenv/references/codebase-memory.md")
                .exists()
        );
    }

    #[test]
    fn all_four_features_writes_all_four_references() {
        let dir = TempDir::new().expect("test");
        materialize_llmenv_skill(dir.path(), &features_with(true, true, true, true)).expect("test");
        for name in ["task-tracker", "memory", "context-mode", "codebase-memory"] {
            assert!(
                dir.path()
                    .join(format!("skills/llmenv/references/{name}.md"))
                    .exists(),
                "missing {name}.md"
            );
        }
    }

    #[test]
    fn returned_paths_are_relative_to_out() {
        let dir = TempDir::new().expect("test");
        let owned = materialize_llmenv_skill(dir.path(), &features_with(true, false, false, false))
            .expect("test");
        assert!(
            owned
                .iter()
                .any(|p| p == std::path::Path::new("skills/llmenv/SKILL.md"))
        );
        assert!(owned.iter().all(|p| p.is_relative()));
    }
}
