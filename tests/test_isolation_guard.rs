#![expect(clippy::panic, reason = "test scaffolding")]
//! Guard that every integration-test binary spawns `llmenv` through
//! [`support::isolated_llmenv_cmd`] rather than `Command::cargo_bin` directly
//! (#1266).
//!
//! `Command::cargo_bin("llmenv")` inherits the developer's real `HOME`, so
//! `cache.cache_dir` tilde-expands to the real `~/.cache/llmenv`. The
//! materialized path is `<adapter>/<version>/<selection-shape>` and the shape
//! digests the active tags and directly-enabled bundles — *not* the config
//! directory. Two binaries declaring the same tags and bundles therefore land
//! in the same folder even with distinct `LLMENV_CONFIG_DIR` tempdirs, and
//! clobber each other when run concurrently (#1254). It also leaves the test
//! suite mutating the real llmenv cache of whoever ran it.
//!
//! Overriding `LLMENV_CONFIG_DIR` alone does not fix this. The helper is the
//! only place that overrides every knob at once, so this guard exists to stop
//! a new test file from silently reintroducing the sharing.

use std::fs;
use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Files allowed to name `Command::cargo_bin` directly.
///
/// `support/mod.rs` is the helper itself; this guard reads its own source, so
/// it necessarily contains the string it searches for.
const ALLOWED: &[&str] = &["support/mod.rs", "test_isolation_guard.rs"];

/// Every `.rs` file under `tests/`, relative to the tests dir, recursively.
fn integration_test_sources(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("read entry in {}: {e}", dir.display()));
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            // `fixtures/` holds config scaffolds, not test code.
            if name != "fixtures" {
                integration_test_sources(&path, &rel, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            let body = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push((rel, body));
        }
    }
}

#[test]
fn integration_tests_spawn_llmenv_through_the_isolation_helper() {
    let tests_dir = Path::new(MANIFEST_DIR).join("tests");
    let mut sources = Vec::new();
    integration_test_sources(&tests_dir, "", &mut sources);

    assert!(
        sources.len() > 10,
        "guard found only {} test sources — the walk is broken, not the suite clean",
        sources.len()
    );

    let offenders: Vec<&str> = sources
        .iter()
        .filter(|(rel, body)| {
            !ALLOWED.contains(&rel.as_str()) && body.contains("Command::cargo_bin")
        })
        .map(|(rel, _)| rel.as_str())
        .collect();

    assert!(
        offenders.is_empty(),
        "these test files spawn the llmenv binary directly instead of via \
         `support::isolated_llmenv_cmd`, so they inherit the real HOME and \
         materialize into the real ~/.cache/llmenv (#1266):\n  {}\n\n\
         Fix: `mod support;` then `support::isolated_llmenv_cmd(<tempdir>)`.",
        offenders.join("\n  ")
    );
}
