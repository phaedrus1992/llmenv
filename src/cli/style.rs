//! CLI styling and color support.
//! Centralized color palette and TTY-aware color emission.

use anstyle::AnsiColor;

pub use llmenv_util::{ColorMode, doctor_warning, should_use_color};

/// Format an active state marker (e.g., "*") with optional green color.
pub fn active_marker(use_color: bool) -> String {
    llmenv_util::paint("*", AnsiColor::Green, use_color)
}

/// Format an inactive annotation (e.g., "(inactive)") with optional yellow color.
pub fn inactive_annotation(use_color: bool) -> String {
    llmenv_util::paint("(inactive)", AnsiColor::Yellow, use_color)
}

/// Format an orphan annotation (e.g., "(orphan)") with optional red color.
pub fn orphan_annotation(use_color: bool) -> String {
    llmenv_util::paint("(orphan)", AnsiColor::Red, use_color)
}

/// Format a doctor "pass" symbol (✓) with optional green color.
pub fn doctor_pass(use_color: bool) -> String {
    llmenv_util::paint("✓", AnsiColor::Green, use_color)
}

/// Format a doctor "fail" symbol (✗) with optional red color.
pub fn doctor_fail(use_color: bool) -> String {
    llmenv_util::paint("✗", AnsiColor::Red, use_color)
}

/// Format a doctor "info" symbol (ℹ), falling back to "i" when color is disabled.
pub fn doctor_info(use_color: bool) -> String {
    if use_color {
        "ℹ".to_string()
    } else {
        "i".to_string()
    }
}

/// Glyph + ANSI color for a task lifecycle state, for human `task ls` output
/// (#926). Mirrors the `doctor_*` markers: the Unicode glyph is kept even
/// without color; only the ANSI wrapping is gated on `use_color`.
#[must_use]
pub fn task_state_glyph(state: crate::task::TaskState, use_color: bool) -> String {
    use crate::task::TaskState;
    let (glyph, color) = match state {
        TaskState::Open => ("○", AnsiColor::White),
        TaskState::Wip => ("◐", AnsiColor::Cyan),
        TaskState::Waiting => ("⏸", AnsiColor::Yellow),
        TaskState::Done => ("✓", AnsiColor::Green),
    };
    llmenv_util::paint(glyph, color, use_color)
}

/// Lowercase label for a task lifecycle state (`open`/`wip`/`waiting`/`done`).
#[must_use]
pub fn task_state_label(state: crate::task::TaskState) -> &'static str {
    state.as_str()
}

