//! Checks the session-log file path (#3138).
//!
//! The sink opens the file with `O_NOFOLLOW` and appends to it, so a symlink, a FIFO, or a
//! device at the path fails on every event. This check shows the problem once, with the fix.

use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::{Config, SessionLog};

/// The result of the check on one resolved path.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// A regular file, or no file yet in a folder the sink can write: the sink creates the file
    /// on the first event.
    Usable { exists: bool },
    /// Something other than a regular file. The text names its kind.
    NotRegular(&'static str),
    /// No file yet, and the nearest existing folder on the path cannot be written.
    NotWritable(PathBuf),
    /// The path cannot be read. The text holds the cause.
    Unreadable(String),
}

/// One line of the check, with the mark it gets.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Pass(String),
    Warn(String),
}

/// Print the session-log path check. Prints nothing when no file path is in use.
pub(super) fn run_doctor_session_log_path(use_color: bool, config: &Config) {
    let Some(line) = report(&config.session_log_resolved()) else {
        return;
    };
    match line {
        Line::Pass(text) => eprintln!("{} {text}", super::super::doctor_pass(use_color)),
        Line::Warn(text) => eprintln!("{} {text}", super::super::doctor_warning(use_color)),
    }
}

/// The check line for `log`. The path is in use when the file sink is on, or when a path is set:
/// the transcript reaper and `llmenv clean` read a configured path even with the sink off.
fn report(log: &SessionLog) -> Option<Line> {
    let enabled = log.file.as_ref().is_some_and(|f| f.enabled);
    if !enabled && log.file_path().is_none() {
        return None;
    }
    let path = match crate::session_log::resolve_file_path(log.file_path()) {
        Ok(path) => path,
        Err(e) => {
            return Some(Line::Warn(format!(
                "session log: cannot resolve the file path: {e:#}"
            )));
        }
    };
    let shown = path.display();
    Some(match check_path(&path) {
        Verdict::Usable { exists: true } => Line::Pass(format!("session log file {shown}")),
        Verdict::Usable { exists: false } => Line::Pass(format!(
            "session log file {shown} (created on the first event)"
        )),
        Verdict::NotRegular(kind) => Line::Warn(format!(
            "session log file {shown} is a {kind}, not a regular file, so no event is written. \
             Fix: remove the {kind}, or set session_log.file.path to a regular file."
        )),
        Verdict::NotWritable(folder) => Line::Warn(format!(
            "session log file {shown} cannot be created, because {} is not writable. Fix: \
             correct the permissions, or set session_log.file.path to a writable location.",
            folder.display()
        )),
        Verdict::Unreadable(cause) => Line::Warn(format!(
            "session log file {shown} cannot be read: {cause}. Fix: correct the permissions, \
             or set session_log.file.path to a writable location."
        )),
    })
}

/// Judge `path` without following a symlink, as the sink opens it.
fn check_path(path: &Path) -> Verdict {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Verdict::Usable { exists: true },
        Ok(meta) => Verdict::NotRegular(kind_name(meta.file_type())),
        Err(e) if e.kind() == ErrorKind::NotFound => match unwritable_folder(path) {
            Some(folder) => Verdict::NotWritable(folder),
            None => Verdict::Usable { exists: false },
        },
        Err(e) => Verdict::Unreadable(e.to_string()),
    }
}

/// The nearest existing folder above `path`, when this user cannot write to it. The sink creates
/// the missing folders and the file in it, so that folder decides whether the first event works.
#[cfg(unix)]
fn unwritable_folder(path: &Path) -> Option<PathBuf> {
    let folder = path.ancestors().skip(1).find(|p| p.exists())?;
    rustix::fs::access(folder, rustix::fs::Access::WRITE_OK)
        .is_err()
        .then(|| folder.to_path_buf())
}

#[cfg(not(unix))]
fn unwritable_folder(_path: &Path) -> Option<PathBuf> {
    None
}

fn kind_name(ft: FileType) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_fifo() {
            return "FIFO";
        }
        if ft.is_socket() {
            return "socket";
        }
        if ft.is_block_device() || ft.is_char_device() {
            return "device";
        }
    }
    if ft.is_symlink() {
        "symlink"
    } else if ft.is_dir() {
        "directory"
    } else {
        "special file"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn log(yaml: &str) -> SessionLog {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn file_log(enabled: bool, path: &Path) -> SessionLog {
        log(&format!(
            "file:\n  enabled: {enabled}\n  path: {}\n",
            path.display()
        ))
    }

    #[test]
    fn no_line_when_the_sink_is_off_and_no_path_is_set() {
        assert_eq!(report(&log("file: false\n")), None);
    }

    #[test]
    fn a_set_path_is_checked_even_when_the_sink_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let line = report(&file_log(false, &dir.path().join("s.jsonl")));
        assert!(matches!(line, Some(Line::Pass(_))), "{line:?}");
    }

    #[test]
    fn a_regular_file_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let line = report(&file_log(true, &path)).unwrap();
        assert_eq!(
            line,
            Line::Pass(format!("session log file {}", path.display()))
        );
    }

    #[test]
    fn a_missing_file_passes_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let line = report(&file_log(true, &dir.path().join("new").join("s.jsonl"))).unwrap();
        assert!(
            matches!(&line, Line::Pass(t) if t.contains("created on the first event")),
            "{line:?}"
        );
    }

    #[test]
    fn a_relative_path_warns_with_the_key() {
        let line = report(&log("file:\n  enabled: true\n  path: logs/s.jsonl\n")).unwrap();
        assert!(
            matches!(&line, Line::Warn(t) if t.contains("session_log.file.path")),
            "{line:?}"
        );
    }

    #[test]
    fn a_directory_warns_with_its_kind_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let line = report(&file_log(true, dir.path())).unwrap();
        assert!(
            matches!(&line, Line::Warn(t) if t.contains("is a directory") && t.contains("Fix:")),
            "{line:?}"
        );
    }

    #[test]
    fn a_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(check_path(dir.path()), Verdict::NotRegular("directory"));
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_file_under_a_read_only_folder_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let verdict = check_path(&locked.join("sub").join("s.jsonl"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root can write to any folder, so the check has nothing to report there.
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert_eq!(verdict, Verdict::NotWritable(locked));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_regular_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.jsonl");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("s.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(check_path(&link), Verdict::NotRegular("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(check_path(&path), Verdict::NotRegular("FIFO"));
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(check_path(&path), Verdict::NotRegular("socket"));
    }

    #[cfg(unix)]
    #[test]
    fn a_device_is_rejected() {
        assert_eq!(
            check_path(Path::new("/dev/null")),
            Verdict::NotRegular("device")
        );
    }
}
