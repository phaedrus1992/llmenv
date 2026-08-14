pub(crate) use llmenv_util::normalize_yaml;
pub use llmenv_util::{dedup, merge_json, merge_yaml};

/// Escape every C0/C1 control character in `s` (`char::is_control`) so it's
/// safe to print verbatim to a terminal (#1076). Config-derived strings
/// (`native_permissions.*` keys, permission tool names and patterns,
/// bundle/marketplace names) can originate from a shared or marketplace
/// `bundle.yaml` since #1072 widened validation to the merged manifest — a
/// key or pattern containing an ANSI escape or a carriage return could
/// otherwise rewrite or hide surrounding `doctor`/`export` output on
/// someone else's terminal. No exceptions for `\n`/`\t`: a legitimate
/// config key, tool name, or pattern has no reason to contain either.
///
/// Returns `s` unchanged (no allocation) when nothing needs escaping.
pub(crate) fn display_safe(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_safe_returns_borrowed_for_plain_text() {
        assert!(matches!(
            display_safe("plain text"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn display_safe_escapes_ansi_escape() {
        let out = display_safe("Bash(\x1b[2Kevil*)");
        assert!(!out.contains('\x1b'));
        assert!(out.contains("\\u{001b}"));
    }

    #[test]
    fn display_safe_escapes_carriage_return() {
        let out = display_safe("native_permissions\rhidden");
        assert!(!out.contains('\r'));
        assert!(out.contains("\\u{000d}"));
    }

    #[test]
    fn display_safe_escapes_newline_and_tab_too() {
        // No exception for \n/\t: a config key/tool name/pattern has no
        // legitimate reason to contain either.
        let out = display_safe("a\nb\tc");
        assert!(!out.contains('\n') && !out.contains('\t'));
    }

    proptest::proptest! {
        #[test]
        fn display_safe_never_panics(s in ".{0,100}") {
            let _ = display_safe(&s);
        }

        #[test]
        fn display_safe_output_never_contains_a_raw_control_char(s in ".{0,100}") {
            let out = display_safe(&s);
            proptest::prop_assert!(!out.chars().any(char::is_control));
        }
    }
}
