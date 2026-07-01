//! Shared skill-writing helpers used by adapters that materialise first-class
//! `skills:` entries and plugin-projected skills.
//!
//! Both [`super::claude_code::ClaudeCodeAdapter`] and
//! [`super::crush::CrushAdapter`] call these; the implementation lives here
//! so it is not duplicated. The helpers are `pub(crate)` — they are an
//! internal rendering detail, not part of the public [`super::AgentAdapter`]
//! contract.

use std::path::{Path, PathBuf};

const IGNORE_INLINE: &str = "# llmenv-ignore: hardcoded-path";
const IGNORE_FILE: &str = "# llmenv-ignore-file: hardcoded-path";

/// Reject materialized content carrying a hardcoded `~/.claude` / `$HOME/.claude`
/// path (#311). Such paths resolve against the *default* config dir, so they
/// break whenever `CLAUDE_CONFIG_DIR` points at a materialized llmenv folder
/// (the normal case). `label` names the offending file in the error.
///
/// Suppression:
/// - `# llmenv-ignore-file: hardcoded-path` anywhere in the file skips the entire file.
/// - `# llmenv-ignore: hardcoded-path` at the end of a line skips that line only.
fn reject_hardcoded_config_path(content: &str, label: &str) -> anyhow::Result<()> {
    if content.contains(IGNORE_FILE) {
        return Ok(());
    }
    for line in content.lines() {
        if line.contains(IGNORE_INLINE) {
            continue;
        }
        if line.contains("~/.claude") || line.contains("$HOME/.claude") {
            anyhow::bail!(
                "{label} contains hardcoded ~/.claude or $HOME/.claude paths. \
                 Use ${{CLAUDE_PLUGIN_ROOT}} or relative paths instead so it \
                 works when CLAUDE_CONFIG_DIR is set to a materialized llmenv folder."
            );
        }
    }
    Ok(())
}

/// Validate a single skill's `SKILL.md` frontmatter (name + description present).
fn validate_skill_frontmatter(skill_md: &Path, skill_dir: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(skill_md)?;
    let Some(frontmatter_end) = content.find("\n---\n").or_else(|| {
        content
            .ends_with("---")
            .then(|| content.len().saturating_sub(3))
    }) else {
        anyhow::bail!(
            "Skill {} SKILL.md missing YAML frontmatter (must start with --- and end with ---)",
            skill_dir.display()
        );
    };
    let frontmatter_str = &content[3..frontmatter_end];
    let mapping = serde_yaml::from_str::<serde_yaml::Mapping>(frontmatter_str).map_err(|e| {
        anyhow::anyhow!(
            "Skill {} SKILL.md has invalid YAML frontmatter: {e}",
            skill_dir.display()
        )
    })?;
    if mapping.get("name").is_none() || mapping.get("description").is_none() {
        anyhow::bail!(
            "Skill {} SKILL.md missing required frontmatter fields (name and description)",
            skill_dir.display()
        );
    }
    Ok(())
}

/// Scan every readable text file under `dir` (recursively) for hardcoded config
/// paths (#311). Covers scripts and helper files, not just SKILL.md. Symlinks
/// are not followed (the caller verified `dir` itself is in-bounds), and
/// non-UTF-8 files (binaries) are skipped — only text can carry a flaggable path.
fn scan_skill_files_for_hardcoded_paths(dir: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            scan_skill_files_for_hardcoded_paths(&path)?;
        } else if meta.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            reject_hardcoded_config_path(&content, &path.display().to_string())?;
        }
    }
    Ok(())
}

/// Validates that all skills in the materialized directory have SKILL.md with
/// required frontmatter and carry no hardcoded `~/.claude` paths (#311).
///
/// Called by both `ClaudeCodeAdapter` and `CrushAdapter` after writing skills.
pub(crate) fn validate_skills(out: &Path) -> anyhow::Result<()> {
    let skills_dir = out.join("skills");
    if !skills_dir.exists() {
        return Ok(());
    }
    // Resolve the skills root once; every skill dir must stay inside it so a
    // symlink can't redirect validation (or the path scan) at a foreign file.
    let skills_root = skills_dir.canonicalize()?;

    for entry in std::fs::read_dir(&skills_dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&skills_root) {
            anyhow::bail!(
                "skill path {} escapes the skills directory (symlink?); refusing to validate",
                path.display()
            );
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            anyhow::bail!("Skill directory {} missing SKILL.md", path.display());
        }
        validate_skill_frontmatter(&skill_md, &path)?;
        scan_skill_files_for_hardcoded_paths(&canonical)?;
    }

    Ok(())
}

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
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "plugin skill directory has a non-UTF-8 name: {}",
                path.display()
            )
        })?;
        // Fail loud rather than silently drop — matches write_first_class_skills.
        if name.is_empty() || crate::paths::is_unsafe_join_target(name) {
            anyhow::bail!(
                "plugin skill '{name}': unsafe name (contains path-traversal components)"
            );
        }
        skills.push(crate::config::SkillSource {
            name: name.to_string(),
            path: path.to_string_lossy().into_owned(),
            when: Vec::new(),
        });
    }
    write_first_class_skills(out, &skills)
}