/// Strip control characters (ANSI/CSI/OSC escapes, NUL, embedded newlines,
/// etc.) from agent-authored text before it's rendered to a TTY. Task titles
/// and session names come from `task add`/`session start` with attacker- or
/// agent-influenced content, so a title carrying raw escapes could spoof
/// `task ls` output or retitle the terminal window when a human runs it. Slugs
/// (kebab-only) and the `--format json` path don't need this.
#[must_use]
pub fn sanitize_for_terminal(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Red `(blocked on: …)` annotation listing the slugs a task is blocked on.
#[must_use]
pub fn blocked_annotation(refs: &[String], use_color: bool) -> String {
    let body = format!("(blocked on: {})", refs.join(", "));
    llmenv_util::paint(&body, AnsiColor::Red, use_color)
}

/// Truncate `s` to at most `max_len` **characters** (not bytes), appending
/// `…` (U+2026, itself counted within `max_len`) when truncation occurs.
/// UTF-8-boundary-safe: always truncates on a `char` boundary since it
/// iterates `chars()` rather than slicing bytes.
#[must_use]
pub fn truncate_ellipsis(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    if max_len == 0 {
        return String::new();
    }
    let keep = max_len.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Parse a space-separated style token string (`"bold cyan"`, `"#ff00aa"`,
/// `"color-208"`) into ANSI escape codes wrapping `s`. Unknown tokens are
/// ignored (never an error — a typo'd style must not crash the render).
/// `use_color: false` (or an empty `style`) passes `s` through unchanged.
#[must_use]
pub fn apply_style(s: &str, style: &str, use_color: bool) -> String {
    if !use_color || style.trim().is_empty() {
        return s.to_string();
    }
    let mut codes: Vec<String> = Vec::new();
    for token in style.split_whitespace() {
        if let Some(code) = style_token_code(token) {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        return s.to_string();
    }
    format!("\x1b[{}m{s}\x1b[0m", codes.join(";"))
}

fn style_token_code(token: &str) -> Option<String> {
    let named = match token {
        "bold" => Some("1"),
        "dim" => Some("2"),
        "italic" => Some("3"),
        "underline" => Some("4"),
        "blink" => Some("5"),
        "reverse" => Some("7"),
        "hidden" => Some("8"),
        "strikethrough" => Some("9"),
        "black" => Some("30"),
        "red" => Some("31"),
        "green" => Some("32"),
        "yellow" => Some("33"),
        "blue" => Some("34"),
        "magenta" => Some("35"),
        "cyan" => Some("36"),
        "white" => Some("37"),
        _ => None,
    };
    if let Some(code) = named {
        return Some(code.to_string());
    }
    if let Some(hex) = token.strip_prefix('#') {
        if hex.len() == 6 && hex.is_ascii() {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(format!("38;2;{r};{g};{b}"));
        }
        return None;
    }
    if let Some(n) = token.strip_prefix("color-") {
        let n: u8 = n.parse().ok()?;
        return Some(format!("38;5;{n}"));
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_marker_functions_plain_when_no_color() {
        // Without color, output contains the bare glyph and no escape codes.
        assert_eq!(active_marker(false), "*");
        assert_eq!(inactive_annotation(false), "(inactive)");
        assert_eq!(orphan_annotation(false), "(orphan)");
        assert_eq!(doctor_pass(false), "✓");
        assert_eq!(doctor_fail(false), "✗");
    }

    #[test]
    fn test_marker_functions_colored_contain_escape_codes() {
        // With color, output wraps the glyph in ANSI escape sequences.
        for s in [
            active_marker(true),
            inactive_annotation(true),
            orphan_annotation(true),
            doctor_pass(true),
            doctor_fail(true),
        ] {
            assert!(s.contains('\u{1b}'), "expected ANSI escape in {s:?}");
        }
    }

    #[test]
    fn test_marker_functions_preserve_glyph_under_color() {
        // Colored output still contains the underlying glyph text.
        assert!(active_marker(true).contains('*'));
        assert!(inactive_annotation(true).contains("(inactive)"));
        assert!(orphan_annotation(true).contains("(orphan)"));
        assert!(doctor_pass(true).contains('✓'));
        assert!(doctor_fail(true).contains('✗'));
    }

    #[test]
    fn truncate_ellipsis_leaves_short_strings_alone() {
        assert_eq!(truncate_ellipsis("hi", 10), "hi");
    }

    #[test]
    fn truncate_ellipsis_truncates_and_appends_ellipsis() {
        assert_eq!(truncate_ellipsis("hello world", 5), "hell…");
    }

    #[test]
    fn truncate_ellipsis_zero_max_len_yields_empty() {
        assert_eq!(truncate_ellipsis("hello", 0), "");
    }

    #[test]
    fn truncate_ellipsis_is_utf8_safe_on_multibyte_boundary() {
        // "║" is a 3-byte UTF-8 char; truncating mid-character must not panic
        // or produce invalid UTF-8.
        let s = "║║║║║";
        for max in 0..=6 {
            let out = truncate_ellipsis(s, max);
            assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        }
    }

    #[test]
    fn apply_style_wraps_bold_cyan() {
        let out = apply_style("hi", "bold cyan", true);
        assert!(out.starts_with("\x1b["));
        assert!(out.ends_with("\x1b[0m"));
        assert!(out.contains("hi"));
    }

    #[test]
    fn apply_style_no_color_passes_through() {
        assert_eq!(apply_style("hi", "bold cyan", false), "hi");
    }

    #[test]
    fn apply_style_empty_style_passes_through() {
        assert_eq!(apply_style("hi", "", true), "hi");
    }

    #[test]
    fn apply_style_hex_token_with_multibyte_char_does_not_panic() {
        // "#aaaéa" is 6 bytes but 5 chars — a byte-length check without an
        // ASCII guard would slice mid-character here and panic.
        let out = apply_style("hi", "#aaaéa", true);
        assert_eq!(out, "hi");
    }

    #[test]
    fn apply_style_valid_hex_token_renders_true_color() {
        let out = apply_style("hi", "#ff00aa", true);
        assert!(out.contains("38;2;255;0;170"));
    }

    #[test]
    fn apply_style_color_n_token_renders_256_color_code() {
        let out = apply_style("hi", "color-208", true);
        assert!(
            out.contains("38;5;208"),
            "expected 256-color code in {out:?}"
        );
    }

    #[test]
    fn truncate_ellipsis_exact_boundary_leaves_string_unchanged() {
        // count() == max_len exactly: must not truncate (off-by-one would
        // drop the last char and append an unneeded ellipsis).
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn truncate_ellipsis_never_panics_and_stays_utf8(
            s in ".*",
            max in 0usize..50,
        ) {
            let out = truncate_ellipsis(&s, max);
            prop_assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        }

        #[test]
        fn apply_style_never_panics_and_stays_utf8(
            s in ".*",
            style in ".*",
            use_color in any::<bool>(),
        ) {
            let out = apply_style(&s, &style, use_color);
            prop_assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        }

        #[test]
        fn apply_style_valid_hex_always_renders_matching_rgb_code(
            r in any::<u8>(),
            g in any::<u8>(),
            b in any::<u8>(),
        ) {
            let style = format!("#{r:02x}{g:02x}{b:02x}");
            let out = apply_style("x", &style, true);
            let expected = format!("38;2;{r};{g};{b}");
            prop_assert!(out.contains(&expected));
        }

        #[test]
        fn style_token_code_never_panics_on_arbitrary_token(token in ".*") {
            let _ = style_token_code(&token);
        }
    }
}
