use std::fmt::Write as _;

/// Concatenate AGENTS.md fragments from each bundle with provenance comments.
///
/// Each fragment is preceded by a blank line and an HTML comment naming the
/// source bundle, e.g. `<!-- # from bundle: base -->`, so the resulting
/// document keeps round-trip provenance for the materializer.
#[must_use]
pub(crate) fn concat(parts: &[(String, String)]) -> String {
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

/// Append rules-file bodies to `base` (an already-concatenated AGENTS.md, as
/// [`concat`] produces), frontmatter stripped. For adapters with no native
/// rules-directory convention that must inline everything into a single
/// instructions file — Codex's `CodexAdapter` (#1103): Codex has no per-file
/// rule mechanism with glob frontmatter, so a rule's path-scoped, conditional
/// application becomes unconditional prose once folded in here.
///
/// The rules section is preceded by an HTML comment naming the source rule
/// file (e.g. `<!-- # from bundle: base rules/rust.md -->`) so provenance is
/// preserved.
#[must_use]
pub(crate) fn append_rules(base: &str, rules: &[super::rules::RuleFile]) -> String {
    let mut out = base.to_string();
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
    use proptest::prelude::*;

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(concat(&[]), "");
    }

    #[test]
    fn append_rules_appends_bodies_to_the_concatenated_base() {
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
        let s = append_rules(&concat(&parts), &rules);
        assert!(s.contains("<!-- # from bundle: base -->"));
        assert!(s.contains("<!-- # from bundle: base rules/rust.md -->"));
        assert!(s.contains("# rust rules"));
        // Frontmatter must NOT leak into the concatenated output.
        assert!(!s.contains("scope: rust"));
    }

    /// A base with no rules at all must round-trip unchanged.
    #[test]
    fn append_rules_is_a_noop_with_no_rules() {
        let base = concat(&[("base".into(), "# base\n".into())]);
        assert_eq!(append_rules(&base, &[]), base);
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

    proptest! {
        /// `append_rules` never truncates or mutates `base`, and every rule's
        /// body and provenance marker survive in the output, for an arbitrary
        /// base string and an arbitrary set of rules (pbt-gap, #1103).
        #[test]
        fn prop_append_rules_preserves_base_and_every_rule_body(
            base in "[a-zA-Z0-9 \n#.-]{0,40}",
            rule_data in proptest::collection::vec(
                (
                    "[a-z][a-z0-9_]{0,8}",
                    "[a-z0-9/_.-]{1,12}",
                    "[a-zA-Z0-9 \n]{0,20}",
                ),
                0..5,
            )
        ) {
            use super::super::rules::RuleFile;
            use std::path::PathBuf;

            let rules: Vec<RuleFile> = rule_data
                .into_iter()
                .map(|(bundle, rel, body)| RuleFile {
                    bundle,
                    rel: PathBuf::from(rel),
                    frontmatter: None,
                    body,
                    raw: String::new(),
                })
                .collect();

            let out = append_rules(&base, &rules);

            prop_assert!(
                out.starts_with(&base),
                "append_rules must never mutate or truncate the caller's base content"
            );

            for rule in &rules {
                prop_assert!(
                    out.contains(&rule.body),
                    "every rule's body must survive verbatim in the output"
                );
                let marker = format!("<!-- # from bundle: {} {} -->", rule.bundle, rule.rel.display());
                prop_assert!(
                    out.contains(&marker),
                    "every rule's provenance marker must appear in the output"
                );
            }

            if rules.is_empty() {
                prop_assert_eq!(out, base, "no rules must leave the base exactly as-is");
            }
        }
    }
}
