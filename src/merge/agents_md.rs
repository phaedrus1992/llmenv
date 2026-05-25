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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(concat(&[]), "");
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
