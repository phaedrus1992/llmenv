//! Status glyphs for `llmenv doctor` output.

use crate::cli::style::paint;
use anstyle::AnsiColor;

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
