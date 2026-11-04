//! Tests for the export output: value quoting and the all-or-nothing export.

#![expect(
    clippy::expect_used,
    reason = "test code: a failed fixture stops the test"
)]

use super::export_lines;
use proptest::prelude::*;
use std::collections::BTreeMap;

#[cfg(unix)]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// A shell that reads the quoted value gets the same bytes back.
    #[test]
    fn a_quoted_value_reads_back_unchanged_in_sh(value in "[^\\x00]{0,64}") {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", crate::util::single_quote(&value)))
            .output()
            .expect("run sh");
        prop_assert!(output.status.success());
        prop_assert_eq!(output.stdout, value.as_bytes().to_vec());
    }
}

#[test]
fn one_bad_value_fails_the_whole_export() {
    let mut vars = BTreeMap::new();
    vars.insert("LLMENV_GOOD".to_string(), "fine".to_string());
    vars.insert("LLMENV_BAD".to_string(), "a\u{0}b".to_string());
    assert!(export_lines(&vars, None).is_err());
}

#[test]
fn an_explained_line_names_its_source_before_the_export() {
    let mut vars = BTreeMap::new();
    vars.insert("LLMENV_ACTIVE_TAGS".to_string(), "rust".to_string());
    let lines = export_lines(&vars, Some("rust-dev")).expect("a valid export");
    assert_eq!(lines[0], "# source: llmenv introspection");
    assert!(
        lines[1].starts_with("export LLMENV_ACTIVE_TAGS="),
        "{lines:?}"
    );
}
