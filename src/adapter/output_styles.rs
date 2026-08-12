//! Materializes `capabilities.output_styles` entries (#1130): natively for
//! Claude Code (`output-styles/<name>.md` + the `outputStyle` settings key,
//! wired in `super::claude_code`), and as a generated skill
//! (`skills/<name>/SKILL.md`) for every other adapter, since only Claude
//! Code has a native output-style concept
//! ([`super::AgentAdapter::supports_output_styles`]).

use std::path::{Path, PathBuf};

use crate::config::OutputStyle;

/// Render one `OutputStyle` as a generated skill for adapters with
/// `supports_output_styles() == false`. Reuses the same YAML-scalar
/// escaping and hardcoded-path rejection [`super::skills`] applies to
/// user-authored skills — `validate_skills` (called by every adapter right
/// after its own skill-writing calls) also revalidates this output
/// generically, since it treats any `skills/<name>/SKILL.md` uniformly.
///
/// Returns the paths written, relative to `out`.
///
/// # Errors
/// Returns an error for an unsafe style name, a hardcoded `~/.claude` path
/// in `content`, or an I/O failure.
pub(crate) fn write_output_style_as_skill(
    out: &Path,
    style: &OutputStyle,
) -> anyhow::Result<Vec<PathBuf>> {
    if !crate::paths::is_valid_short_name(&style.name) {
        anyhow::bail!(
            "unsafe output style name '{}': not a valid skill name",
            style.name
        );
    }

    let skill_dir = out.join("skills").join(&style.name);
    super::skills::create_dir_owner_only(&skill_dir)?;

    let skill_md = format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}\n",
        super::skills::quote_yaml_scalar(&style.name),
        super::skills::quote_yaml_scalar(&style.description),
        style.content.trim_end(),
    );
    super::skills::reject_hardcoded_config_path(
        &skill_md,
        &format!("output style '{}' (generated skill)", style.name),
    )?;

    crate::paths::write_owner_only(&skill_dir.join("SKILL.md"), skill_md.as_bytes())?;

    Ok(vec![
        PathBuf::from("skills").join(&style.name).join("SKILL.md"),
        PathBuf::from("skills").join(&style.name),
    ])
}

