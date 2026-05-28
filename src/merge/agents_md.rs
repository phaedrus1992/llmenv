use std::fmt::Write as _;

/// Concatenate AGENTS.md fragments from each bundle with provenance comments.
///
/// Each fragment is preceded by a blank line and an HTML comment naming the
/// source bundle, e.g. `<!-- # from bundle: base -->`, so the resulting
/// document keeps round-trip provenance for the materializer.
#[must_use]
pub fn concat(parts: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, body) in parts {
        let _ = writeln!(out);
        let _ = writeln!(out, "<!-- # from bundle: {name} -->");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Concatenate AGENTS.md with rules-file bodies appended after, frontmatter
/// stripped. For adapters that don't have a native rules-directory convention
/// and must inline everything into a single rules file.
///
/// The rules section is preceded by an HTML comment naming the source rule
/// file (e.g. `<!-- # from bundle: base rules/rust.md -->`) so provenance is
/// preserved.
#[must_use]
pub fn concat_with_rules(parts: &[(String, String)], rules: &[super::rules::RuleFile]) -> String {
    let mut out = concat(parts);
    for r in rules {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "<!-- # from bundle: {} {} -->",
            r.bundle,
            r.rel.display()
        );
        out.push_str(&r.body);
        if !r.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(concat(&[]), "");
    }

    #[test]
    fn concat_with_rules_appends_bodies() {
        use super::super::rules::RuleFile;
        use std::path::PathBuf;
        let parts = vec![("base".into(), "# base\n".into())];
        let rules = vec![RuleFile {
            bundle: "base".into(),
            rel: PathBuf::from("rules/rust.md"),
            frontmatter: Some("scope: rust".into()),
            body: "# rust rules\n".into(),
            raw: "---\nscope: rust\n---\n# rust rules\n".into(),
        }];
        let s = concat_with_rules(&parts, &rules);
        assert!(s.contains("<!-- # from bundle: base -->"));
        assert!(s.contains("<!-- # from bundle: base rules/rust.md -->"));
        assert!(s.contains("# rust rules"));
        // Frontmatter must NOT leak into the concatenated output.
        assert!(!s.contains("scope: rust"));
    }

    #[test]
    fn each_part_gets_provenance_header() {
        let s = concat(&[
            ("base".into(), "# base\n".into()),
            ("rust".into(), "# rust".into()),
        ]);
        assert!(s.contains("<!-- # from bundle: base -->"));
        assert!(s.contains("<!-- # from bundle: rust -->"));
        // Trailing newline added when body lacks one:
        assert!(s.ends_with('\n'));
    }
}
