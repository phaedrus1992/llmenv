//! The Claude Code version, for the hooks (#2898).
//!
//! Claude Code does not export its version to hook processes. llmenv reads it once per
//! materialize and passes it as `CLAUDE_CODE_VERSION`, so session metadata can record it.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

/// How long `claude --version` may take. The binary prints one line and exits.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The first Claude Code release that reads `onFailure` on a hook (its changelog entry for
/// 2.1.295). An older engine may reject a settings file with a key it does not know (#3101).
const ON_FAILURE_SINCE: (u64, u64, u64) = (2, 1, 295);

/// The version read once in this process, so the hook render and the env var share one
/// `claude --version` run.
static DETECTED: OnceLock<Option<String>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// A test sets the version here, so no test runs the real `claude` binary.
    pub(crate) static TEST_VERSION: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// The Claude Code version, read once per process. `None` when it cannot be read.
pub(crate) fn detected() -> Option<String> {
    #[cfg(test)]
    if let Some(version) = TEST_VERSION.with(|v| v.borrow().clone()) {
        return version;
    }
    DETECTED.get_or_init(read_version).clone()
}

/// The `CLAUDE_CODE_VERSION` variable, or no variable when the version cannot be read. The hooks
/// then record an empty version, as they did before this change.
pub(crate) fn env_vars() -> Vec<(String, String)> {
    detected()
        .map(|version| vec![("CLAUDE_CODE_VERSION".into(), version)])
        .unwrap_or_default()
}

/// Whether Claude Code `version` reads `onFailure` on a hook. An unknown or unreadable version
/// does not, so llmenv never writes a key that an older engine may reject (#3101).
pub(crate) fn supports_on_failure(version: Option<&str>) -> bool {
    version.and_then(numeric_triple) >= Some(ON_FAILURE_SINCE)
}

/// The first three numbers of a version such as `2.1.295` or `2.1.295-beta`.
fn numeric_triple(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Run `claude --version`. Each failure is logged, and gives `None`.
fn read_version() -> Option<String> {
    // Export runs in the folder of each shell prompt, so a bare `claude` could run a copy that a
    // cloned repo holds in a relative PATH folder (#2932).
    let claude = match crate::path_lookup::resolve_program("claude") {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("CLAUDE_CODE_VERSION not set: {e:#}");
            return None;
        }
    };
    let mut cmd = Command::new(claude);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match crate::process::run_traced("claude-code:version", cmd, VERSION_TIMEOUT) {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let version = parse_version(&text);
            if version.is_none() {
                tracing::error!("claude --version printed no version: {}", text.trim());
            }
            version
        }
        Ok(out) => {
            tracing::error!("claude --version exited with {}", out.status);
            None
        }
        Err(e) => {
            tracing::error!("claude --version failed: {e:#}");
            None
        }
    }
}

/// The version number at the start of `claude --version` output, such as `2.1.251` from
/// `2.1.251 (Claude Code)`. `None` when the first word is not a dotted number.
fn parse_version(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let numeric = first.starts_with(|c: char| c.is_ascii_digit())
        && first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    numeric.then(|| first.to_string())
}

#[cfg(test)]
mod tests {
    use super::{parse_version, supports_on_failure};
    use proptest::prelude::*;

    #[test]
    fn on_failure_starts_at_2_1_295() {
        assert!(supports_on_failure(Some("2.1.295")));
        assert!(supports_on_failure(Some("2.1.318")));
        assert!(supports_on_failure(Some("2.2.0")));
        assert!(supports_on_failure(Some("3.0.0")));
        assert!(supports_on_failure(Some("2.1.295-beta")));
        assert!(!supports_on_failure(Some("2.1.294")));
        assert!(!supports_on_failure(Some("2.0.999")));
        assert!(!supports_on_failure(Some("1.9.999")));
        assert!(!supports_on_failure(Some("2.1")));
        assert!(!supports_on_failure(Some("2.x.300")));
        assert!(!supports_on_failure(None));
    }

    proptest! {
        /// The support check is the numeric order of the first three parts, at any version.
        #[test]
        fn on_failure_support_follows_the_numeric_order(
            major in 0u64..5, minor in 0u64..5, patch in 0u64..1000,
        ) {
            let version = format!("{major}.{minor}.{patch}");
            prop_assert_eq!(
                supports_on_failure(Some(&version)),
                (major, minor, patch) >= (2, 1, 295)
            );
        }
    }

    #[test]
    fn the_version_is_the_first_word_of_the_output() {
        assert_eq!(
            parse_version("2.1.251 (Claude Code)\n").as_deref(),
            Some("2.1.251")
        );
    }

    #[test]
    fn output_without_a_dotted_number_has_no_version() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("error: unknown flag"), None);
        assert_eq!(parse_version("v2.1.251"), None);
        assert_eq!(
            parse_version("2.1.251-beta"),
            Some("2.1.251-beta".to_string())
        );
    }
}
