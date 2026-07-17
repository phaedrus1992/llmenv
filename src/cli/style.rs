//! CLI styling and color support.
//! Centralized color palette and TTY-aware color emission.

use anstyle::{AnsiColor, Color, Style};

/// Wrap text in an ANSI style when `use_color` is set, else return it plain.
fn paint(text: &str, color: AnsiColor, use_color: bool) -> String {
    if use_color {
        let style = Style::new().fg_color(Some(Color::Ansi(color)));
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

/// Color mode: auto-detect, always on, or always off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Auto-detect based on stdout TTY and NO_COLOR env var
    Auto,
    /// Force colors on
    Always,
    /// Force colors off
    Never,
}

/// Determine whether to emit colors based on flags, env vars, and TTY state.
///
/// # Arguments
/// * `mode` - Explicit color mode (None = Auto)
/// * `is_tty` - Whether stdout is a terminal
///
/// # Returns
/// true if colors should be emitted, false otherwise.
pub fn should_use_color(mode: Option<ColorMode>, is_tty: bool) -> bool {
    should_use_color_with_env(mode, is_tty, &|name| std::env::var(name).ok())
}

/// Determine whether to emit colors, with injectable env var provider for testing.
///
/// # Arguments
/// * `mode` - Explicit color mode (None = Auto)
/// * `is_tty` - Whether stdout is a terminal
/// * `get_env` - Function to retrieve environment variables (for testing)
///
/// # Returns
/// true if colors should be emitted, false otherwise.
fn should_use_color_with_env<F>(mode: Option<ColorMode>, is_tty: bool, get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let effective_mode = mode.unwrap_or(ColorMode::Auto);

    match effective_mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            // Check NO_COLOR env var first (unconditional disable, any value disables)
            if get_env("NO_COLOR").is_some() {
                return false;
            }
            // Then check CLICOLOR_FORCE (unconditional enable, must be non-empty per spec)
            if get_env("CLICOLOR_FORCE")
                .filter(|v| !v.is_empty())
                .is_some()
            {
                return true;
            }
            // Default to TTY detection
            is_tty
        }
    }
}

/// Format an active state marker (e.g., "*") with optional green color.
pub fn active_marker(use_color: bool) -> String {
    paint("*", AnsiColor::Green, use_color)
}

/// Format an inactive annotation (e.g., "(inactive)") with optional yellow color.
pub fn inactive_annotation(use_color: bool) -> String {
    paint("(inactive)", AnsiColor::Yellow, use_color)
}

/// Format an orphan annotation (e.g., "(orphan)") with optional red color.
pub fn orphan_annotation(use_color: bool) -> String {
    paint("(orphan)", AnsiColor::Red, use_color)
}

/// Format a doctor "pass" symbol (✓) with optional green color.
pub fn doctor_pass(use_color: bool) -> String {
    paint("✓", AnsiColor::Green, use_color)
}

/// Format a doctor "warning" symbol (⚠) with optional yellow color.
pub fn doctor_warning(use_color: bool) -> String {
    paint("⚠", AnsiColor::Yellow, use_color)
}

/// Format a doctor "fail" symbol (✗) with optional red color.
pub fn doctor_fail(use_color: bool) -> String {
    paint("✗", AnsiColor::Red, use_color)
}

/// Format a doctor "info" symbol (ℹ), falling back to "i" when color is disabled.
pub fn doctor_info(use_color: bool) -> String {
    if use_color {
        "ℹ".to_string()
    } else {
        "i".to_string()
    }
}

/// Truncate `s` to at most `max_len` **characters** (not bytes), appending
/// `…` (U+2026, itself counted within `max_len`) when truncation occurs.
/// UTF-8-boundary-safe: always truncates on a `char` boundary since it
/// iterates `chars()` rather than slicing bytes.
#[must_use]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by statusline widget rendering, wired up in a follow-up task"
    )
)]
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed by statusline widget rendering, wired up in a follow-up task"
    )
)]
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
        if hex.len() == 6 {
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
    fn test_should_use_color_always_mode() {
        assert!(should_use_color(Some(ColorMode::Always), false));
        assert!(should_use_color(Some(ColorMode::Always), true));
    }

    #[test]
    fn test_should_use_color_never_mode() {
        assert!(!should_use_color(Some(ColorMode::Never), false));
        assert!(!should_use_color(Some(ColorMode::Never), true));
    }

    #[test]
    fn test_should_use_color_auto_respects_tty() {
        assert!(!should_use_color(Some(ColorMode::Auto), false));
        // TTY test now possible with controlled env via should_use_color_with_env
    }

    #[test]
    fn test_should_use_color_auto_with_tty_isolated() {
        let no_env = |_name: &str| -> Option<String> { None };
        // With controlled env (no NO_COLOR, no CLICOLOR_FORCE), auto mode respects is_tty
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            false,
            &no_env
        ));
        assert!(should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &no_env
        ));
    }

    #[test]
    fn test_should_use_color_no_color_overrides() {
        let no_color_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some("1".to_string()),
                _ => None,
            }
        };
        // NO_COLOR unconditionally disables colors even with is_tty=true
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &no_color_env
        ));
    }

    #[test]
    fn test_should_use_color_no_color_empty_string() {
        let no_color_empty_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some(String::new()),
                _ => None,
            }
        };
        // NO_COLOR with empty string should still disable colors (presence matters, not value)
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &no_color_empty_env
        ));
    }

    #[test]
    fn test_should_use_color_clicolor_force_overrides() {
        let force_env = |name: &str| -> Option<String> {
            match name {
                "CLICOLOR_FORCE" => Some("1".to_string()),
                _ => None,
            }
        };
        // CLICOLOR_FORCE unconditionally enables colors even with is_tty=false
        assert!(should_use_color_with_env(
            Some(ColorMode::Auto),
            false,
            &force_env
        ));
    }

    #[test]
    fn test_marker_functions_plain_when_no_color() {
        // Without color, output contains the bare glyph and no escape codes.
        assert_eq!(active_marker(false), "*");
        assert_eq!(inactive_annotation(false), "(inactive)");
        assert_eq!(orphan_annotation(false), "(orphan)");
        assert_eq!(doctor_pass(false), "✓");
        assert_eq!(doctor_warning(false), "⚠");
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
            doctor_warning(true),
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
        assert!(doctor_warning(true).contains('⚠'));
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
    }
}
