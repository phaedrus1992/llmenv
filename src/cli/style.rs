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
    let effective_mode = mode.unwrap_or(ColorMode::Auto);

    match effective_mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            // Check NO_COLOR env var first (unconditional disable)
            if std::env::var("NO_COLOR").is_ok() {
                return false;
            }
            // Then check CLICOLOR_FORCE (unconditional enable, must be non-empty per spec)
            if std::env::var("CLICOLOR_FORCE")
                .ok()
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

#[cfg(test)]
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
        // TTY test would require mocking; skipped here
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
}
