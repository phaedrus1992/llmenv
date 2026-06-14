#![expect(clippy::expect_used, reason = "test scaffolding")]
//! Test paragraph reflow for release notes (#320).

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn reflow_prose_paragraphs() {
    // Input: prose split across source lines (like CHANGELOG hard-wrapping)
    let input = r#"### Added

- Feature one
  that spans multiple source lines
  in the changelog
- Another feature

### Fixed

- Bug fix with a long description that
  spans multiple source lines for readability
  in the source file
- Another bug fix"#;

    // Expected: prose lines joined within paragraphs, block elements preserved
    let expected = r#"### Added

- Feature one that spans multiple source lines in the changelog
- Another feature

### Fixed

- Bug fix with a long description that spans multiple source lines for readability in the source file
- Another bug fix"#;

    // Find and run the reflow script
    let script_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reflow-markdown.py");

    let mut child = Command::new("python3")
        .arg(&script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn reflow script");

    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin
            .write_all(input.as_bytes())
            .expect("failed to write to stdin");
    }

    let output = child.wait_with_output().expect("failed to wait on script");
    assert!(
        output.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let actual = String::from_utf8_lossy(&output.stdout);
    assert_eq!(actual.trim(), expected.trim(), "reflow output mismatch");
}

#[test]
fn reflow_preserves_code_blocks() {
    // Code blocks and blank lines should be preserved
    let input = r#"Some paragraph that
spans multiple lines.

```
code block here
should stay as-is
```

Another paragraph
on multiple lines"#;

    let expected = r#"Some paragraph that spans multiple lines.

```
code block here
should stay as-is
```

Another paragraph on multiple lines"#;

    let script_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reflow-markdown.py");
    let mut child = Command::new("python3")
        .arg(&script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn reflow script");

    {
        let stdin = child.stdin.as_mut().expect("failed to open stdin");
        stdin
            .write_all(input.as_bytes())
            .expect("failed to write to stdin");
    }

    let output = child.wait_with_output().expect("failed to wait on script");
    assert!(
        output.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let actual = String::from_utf8_lossy(&output.stdout);
    assert_eq!(actual.trim(), expected.trim(), "reflow output mismatch");
}
