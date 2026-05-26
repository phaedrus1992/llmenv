//! CLI styling and color support.
//! Centralized color palette and TTY-aware color emission.

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

/// Format an active state marker (e.g., "*") in green.
pub fn active_marker() -> &'static str {
    "* "
}

/// Format an inactive annotation (e.g., "(inactive)") in yellow/dim.
pub fn inactive_annotation() -> &'static str {
    "(inactive)"
}

#[allow(dead_code)]
/// Format an orphan annotation (e.g., "(orphan)") in red.
pub fn orphan_annotation() -> &'static str {
    "(orphan)"
}

#[allow(dead_code)]
/// Format a doctor "pass" symbol (✓) in green.
pub fn doctor_pass() -> &'static str {
    "✓ "
}

#[allow(dead_code)]
/// Format a doctor "warning" symbol (⚠) in yellow.
pub fn doctor_warning() -> &'static str {
    "⚠ "
}

#[allow(dead_code)]
/// Format a doctor "fail" symbol (✗) in red.
pub fn doctor_fail() -> &'static str {
    "✗ "
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
    fn test_marker_functions_return_strings() {
        assert!(!active_marker().is_empty());
        assert!(!inactive_annotation().is_empty());
        assert!(!orphan_annotation().is_empty());
        assert!(!doctor_pass().is_empty());
        assert!(!doctor_warning().is_empty());
        assert!(!doctor_fail().is_empty());
    }
}
