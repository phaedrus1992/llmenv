//! The rules in a model answer: one `- ` bullet line for each rule.

/// Maximum character length for a single rule bullet.
const MAX_RULE_LENGTH: usize = 500;

/// Parse bullet-point rules from the model's text output.
///
/// Returns lines that start with `- ` (dash-space), trimming whitespace.
/// Empty output → no rules → no store calls (success, not an error).
pub(super) fn parse_bullets(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("- ") && l.len() > 2)
        .map(|l| {
            let rule = l[2..].trim();
            if rule.chars().count() > MAX_RULE_LENGTH {
                // Ponytail: truncate overlong rules with a marker. Truncates
                // by character count, not byte length — MAX_RULE_LENGTH is a
                // character bound, and byte-slicing panics when a multi-byte
                // char straddles the cut point (#1166).
                let truncated: String = rule.chars().take(MAX_RULE_LENGTH).collect();
                format!("{truncated}… (truncated)")
            } else {
                rule.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parse_bullets_empty_text() {
        assert!(parse_bullets("").is_empty());
    }

    #[test]
    fn parse_bullets_only_prose() {
        let text = "This is just a paragraph of text.\nNo bullet points here.";
        assert!(parse_bullets(text).is_empty());
    }

    #[test]
    fn parse_bullets_single() {
        let text = "- Use Rust for all new projects";
        let bullets = parse_bullets(text);
        assert_eq!(bullets, vec!["Use Rust for all new projects"]);
    }

    #[test]
    fn parse_bullets_multiple() {
        let text = "- First rule\n- Second rule\nSome prose in between\n- Third rule";
        let bullets = parse_bullets(text);
        assert_eq!(bullets, vec!["First rule", "Second rule", "Third rule"]);
    }

    #[test]
    fn parse_bullets_respects_max_rule_length() {
        let long = "x".repeat(MAX_RULE_LENGTH + 10);
        let text = format!("- {long}");
        let bullets = parse_bullets(&text);
        assert_eq!(bullets.len(), 1);
        assert!(bullets[0].ends_with("… (truncated)"));
        assert!(bullets[0].len() <= MAX_RULE_LENGTH + "… (truncated)".len());
    }

    // #1166: MAX_RULE_LENGTH is documented as a *character* bound, but the
    // truncation sliced by *byte* index — a multi-byte char straddling byte
    // index MAX_RULE_LENGTH panics ("byte index is not a char boundary").
    // "x" (1 byte) then "é" (2 bytes) repeated puts the boundary mid-char.
    #[test]
    fn parse_bullets_truncates_multibyte_rule_without_panicking() {
        let long = "x".to_string() + &"é".repeat(MAX_RULE_LENGTH);
        let text = format!("- {long}");
        let bullets = parse_bullets(&text);
        assert_eq!(bullets.len(), 1);
        assert!(bullets[0].ends_with("… (truncated)"));
        assert_eq!(
            bullets[0].chars().count(),
            MAX_RULE_LENGTH + "… (truncated)".chars().count(),
            "truncation must count characters, not bytes"
        );
    }

    #[test]
    fn parse_bullets_strips_leading_dash_space() {
        let text = "-  hello world";
        let bullets = parse_bullets(text);
        assert_eq!(bullets, vec!["hello world"]);
    }

    // #1166: property-test coverage for parse_bullets.

    proptest! {
        /// The model's raw text response is arbitrary as far as `parse_bullets`
        /// is concerned — no input should panic. Multi-line, multi-byte
        /// content is exactly the case #1166 found panicking.
        #[test]
        fn parse_bullets_never_panics(
            lines in proptest::collection::vec(".{0,600}", 0..10),
        ) {
            let text = lines.join("\n");
            let _ = parse_bullets(&text);
        }

        /// Every returned bullet is bounded by MAX_RULE_LENGTH characters
        /// (plus the truncation marker) regardless of the input's byte/char
        /// composition — the invariant the byte/char confusion violated.
        #[test]
        fn parse_bullets_never_exceeds_max_length(
            rule in ".{0,600}",
        ) {
            let text = format!("- {rule}");
            let bullets = parse_bullets(&text);
            let marker_len = "… (truncated)".chars().count();
            for bullet in &bullets {
                prop_assert!(
                    bullet.chars().count() <= MAX_RULE_LENGTH + marker_len,
                    "bullet {bullet:?} exceeds the length bound"
                );
            }
        }

        /// A rule within the bound, with no edge space, comes back unchanged.
        #[test]
        fn a_rule_within_the_bound_round_trips(rule in "[a-zA-Z0-9é]([a-zA-Z0-9é ,.]{0,498}[a-zA-Z0-9é])?") {
            prop_assert_eq!(parse_bullets(&format!("- {rule}")), vec![rule]);
        }

        /// A rule past the bound keeps its first MAX_RULE_LENGTH characters and the marker.
        #[test]
        fn a_rule_past_the_bound_keeps_its_head(rule in "[a-zA-Zé]{501,700}") {
            let head: String = rule.chars().take(MAX_RULE_LENGTH).collect();
            prop_assert_eq!(
                parse_bullets(&format!("- {rule}")),
                vec![format!("{head}… (truncated)")]
            );
        }
    }

    #[test]
    fn a_rule_of_exactly_the_bound_is_not_truncated() {
        let rule = "é".repeat(MAX_RULE_LENGTH);
        assert_eq!(parse_bullets(&format!("- {rule}")), vec![rule]);
    }
}