/// Render `styles` (already tag-filtered/deduped by
/// `materialize_from_manifest`) natively for Claude Code: each into
/// `out/output-styles/<name>.md`.
///
/// Also returns, when exactly one non-`force_for_plugin` style is present,
/// that style's name — [`super::claude_code::generate_settings_json`] sets
/// `outputStyle` to it. Zero or more-than-one selectable styles leaves the
/// selector untouched rather than erroring: unlike `Memory`/`CodebaseMemory`
/// (which resolve to one MCP registration slot with no valid multi-active
/// representation), Claude Code holds multiple style *files* simultaneously
/// without conflict — only the *selector* is single-valued, and an unset
/// selector is a safe, meaningful state (Claude Code keeps whatever's
/// already configured).
///
/// Returns the paths written, relative to `out`.
///
/// # Errors
/// Returns an error for an unsafe style name, a hardcoded `~/.claude` path
/// in `content`, or an I/O failure.
pub(crate) fn write_native_output_styles(
    out: &Path,
    styles: &[OutputStyle],
) -> anyhow::Result<(Vec<PathBuf>, Option<String>)> {
    if styles.is_empty() {
        return Ok((Vec::new(), None));
    }

    let styles_dir = out.join("output-styles");
    super::skills::create_dir_owner_only(&styles_dir)?;

    let mut owned = Vec::new();
    for style in styles {
        if !crate::paths::is_valid_short_name(&style.name) {
            anyhow::bail!(
                "unsafe output style name '{}': not a valid file name",
                style.name
            );
        }

        let mut content = format!(
            "---\nname: {}\ndescription: {}\n",
            super::skills::quote_yaml_scalar(&style.name),
            super::skills::quote_yaml_scalar(&style.description),
        );
        if style.keep_coding_instructions {
            content.push_str("keep-coding-instructions: true\n");
        }
        content.push_str("---\n\n");
        content.push_str(style.content.trim_end());
        content.push('\n');
        super::skills::reject_hardcoded_config_path(
            &content,
            &format!("output style '{}'", style.name),
        )?;

        let rel = PathBuf::from("output-styles").join(format!("{}.md", style.name));
        crate::paths::write_owner_only(&out.join(&rel), content.as_bytes())?;
        owned.push(rel);
    }

    let selectable: Vec<&str> = styles
        .iter()
        .filter(|o| !o.force_for_plugin)
        .map(|o| o.name.as_str())
        .collect();
    let selected = match selectable[..] {
        [name] => Some(name.to_string()),
        _ => None,
    };
    Ok((owned, selected))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::{write_native_output_styles, write_output_style_as_skill};
    use crate::config::OutputStyle;

    fn style(name: &str) -> OutputStyle {
        OutputStyle {
            name: name.to_string(),
            description: "A test style".to_string(),
            content: "Be terse.".to_string(),
            when: Vec::new(),
            keep_coding_instructions: false,
            force_for_plugin: false,
        }
    }

    #[test]
    fn write_output_style_as_skill_writes_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_output_style_as_skill(tmp.path(), &style("concise")).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("skills/concise/SKILL.md")).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: \"concise\""));
        assert!(content.contains("description: \"A test style\""));
        assert!(content.contains("Be terse."));
    }

    #[test]
    fn write_output_style_as_skill_rejects_unsafe_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = write_output_style_as_skill(tmp.path(), &style("../escape")).unwrap_err();
        assert!(err.to_string().contains("unsafe"));
    }

    #[test]
    fn write_output_style_as_skill_rejects_hardcoded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = style("bad");
        s.content = "See ~/.claude/settings.json for details.".to_string();
        let err = write_output_style_as_skill(tmp.path(), &s).unwrap_err();
        assert!(err.to_string().contains("hardcoded"));
    }

    #[test]
    fn write_native_output_styles_writes_frontmatter_with_keep_coding_instructions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = style("explanatory");
        s.keep_coding_instructions = true;
        let (owned, selected) = write_native_output_styles(tmp.path(), &[s]).unwrap();
        let content =
            std::fs::read_to_string(tmp.path().join("output-styles/explanatory.md")).unwrap();
        assert!(content.contains("keep-coding-instructions: true"));
        assert_eq!(selected, Some("explanatory".to_string()));
        assert!(owned.contains(&std::path::PathBuf::from("output-styles/explanatory.md")));
    }

    #[test]
    fn write_native_output_styles_omits_keep_coding_instructions_when_false() {
        let tmp = tempfile::tempdir().unwrap();
        write_native_output_styles(tmp.path(), &[style("plain")]).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("output-styles/plain.md")).unwrap();
        assert!(!content.contains("keep-coding-instructions"));
    }

    #[test]
    fn write_native_output_styles_selects_the_one_non_plugin_style() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, selected) = write_native_output_styles(tmp.path(), &[style("only")]).unwrap();
        assert_eq!(selected, Some("only".to_string()));
    }

    #[test]
    fn write_native_output_styles_no_selection_when_multiple_selectable() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, selected) =
            write_native_output_styles(tmp.path(), &[style("a"), style("b")]).unwrap();
        assert_eq!(selected, None);
    }

    #[test]
    fn write_native_output_styles_no_selection_when_zero_selectable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = style("plugin-style");
        s.force_for_plugin = true;
        let (owned, selected) = write_native_output_styles(tmp.path(), &[s]).unwrap();
        assert_eq!(selected, None);
        // Still written as a file even though it's not selected.
        assert!(owned.contains(&std::path::PathBuf::from("output-styles/plugin-style.md")));
    }

    #[test]
    fn write_native_output_styles_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (owned, selected) = write_native_output_styles(tmp.path(), &[]).unwrap();
        assert!(owned.is_empty());
        assert_eq!(selected, None);
        assert!(!tmp.path().join("output-styles").exists());
    }

    #[test]
    fn write_native_output_styles_rejects_unsafe_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = write_native_output_styles(tmp.path(), &[style("../escape")]).unwrap_err();
        assert!(err.to_string().contains("unsafe"));
    }

    #[test]
    fn write_native_output_styles_rejects_hardcoded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = style("bad");
        s.content = "See ~/.claude/settings.json for details.".to_string();
        let err = write_native_output_styles(tmp.path(), &[s]).unwrap_err();
        assert!(err.to_string().contains("hardcoded"));
    }
}
