//! Shared skill-writing helpers used by adapters that materialise first-class
//! `skills:` entries and plugin-projected skills.
//!
//! Both [`super::claude_code::ClaudeCodeAdapter`] and
//! [`super::crush::CrushAdapter`] call these; the implementation lives here
//! so it is not duplicated. The helpers are `pub(crate)` — they are an
//! internal rendering detail, not part of the public [`super::AgentAdapter`]
//! contract.

use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

const IGNORE_INLINE: &str = "# llmenv-ignore: hardcoded-path";
const IGNORE_FILE: &str = "# llmenv-ignore-file: hardcoded-path";

/// Create a directory with owner-only permissions (0o700 on Unix, default on non-Unix).
/// Recursive — creates parent directories as needed.
pub(crate) fn create_dir_owner_only(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| anyhow::anyhow!("failed to create dir {}: {e}", dir.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("failed to create dir {}: {e}", dir.display()))
    }
}

/// Reject materialized content carrying a hardcoded `~/.claude` / `$HOME/.claude`
/// path (#311). Such paths resolve against the *default* config dir, so they
/// break whenever `CLAUDE_CONFIG_DIR` points at a materialized llmenv folder
/// (the normal case). `label` names the offending file in the error.
///
/// Uses `pulldown-cmark`'s CommonMark event stream so paths inside fenced code
/// blocks and inline code spans are correctly skipped — no fragile heuristics.
///
/// Suppression:
/// - `# llmenv-ignore-file: hardcoded-path` anywhere in the file skips the entire file.
/// - `# llmenv-ignore: hardcoded-path` at the end of a line skips that line only.
pub(crate) fn reject_hardcoded_config_path(content: &str, label: &str) -> anyhow::Result<()> {
    if content.contains(IGNORE_FILE) {
        return Ok(());
    }

    let mut in_code_block = false;
    for event in Parser::new(content) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Code(_) => {} // inline code — skip
            Event::Text(text) if !in_code_block => {
                for line in text.lines() {
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
            }
            _ => {}
        }
    }
    Ok(())
}

