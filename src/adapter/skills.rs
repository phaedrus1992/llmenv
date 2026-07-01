//! Shared skill-writing helpers used by adapters that materialise first-class
//! `skills:` entries and plugin-projected skills.
//!
//! Both [`super::claude_code::ClaudeCodeAdapter`] and
//! [`super::crush::CrushAdapter`] call these; the implementation lives here
//! so it is not duplicated. The helpers are `pub(crate)` — they are an
//! internal rendering detail, not part of the public [`super::AgentAdapter`]
//! contract.

use std::path::{Path, PathBuf};

/// Copy all first-class skill sources into `out/skills/<name>/`, owner-only.
///
/// Returns the paths written (relative to `out`). The `out/skills/` directory
/// is created on first use. An empty `skills` slice is a no-op (no directory
/// created, empty vec returned).
///
/// # Errors
/// - Unsafe (path-traversal) skill name.
/// - Source path is not a directory.
/// - I/O error during directory copy.
pub(crate) fn write_first_class_skills(
    out: &Path,
    skills: &[crate::config::SkillSource],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut owned: Vec<PathBuf> = Vec::new();
    if skills.is_empty() {
        return Ok(owned);
    }
    let skills_dir = out.join("skills");
    for skill in skills {
        if crate::paths::is_unsafe_join_target(&skill.name) {
            anyhow::bail!(
                "unsafe skill name '{}': contains path-traversal components",
                skill.name
            );
        }
        let src = Path::new(&skill.path);
        if !src.is_dir() {
            anyhow::bail!(
                "skill '{}': path '{}' is not a directory",
                skill.name,
                skill.path
            );
        }
        let dest = skills_dir.join(&skill.name);
        let written = super::claude_code::copy_dir_owner_only(src, &dest)?;
        // Track relative paths (relative to `out`) in the owned set.
        // strip_prefix is infallible here: copy_dir_owner_only writes under
        // `dest` which is `out/skills/<name>`, so every returned path starts
        // with `out`. The debug_assert guards this invariant in test builds.
        for abs_path in written {
            debug_assert!(
                abs_path.starts_with(out),
                "copy_dir_owner_only returned a path outside `out`: {}",
                abs_path.display()
            );
            let rel = abs_path.strip_prefix(out).map_err(|_| {
                anyhow::anyhow!(
                    "internal error: written path '{}' is not under output dir '{}'",
                    abs_path.display(),
                    out.display()
                )
            })?;
            owned.push(rel.to_path_buf());
        }
        // skills/<name> dir itself (no trailing slash, for reconcile logic).
        owned.push(PathBuf::from("skills").join(&skill.name));
    }
    Ok(owned)
}

/// Scan `plugin_dir/skills/` and project each skill sub-directory into
/// `out/skills/<name>/` via [`write_first_class_skills`].
///
/// This enables adapters where `supports_plugins() == false` (e.g. Crush) to
/// still materialise skills bundled inside a plugin directory without fully
/// loading the plugin.
///
/// Returns the paths written (relative to `out`). Returns an empty vec when
/// `plugin_dir/skills/` does not exist.
///
/// # Errors
/// Propagates any I/O error from [`write_first_class_skills`].
pub(crate) fn project_plugin_skills(plugin_dir: &Path, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let skills_src = plugin_dir.join("skills");
    if !skills_src.is_dir() {
        return Ok(Vec::new());
    }
    let mut skills: Vec<crate::config::SkillSource> = Vec::new();
    for entry in std::fs::read_dir(&skills_src)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || crate::paths::is_unsafe_join_target(&name) {
            continue;
        }
        skills.push(crate::config::SkillSource {
            name,
            path: path.to_string_lossy().into_owned(),
            when: Vec::new(),
        });
    }
    write_first_class_skills(out, &skills)
}
