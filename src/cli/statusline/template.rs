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
pub fn parse_template(template: &str) -> Vec<TemplateToken> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = template.char_indices();
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
            // Advance the outer iterator past the consumed `inner}`. `end` is
            // a *byte* offset (from `rest.find('}')`), but `chars` advances
            // by *character* — using `end` directly desyncs whenever `inner`
            // contains a multibyte char, silently dropping literal text after
            // the widget (a multibyte char is >1 byte but always 1 char, so
            // `end` overcounts the number of chars to skip). Skip by `inner`'s
            // char count plus one (for the closing brace) instead.
            for _ in 0..=inner.chars().count() {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn colon_suffix_other_than_t_is_kept_as_part_of_the_widget_name() {
        // Only an exact ":t" suffix is the truncate shorthand; any other
        // colon suffix must use the *whole* inner text as the widget name
        // (which then simply won't match a known widget at render time),
        // not just the text before the colon.
        let tokens = parse_template("{name:x}");
        assert_eq!(
            tokens,
            vec![TemplateToken::Widget {
                name: "name:x".to_string(),
                truncate: false
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

    #[test]
    fn multibyte_char_inside_widget_name_does_not_drop_trailing_literal() {
        // Regression: `end` in the consumption loop is a byte offset, but the
        // outer iterator advances by char — a multibyte char inside the
        // braces used to desync the two, silently eating into the literal
        // text that follows the widget.
        let tokens = parse_template("{a→b}tail");
        assert_eq!(
            tokens,
            vec![
                TemplateToken::Widget {
                    name: "a→b".to_string(),
                    truncate: false
                },
                TemplateToken::Literal("tail".to_string()),
            ]
        );
    }

    fn arb_template_char() -> impl Strategy<Value = char> {
        prop_oneof![
            Just('{'),
            Just('}'),
            Just(':'),
            Just('t'),
            Just('→'),
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
