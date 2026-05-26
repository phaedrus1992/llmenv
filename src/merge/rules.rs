//! Bundle `rules/` directory ingestion.
//!
//! Each bundle may contain a `rules/` subtree of `.md` files. Files may begin
//! with an optional YAML frontmatter block delimited by `---\n…\n---\n`. The
//! merge stage parses these into [`RuleFile`] values so the per-agent adapter
//! can pick its own strategy:
//!
//! * **Claude Code** copies each rule verbatim to `<out>/rules/<rel>` —
//!   frontmatter preserved — so Claude's own rules-directory convention
//!   discovers them.
//! * **AGENTS.md-only adapters** append `body` (frontmatter stripped) into
//!   the concatenated rules file via [`agents_md_with_rules`](super::agents_md::concat_with_rules).
//!
//! The split lives in the merge stage (not the adapter) so the cache hash
//! covers a single canonical representation regardless of which adapter
//! renders later.

use std::path::{Path, PathBuf};

/// A single rule file resolved from a bundle's `rules/` subtree.
#[derive(Debug, Clone)]
pub struct RuleFile {
    /// Owning bundle name — used for provenance comments and diagnostics.
    pub bundle: String,
    /// Path relative to the bundle root (e.g. `rules/rust.md`). Preserves
    /// subdirectory structure under `rules/`.
    pub rel: PathBuf,
    /// Raw frontmatter text between the `---` fences, exclusive. `None`
    /// when the file has no frontmatter block.
    pub frontmatter: Option<String>,
    /// File body with the frontmatter block removed. The leading newline
    /// after the closing `---` fence is also stripped so the body starts
    /// at meaningful content.
    pub body: String,
    /// Raw file contents — frontmatter + body — for adapters that want to
    /// pass the file through verbatim.
    pub raw: String,
}

/// Walk `<bundle_root>/rules/` and return all `.md` files. Non-`.md` files
/// are ignored; symlinks are skipped (consistent with [`super::merge`]).
/// Missing `rules/` directory yields an empty Vec.
pub fn collect_from_bundle(bundle_root: &Path, bundle_name: &str) -> anyhow::Result<Vec<RuleFile>> {
    let dir = bundle_root.join("rules");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk(bundle_root, &dir, bundle_name, &mut out)?;
    // Sort by relative path so order is deterministic across filesystems.
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn walk(
    bundle_root: &Path,
    dir: &Path,
    bundle_name: &str,
    out: &mut Vec<RuleFile>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let p = entry.path();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk(bundle_root, &p, bundle_name, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if p.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let raw = std::fs::read_to_string(&p)?;
        let (frontmatter, body) = split_frontmatter(&raw);
        let rel = p
            .strip_prefix(bundle_root)
            .map_err(|e| anyhow::anyhow!("path {} not under bundle root: {e}", p.display()))?
            .to_path_buf();
        out.push(RuleFile {
            bundle: bundle_name.to_owned(),
            rel,
            frontmatter,
            body,
            raw,
        });
    }
    Ok(())
}

/// Split a file into (frontmatter, body). Frontmatter must start at byte 0
/// with `---\n` and be closed by a line containing only `---`. Any other
/// shape (no leading fence, missing closer, etc.) yields `(None, raw)`.
fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, raw.to_owned());
    };
    // Look for a `---` line. Accept either `\n---\n` (closer with content
    // following) or trailing `\n---` at EOF (closer with nothing after).
    if let Some(end) = rest.find("\n---\n") {
        let fm = rest[..end].to_owned();
        let body = rest[end + "\n---\n".len()..].to_owned();
        return (Some(fm), body);
    }
    if let Some(stripped) = rest.strip_suffix("\n---") {
        return (Some(stripped.to_owned()), String::new());
    }
    // Unterminated frontmatter — treat as plain body so we don't lose data.
    (None, raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_passes_through() {
        let (fm, body) = split_frontmatter("# Title\nstuff\n");
        assert!(fm.is_none());
        assert_eq!(body, "# Title\nstuff\n");
    }

    #[test]
    fn frontmatter_with_body() {
        let raw = "---\nscope: rust\ntags: [a, b]\n---\n# Body\ntext\n";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm.as_deref(), Some("scope: rust\ntags: [a, b]"));
        assert_eq!(body, "# Body\ntext\n");
    }

    #[test]
    fn frontmatter_at_eof() {
        let raw = "---\nscope: rust\n---";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm.as_deref(), Some("scope: rust"));
        assert_eq!(body, "");
    }

    #[test]
    fn unterminated_frontmatter_falls_back_to_plain() {
        let raw = "---\nscope: rust\nno closer here\n";
        let (fm, body) = split_frontmatter(raw);
        assert!(fm.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn empty_file_has_no_frontmatter() {
        let (fm, body) = split_frontmatter("");
        assert!(fm.is_none());
        assert_eq!(body, "");
    }

    #[test]
    fn collect_skips_missing_rules_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let rules = collect_from_bundle(tmp.path(), "x").unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn collect_finds_and_parses_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(rules_dir.join("nested")).unwrap();
        std::fs::write(rules_dir.join("a.md"), "---\nscope: rust\n---\n# A\n").unwrap();
        std::fs::write(rules_dir.join("b.md"), "# B no fm\n").unwrap();
        std::fs::write(rules_dir.join("nested/c.md"), "# C\n").unwrap();
        std::fs::write(rules_dir.join("ignored.txt"), "skipped\n").unwrap();

        let rules = collect_from_bundle(tmp.path(), "pkg").unwrap();
        assert_eq!(rules.len(), 3);
        // Sorted by rel path.
        assert_eq!(rules[0].rel, Path::new("rules/a.md"));
        assert_eq!(rules[1].rel, Path::new("rules/b.md"));
        assert_eq!(rules[2].rel, Path::new("rules/nested/c.md"));
        assert_eq!(rules[0].frontmatter.as_deref(), Some("scope: rust"));
        assert_eq!(rules[0].body, "# A\n");
        assert!(rules[1].frontmatter.is_none());
        assert_eq!(rules[0].bundle, "pkg");
    }
}
