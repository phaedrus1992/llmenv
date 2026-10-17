//! The paths that processes hold open, as reported by `lsof`.
//!
//! The `state/tmp` sweep keeps an entry that a live process still uses. This module builds the
//! list of those paths for the sweep to check against.

use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use anyhow::Context;

/// Paths that a process holds open or uses as its working directory.
///
/// # Errors
/// Fails when `lsof` cannot run or does not exit 0. A sweep without a complete list could
/// remove an entry that a live process uses, so the sweep stops instead.
#[cfg(unix)]
pub fn open_paths() -> anyhow::Result<Vec<PathBuf>> {
    lsof_paths("lsof", DEFAULT_LSOF_TIMEOUT)
}

/// The longest wait for `lsof`. A full list takes a few seconds on a busy host, and a hung
/// network or SMB mount blocks lsof with no end (#2832). The value is fixed, because the sweep
/// runs only from `llmenv doctor --gc`, and a minute is a clear sign of a hung mount.
#[cfg(unix)]
const DEFAULT_LSOF_TIMEOUT: Duration = Duration::from_secs(60);

/// The sweep needs lsof to find the entries in use, and lsof is a unix tool.
///
/// # Errors
/// Always fails, because the in-use list cannot be built on this platform.
#[cfg(not(unix))]
pub fn open_paths() -> anyhow::Result<Vec<PathBuf>> {
    anyhow::bail!("the state/tmp sweep needs lsof, which runs on unix only; the sweep stopped")
}

/// Run `program -F n` and read the paths it names. `program` is a parameter so a test can
/// name a binary that fails or hangs.
#[cfg(unix)]
fn lsof_paths(program: &str, timeout: Duration) -> anyhow::Result<Vec<PathBuf>> {
    let mut cmd = Command::new(program);
    cmd.args(["-F", "n"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match crate::process::run_with_timeout(cmd, timeout) {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => anyhow::bail!(
            "`{program}` gave no answer within {} s, so it was stopped; the state/tmp sweep \
             stopped. A hung network or SMB mount can block lsof. Fix: run `mount` to find the \
             hung network mount, unmount or reconnect it, then run `llmenv doctor --gc` again",
            timeout.as_secs()
        ),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "running `{program}` to find entries in use; the state/tmp sweep stopped. \
                     Fix: install lsof, then run `llmenv doctor --gc` again"
                )
            });
        }
    };
    // lsof exits 1 on any error, and its list is then incomplete. Only exit 0 counts.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if output.status.code() != Some(0) {
        anyhow::bail!(
            "`{program}` exited with {}: {stderr}; the state/tmp sweep stopped. \
             Fix: resolve the lsof error above, then run `llmenv doctor --gc` again",
            output.status
        );
    }
    // lsof reports partial results on stderr, for example for a mount it cannot stat. Those
    // results are routine on some hosts, so they are logged, not fatal (#2814). The in-use list
    // may then miss an entry that is open on that mount.
    if !stderr.is_empty() {
        tracing::warn!(
            stderr,
            "lsof reported a problem; the in-use list may be incomplete"
        );
    }
    Ok(parse_lsof_names(&output.stdout))
}

/// The name fields of `lsof -F n` output that are absolute paths. Socket and pipe names do not
/// start with `/`, so they drop out. Names are kept as bytes, so a non-UTF-8 name still matches.
#[cfg(unix)]
fn parse_lsof_names(output: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"n"))
        .filter(|name| name.starts_with(b"/"))
        .map(|name| PathBuf::from(std::ffi::OsStr::from_bytes(name)))
        .collect()
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use super::{lsof_paths, parse_lsof_names};

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn parse_keeps_only_the_absolute_path_names() {
        let out = b"p123\nfcwd\nn/work/a\nnTCP *:8080\np9\nn/work/b (deleted)\n";
        let paths = parse_lsof_names(out);
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("/work/a"),
                std::path::PathBuf::from("/work/b (deleted)"),
            ]
        );
    }

    #[test]
    fn parse_keeps_a_non_utf8_name_byte_for_byte() {
        use std::os::unix::ffi::OsStrExt;
        let paths = parse_lsof_names(b"n/tmp/\xff\n");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/tmp/\xff");
    }

    #[test]
    fn parse_yields_one_path_per_absolute_name_line() {
        use proptest::prelude::*;
        proptest!(|(lines in proptest::collection::vec("[pfn/ a-z]{0,10}", 0..12))| {
            let text = lines.join("\n");
            let expected: Vec<String> = lines
                .iter()
                .filter(|line| line.starts_with("n/"))
                .map(|line| line[1..].to_owned())
                .collect();
            let got: Vec<String> = parse_lsof_names(text.as_bytes())
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            prop_assert_eq!(got, expected);
        });
    }

    #[test]
    fn a_missing_lsof_is_an_error_that_names_the_fix() {
        let err = lsof_paths("llmenv-no-such-lsof-binary", WAIT).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("Fix:"), "no fix named: {text}");
        assert!(
            text.contains("state/tmp sweep stopped"),
            "no effect named: {text}"
        );
    }

    #[test]
    fn an_lsof_that_writes_to_stderr_keeps_the_names_it_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-lsof");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'cannot stat /mnt/x' >&2\necho 'n/work/open'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let paths = lsof_paths(fake.to_str().unwrap(), WAIT).unwrap();

        assert_eq!(paths, vec![std::path::PathBuf::from("/work/open")]);
    }

    #[test]
    fn an_lsof_exit_other_than_zero_is_an_error_even_with_empty_output() {
        // `false` exits 1 and prints nothing. An exit 1 is a failure, not an empty list.
        let err = lsof_paths("false", WAIT).unwrap_err();
        assert!(format!("{err:#}").contains("exited with"), "{err:#}");
    }

    #[test]
    fn a_hung_lsof_is_stopped_and_the_fix_names_the_mount() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("hung-lsof");
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let start = std::time::Instant::now();
        let err = lsof_paths(fake.to_str().unwrap(), Duration::from_millis(200)).unwrap_err();

        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the deadline did not stop lsof"
        );
        let text = format!("{err:#}");
        assert!(text.contains("no answer"), "{text}");
        assert!(text.contains("Fix:") && text.contains("mount"), "{text}");
    }
}
