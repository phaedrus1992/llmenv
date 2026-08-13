//! Materializes `capabilities.output_styles` entries (#1130): natively for
//! Claude Code (`output-styles/<name>.md` + the `outputStyle` settings key,
//! wired in `super::claude_code`), and as a generated skill
//! (`skills/<name>/SKILL.md`) for every other adapter, since only Claude
//! Code has a native output-style concept
//! ([`super::AgentAdapter::supports_output_styles`]).

use std::path::{Path, PathBuf};

use crate::config::OutputStyle;
use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};

/// Rejects an output style name that collides with a skill name a plugin
/// would project via its own `skills/` directory (#1333). The
/// `materialize_from_manifest` collision check in `cli/mod.rs` only sees
/// `capabilities.skills` and reserved built-in names — plugin-projected
/// names are resolved from on-disk plugin content that isn't available at
/// that point, so this must run per-adapter, right before the
/// generated-skill fallback (`write_output_style_as_skill`) writes anything.
///
/// `is_compatible` must be the *same* predicate the caller's own plugin
/// projection loop uses to decide whether to skip a plugin entirely (e.g.
/// Crush skips a plugin with an `agents/`/`commands/`/`hooks/` directory it
/// can't express). Passing a different or looser predicate here than the
/// caller's projection loop uses would count skills from a plugin that is
/// never actually projected, hard-failing `materialize` over a collision
/// that can't happen. Pass `|_| true` for an adapter with no such filter
/// (opencode).
///
/// No-op when `styles` or `plugins` is empty.
///
/// # Errors
/// Returns an error naming the colliding style, or propagates a plugin
/// resolution/I/O failure from resolving a plugin's skill directory.
pub(crate) fn reject_plugin_skill_collisions(
    styles: &[OutputStyle],
    plugins: &[ResolvedPlugin],
    marketplaces: &[ResolvedMarketplace],
    is_compatible: impl Fn(&Path) -> bool,
) -> anyhow::Result<()> {
    if styles.is_empty() || plugins.is_empty() {
        return Ok(());
    }
    // Keyed lowercase (#1333 security-audit): both render to a single
    // `skills/<name>/` directory entry, which collides on a case-insensitive
    // filesystem (macOS, Windows) even when the two names differ in case.
    let mut plugin_skill_names: std::collections::HashMap<String, &str> =
        std::collections::HashMap::new();
    for plugin in plugins {
        let payload = super::resolve_plugin_payload(plugin, marketplaces)?;
        if !is_compatible(&payload) {
            continue;
        }
        for name in super::skills::plugin_skill_names(&payload)? {
            plugin_skill_names
                .entry(name.to_lowercase())
                .or_insert(&plugin.plugin);
        }
    }
    for style in styles {
        if let Some(plugin_name) = plugin_skill_names.get(&style.name.to_lowercase()) {
            anyhow::bail!(
                "output style '{}' collides (case-insensitive) with a skill projected from \
                 plugin '{}'; rename the style to avoid silently overwriting or being \
                 shadowed by that skill on adapters with no native output-style concept \
                 (Crush, opencode)",
                style.name,
                plugin_name,
            );
        }
    }
    Ok(())
}

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

    // #1130 (security-audit P2): only the file goes in the owned set. Ghost
    // reconciliation removes stale owned paths with `remove_file`, which
    // can't remove a directory — recording the bare dir here would leave an
    // empty `skills/<name>/` behind after the style is removed, and that
    // empty dir then hard-fails `validate_skills` on the next render.
    Ok(vec![
        PathBuf::from("skills").join(&style.name).join("SKILL.md"),
    ])
}