/// `name`/`description` are freeform one-line text per the SKILL.md spec, never
/// nested mappings — so a plain scalar value containing `: ` (e.g. `description:
/// Do X. Triggers on: Y, Z.`) is valid *intent* even though it isn't valid plain
/// YAML (an unquoted `: ` mid-scalar reads as a nested mapping key). Auto-quote
/// those two fields' values before reparsing so real-world descriptions aren't
/// rejected for using a colon (#568).
///
/// C0 control characters other than `\n`/`\r`/`\t` are not valid unescaped
/// inside a YAML double-quoted scalar, so they're hex-escaped (`\uXXXX`)
/// rather than passed through literally. Unicode noncharacters (see
/// [`is_yaml_noncharacter`]) get the same treatment.
fn quote_yaml_scalar(value: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || is_yaml_noncharacter(c) => {
                let cp = c as u32;
                // YAML's `\u` escape is fixed at 4 hex digits; code points
                // above the BMP (only reachable via noncharacters here, since
                // `is_control()` never exceeds U+009F) need the 8-digit `\U`
                // form instead, or the extra digits leak out as literal text.
                if cp > 0xFFFF {
                    let _ = write!(out, "\\U{cp:08x}");
                } else {
                    let _ = write!(out, "\\u{cp:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Unicode noncharacters (`U+FDD0..=U+FDEF` and `U+xFFFE`/`U+xFFFF` for every
/// plane `0..=0x10`, per the Unicode standard). Not `char::is_control()`, but
/// `serde_yaml`'s parser rejects at least U+FFFE as a bare control character
/// (#873), so `quote_yaml_scalar` hex-escapes them too.
fn is_yaml_noncharacter(c: char) -> bool {
    let cp = c as u32;
    matches!(cp, 0xFDD0..=0xFDEF) || (cp & 0xFFFE) == 0xFFFE
}

fn requote_name_and_description(frontmatter: &str) -> String {
    frontmatter
        .lines()
        .map(|line| {
            for key in ["name", "description"] {
                let Some(value) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
                    continue;
                };
                let value = value.trim_start();
                if value.is_empty() || matches!(value.as_bytes()[0], b'"' | b'\'' | b'|' | b'>') {
                    return line.to_string();
                }
                return format!("{key}: {}", quote_yaml_scalar(value));
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    let mapping = match serde_yaml::from_str::<serde_yaml::Mapping>(frontmatter_str) {
        Ok(mapping) => mapping,
        Err(e) => serde_yaml::from_str::<serde_yaml::Mapping>(&requote_name_and_description(
            frontmatter_str,
        ))
        .map_err(|_| {
            anyhow::anyhow!(
                "Skill {} SKILL.md has invalid YAML frontmatter: {e}",
                skill_dir.display()
            )
        })?,
    };
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
    // #918: a missing skills dir → nothing to validate; a permission error on
    // it propagates instead of an exists() stat masking it as absent, which
    // would silently bypass skill validation.
    let Some(entries) = crate::paths::read_dir_optional(&skills_dir)? else {
        return Ok(());
    };
    // Resolve the skills root once; every skill dir must stay inside it so a
    // symlink can't redirect validation (or the path scan) at a foreign file.
    let skills_root = skills_dir.canonicalize()?;

    for entry in entries {
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

        // #556: llmenv's own synthetic LSP plugin dir is a skills-directory plugin
        // (marked by `.claude-plugin/plugin.json`), not a skill — it needs no
        // SKILL.md and, being synthesized from typed config rather than copied
        // bundle files, no hardcoded-path scan either. Scoped to the exact
        // reserved name (not "any dir with that marker") so a plugin-sourced skill
        // can't use the same marker to bypass validation.
        if path.file_name()
            == Some(std::ffi::OsStr::new(
                crate::adapter::claude_code::LSP_PLUGIN_NAME,
            ))
            && path.join(".claude-plugin").join("plugin.json").exists()
        {
            continue;
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
        // #534: an allowlist (ASCII alphanumeric + '.'/'_'/'-') closes the gap
        // a traversal-only check leaves — no path separators, no control
        // characters, no Unicode formatting characters (zero-width space, RTL
        // override) — rather than enumerating what to reject.
        if !crate::paths::is_valid_short_name(&skill.name) {
            anyhow::bail!("unsafe skill name '{}': not a valid skill name", skill.name);
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
        if !crate::paths::is_valid_short_name(name) {
            anyhow::bail!("plugin skill '{name}': not a valid skill name");
        }
        skills.push(crate::config::SkillSource {
            name: name.to_string(),
            path: path.to_string_lossy().into_owned(),
            when: Vec::new(),
        });
    }
    write_first_class_skills(out, &skills)
}

/// Recursively-shaped arbitrary YAML for fragment fuzzing, shared by adapter test
/// modules (`claude_code.rs`, `crush.rs`) that need to fuzz native-fragment merging.
/// Bounded depth keeps generation cheap while still exercising nested
/// mappings/sequences.
#[cfg(test)]
pub(crate) fn arb_yaml_value(
    depth: u32,
) -> impl proptest::prelude::Strategy<Value = serde_yaml::Value> {
    use proptest::prelude::*;
    let leaf = prop_oneof![
        Just(serde_yaml::Value::Null),
        any::<bool>().prop_map(serde_yaml::Value::Bool),
        any::<i64>().prop_map(|n| serde_yaml::Value::Number(n.into())),
        "[a-z]{0,8}".prop_map(serde_yaml::Value::String),
    ];
    leaf.prop_recursive(depth, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_yaml::Value::Sequence),
            proptest::collection::vec(("[a-z]{1,6}", inner), 0..4).prop_map(|pairs| {
                let mut m = serde_yaml::Mapping::new();
                for (k, v) in pairs {
                    m.insert(serde_yaml::Value::String(k), v);
                }
                serde_yaml::Value::Mapping(m)
            }),
        ]
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    #![allow(clippy::unwrap_used)]
    use proptest::prelude::*;

    use super::*;

    // #918: an unreadable skills dir must make validation FAIL LOUDLY, not be
    // silently skipped the way the old `exists()` guard did (which would let an
    // EACCES bypass skill validation entirely).
    #[cfg(unix)]
    #[test]
    fn validate_skills_propagates_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir(&skills_dir).unwrap();
        std::fs::set_permissions(&skills_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = validate_skills(tmp.path());
        let readable_anyway = std::fs::read_dir(&skills_dir).is_ok();
        std::fs::set_permissions(&skills_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "unreadable skills dir must fail validation, got {result:?}"
        );
    }

    #[test]
    fn validate_skills_ok_when_dir_absent() {
        // Missing skills dir → nothing to validate.
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate_skills(tmp.path()).is_ok());
    }

    proptest! {
        /// `name`/`description` values come from repo-authored SKILL.md
        /// frontmatter — still arbitrary text as far as this function is
        /// concerned. No input should make the escaping panic. `(?s)` so the
        /// generator can also produce embedded newlines, exercising the
        /// `'\n'` escape arm.
        #[test]
        fn quote_yaml_scalar_never_panics(value in "(?s).{0,200}") {
            let _ = quote_yaml_scalar(&value);
        }

        /// The escaped output, dropped into a YAML mapping, must parse back
        /// to exactly the original string — no unescaped quote or backslash
        /// may leak out of the scalar and corrupt the surrounding YAML (#568).
        #[test]
        fn quote_yaml_scalar_round_trips(value in "(?s).{0,200}") {
            let quoted = quote_yaml_scalar(&value);
            let yaml = format!("key: {quoted}");
            let mapping: serde_yaml::Mapping =
                serde_yaml::from_str(&yaml).expect("quoted scalar must parse as valid YAML");
            let parsed = mapping
                .get("key")
                .and_then(serde_yaml::Value::as_str)
                .expect("key must be a string scalar");
            prop_assert_eq!(parsed, value.as_str());
        }

        /// `is_yaml_noncharacter`'s bit-trick (`(cp & 0xFFFE) == 0xFFFE`) must
        /// agree with a naive reference built directly from the Unicode
        /// noncharacter ranges, for any `char` — not just the finite set the
        /// exhaustive test below enumerates. Also guards the inline claim
        /// that noncharacters and `is_control()` are disjoint sets (#873).
        #[test]
        fn is_yaml_noncharacter_matches_reference_definition(c in proptest::char::any()) {
            let cp = c as u32;
            let naive = (0xFDD0..=0xFDEF).contains(&cp)
                || (0..=0x10u32).any(|plane| {
                    let base = plane * 0x1_0000;
                    cp == base + 0xFFFE || cp == base + 0xFFFF
                });
            prop_assert_eq!(is_yaml_noncharacter(c), naive);
            prop_assert!(!(c.is_control() && is_yaml_noncharacter(c)));
        }
    }

    /// Unicode noncharacters (#873): `U+FDD0..=U+FDEF` plus the last two code
    /// points of every plane (`U+xFFFE`/`U+xFFFF` for planes 0..=0x10). These
    /// are not `char::is_control()`, but `serde_yaml`'s parser rejects at
    /// least U+FFFE as a bare control character, so they must be escaped like
    /// control characters for the scalar to round-trip.
    #[test]
    fn quote_yaml_scalar_escapes_noncharacters() {
        let noncharacters = (0xFDD0u32..=0xFDEF)
            .chain((0..=0x10u32).flat_map(|plane| {
                let base = plane * 0x1_0000;
                [base + 0xFFFE, base + 0xFFFF]
            }))
            .map(|cp| char::from_u32(cp).expect("all noncharacter code points are valid chars"));

        for c in noncharacters {
            let value = c.to_string();
            let quoted = quote_yaml_scalar(&value);
            let yaml = format!("key: {quoted}");
            let result = serde_yaml::from_str::<serde_yaml::Mapping>(&yaml);
            assert!(
                result.is_ok(),
                "noncharacter U+{:04X} must round-trip: {:?}",
                c as u32,
                result.as_ref().err()
            );
            let mapping = result.expect("checked by assert! above");
            let parsed = mapping
                .get("key")
                .and_then(serde_yaml::Value::as_str)
                .expect("key must be a string scalar");
            assert_eq!(
                parsed, value,
                "noncharacter U+{:04X} round-trip mismatch",
                c as u32
            );
        }
    }
}
