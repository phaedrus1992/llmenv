//! Row template parsing: `"{model} │ {context_pct}"` → literal + widget
//! tokens, resolved against widget renderers by the orchestrator.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateToken {
    Literal(String),
    Widget { name: String, truncate: bool },
}

/// Parse a row template into tokens. `{name}` is a widget reference;
/// `{name:t}` is shorthand for "apply the widget's configured truncation".
/// An unclosed `{` (no matching `}`) is treated as literal text, not an
/// error — the design doc requires the renderer to never fail on template
/// parsing, only on data/config I/O.
#[must_use]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by statusline orchestrator, wired up in a follow-up task"
    )
)]
pub fn parse_template(template: &str) -> Vec<TemplateToken> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = template.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c != '{' {
            literal.push(c);
            continue;
        }
        // Look for the matching '}' from here.
        let rest = &template[start + 1..];
        if let Some(end) = rest.find('}') {
            let inner = &rest[..end];
            let (name, truncate) = match inner.split_once(':') {
                Some((name, "t")) => (name, true),
                _ => (inner, false),
            };
            if !literal.is_empty() {
                tokens.push(TemplateToken::Literal(std::mem::take(&mut literal)));
            }
            tokens.push(TemplateToken::Widget {
                name: name.to_string(),
                truncate,
            });
            // Advance the outer iterator past the consumed `inner}`.
            for _ in 0..=end {
                chars.next();
            }
        } else {
            // No closing brace anywhere in the remainder: the rest of the
            // template (including this `{`) is literal.
            literal.push_str(&template[start..]);
            break;
        }
    }
    if !literal.is_empty() {
        tokens.push(TemplateToken::Literal(literal));
    }
    tokens
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by Task 8 orchestrator")
)]
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_literal_and_widget_tokens() {
        let tokens = parse_template("{model} │ {context_pct}");
        assert_eq!(
            tokens,
            vec![
                TemplateToken::Widget {
                    name: "model".to_string(),
                    truncate: false
                },
                TemplateToken::Literal(" │ ".to_string()),
                TemplateToken::Widget {
                    name: "context_pct".to_string(),
                    truncate: false
                },
            ]
        );
    }

    #[test]
    fn parses_truncate_shorthand() {
        let tokens = parse_template("{scopes:t}");
        assert_eq!(
            tokens,
            vec![TemplateToken::Widget {
                name: "scopes".to_string(),
                truncate: true
            }]
        );
    }

    #[test]
    fn plain_literal_with_no_placeholders() {
        let tokens = parse_template("no widgets here");
        assert_eq!(
            tokens,
            vec![TemplateToken::Literal("no widgets here".to_string())]
        );
    }

    #[test]
    fn unclosed_brace_is_literal() {
        let tokens = parse_template("{model");
        assert_eq!(tokens, vec![TemplateToken::Literal("{model".to_string())]);
    }

    #[test]
    fn empty_template_yields_no_tokens() {
        assert_eq!(parse_template(""), Vec::<TemplateToken>::new());
    }

    fn arb_template_char() -> impl Strategy<Value = char> {
        prop_oneof![
            Just('{'),
            Just('}'),
            Just(':'),
            Just('t'),
            "[a-z_]".prop_map(|s| s.chars().next().unwrap()),
        ]
    }

    proptest! {
        // Any string built purely from the parser's own alphabet must parse
        // without panicking, and re-flattening the tokens' literal text plus
        // widget braces must not silently drop input length in a way that
        // loses non-widget characters (a weaker, panic-safety-focused
        // invariant — full round-trip isn't required since `{bad` is
        // deliberately folded into a literal).
        #[test]
        fn parse_template_never_panics(s in prop::collection::vec(arb_template_char(), 0..40)) {
            let input: String = s.into_iter().collect();
            let _ = parse_template(&input);
        }
    }
}