/// Render `styles` (already tag-filtered/deduped by
/// `materialize_from_manifest`) natively for Claude Code: each into
/// `out/output-styles/<name>.md`.
///
/// The `outputStyle` settings.json selector is a separate concern, computed
/// independently in `super::claude_code::generate_settings_json` from the
/// same `manifest.capabilities.output_styles` — see that function's doc
/// comment for why zero/multiple selectable styles leaves the selector
/// unset rather than erroring.
///
/// Returns the paths written, relative to `out`.
///
/// # Errors
/// Returns an error for an unsafe style name, a hardcoded `~/.claude` path
/// in `content`, or an I/O failure.
pub(crate) fn write_native_output_styles(
    out: &Path,
    styles: &[OutputStyle],
) -> anyhow::Result<Vec<PathBuf>> {
    if styles.is_empty() {
        return Ok(Vec::new());
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

    Ok(owned)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::{
        reject_plugin_skill_collisions, write_native_output_styles, write_output_style_as_skill,
    };
    use crate::config::OutputStyle;
    use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};

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

    fn plugin_with_skill(
        skill_name: &str,
    ) -> (tempfile::TempDir, ResolvedPlugin, Vec<ResolvedMarketplace>) {
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join("skills").join(skill_name)).unwrap();
        std::fs::write(
            plugin_dir
                .path()
                .join("skills")
                .join(skill_name)
                .join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: A plugin skill.\n---\n# {skill_name}\n"),
        )
        .unwrap();
        let plugin = ResolvedPlugin {
            marketplace: "local".into(),
            plugin: "my-plugin".into(),
            collection: String::new(),
            install_path: Some(plugin_dir.path().to_string_lossy().into_owned()),
            git_commit_sha: None,
        };
        (plugin_dir, plugin, Vec::new())
    }

    #[test]
    fn reject_plugin_skill_collisions_catches_case_insensitive_match() {
        let (_dir, plugin, marketplaces) = plugin_with_skill("foo");
        let err = reject_plugin_skill_collisions(
            &[style("Foo")],
            std::slice::from_ref(&plugin),
            &marketplaces,
            |_| true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Foo"));
        assert!(err.to_string().contains("my-plugin"));
    }

    #[test]
    fn reject_plugin_skill_collisions_skips_incompatible_plugin() {
        let (_dir, plugin, marketplaces) = plugin_with_skill("foo");
        // is_compatible always false: the caller's projection loop would
        // never write this plugin's skills, so no collision is possible.
        reject_plugin_skill_collisions(
            &[style("foo")],
            std::slice::from_ref(&plugin),
            &marketplaces,
            |_| false,
        )
        .unwrap();
    }

    #[test]
    fn reject_plugin_skill_collisions_no_collision_is_ok() {
        let (_dir, plugin, marketplaces) = plugin_with_skill("foo");
        reject_plugin_skill_collisions(
            &[style("bar")],
            std::slice::from_ref(&plugin),
            &marketplaces,
            |_| true,
        )
        .unwrap();
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
        let owned = write_native_output_styles(tmp.path(), &[s]).unwrap();
        let content =
            std::fs::read_to_string(tmp.path().join("output-styles/explanatory.md")).unwrap();
        assert!(content.contains("keep-coding-instructions: true"));
        assert!(owned.contains(&std::path::PathBuf::from("output-styles/explanatory.md")));
    }

    #[test]
    fn write_native_output_styles_omits_keep_coding_instructions_when_false() {
        let tmp = tempfile::tempdir().unwrap();
        write_native_output_styles(tmp.path(), &[style("plain")]).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("output-styles/plain.md")).unwrap();
        assert!(!content.contains("keep-coding-instructions"));
    }

    /// #1130 (overengineering-reviewer): the `outputStyle` selector decision
    /// (one vs. multiple vs. zero selectable styles) lives entirely in
    /// `claude_code::generate_settings_json` now — see
    /// `output_style_selects_when_exactly_one_active` and its siblings there.
    /// This function only ever writes files; `force_for_plugin` styles still
    /// get one, same as any other.
    #[test]
    fn write_native_output_styles_writes_force_for_plugin_style_too() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = style("plugin-style");
        s.force_for_plugin = true;
        let owned = write_native_output_styles(tmp.path(), &[s]).unwrap();
        assert!(owned.contains(&std::path::PathBuf::from("output-styles/plugin-style.md")));
    }

    #[test]
    fn write_native_output_styles_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let owned = write_native_output_styles(tmp.path(), &[]).unwrap();
        assert!(owned.is_empty());
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
