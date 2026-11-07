use super::*;
use crate::session_log::rotation::{KEPT_ROTATIONS, rotation_path};
use std::fs::File;

#[test]
fn append_writes_lines_and_preserves_existing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    let sink = FileSink::new(path.clone());
    sink.append("{\"a\":1}");
    sink.append("{\"b\":2}");
    let body = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "{\"a\":1}");
    assert_eq!(lines[1], "{\"b\":2}");
}

#[cfg(unix)]
#[test]
fn append_creates_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    FileSink::new(path.clone()).append("{}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "group/other bits must be unset: {mode:o}");
}

// #1186: append creates its parent directory owner-only, not just the file.
#[cfg(unix)]
#[test]
fn append_creates_parent_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("s.jsonl");
    FileSink::new(path.clone()).append("{}");
    let mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "log dir must be owner-only, got {mode:o}");
}

/// `try_append` re-opens the destination on every call with no symlink
/// check — a pre-existing symlink there gets appended-to (and its
/// permissions rewritten) rather than refused, the same TOCTOU class
/// already fixed for other writers in this codebase (#1341, #1422,
/// #1423, #1427, #1429; extended here since append can't safely
/// unlink-and-replace the way those fixes did — that would destroy the
/// log history append is meant to preserve) (#1431).
#[cfg(unix)]
#[test]
fn try_append_does_not_write_through_a_symlinked_destination() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "must-not-be-touched\n").unwrap();
    let path = dir.path().join("session-log.jsonl");
    std::os::unix::fs::symlink(&victim, &path).unwrap();

    let sink = FileSink::new(path.clone());
    let result = sink.try_append("{\"attacker\":true}");

    assert!(
        result.is_err(),
        "appending through a pre-existing symlink must be refused"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "must-not-be-touched\n",
        "the symlink's target must be untouched"
    );
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink itself must be left alone, not replaced or removed"
    );
}

#[cfg(unix)]
#[test]
fn append_re_protects_a_pre_existing_world_readable_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    FileSink::new(path.clone()).append("{}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o077,
        0,
        "append must lock down a pre-existing looser-permission file: {mode:o}"
    );
}

#[test]
fn an_append_to_a_full_file_starts_the_new_file_with_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    // A sparse file of the real cap: no 64 MiB of data is written.
    File::create(&path).unwrap().set_len(MAX_BYTES).unwrap();
    FileSink::new(path.clone()).append("{\"after\":1}");
    let body = std::fs::read_to_string(&path).unwrap();
    let mut lines = body.lines();
    assert_eq!(
        lines.next(),
        Some(rotation_marker("session-log.jsonl.1").as_str())
    );
    assert_eq!(lines.next(), Some("{\"after\":1}"));
    assert!(rotation_path(&path, 1).exists());
}

#[cfg(unix)]
#[test]
fn append_rejects_a_symlink_without_moving_it_or_its_target() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.jsonl");
    let path = dir.path().join("session-log.jsonl");
    File::create(&target).unwrap().set_len(MAX_BYTES).unwrap();
    symlink(&target, &path).unwrap();

    let error = FileSink::new(path.clone())
        .try_append("{\"event\":true}")
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("Fix:"));
    assert!(error.to_string().contains("session_log.file.path"));
    assert_eq!(std::fs::read_link(&path).unwrap(), target);
    assert_eq!(std::fs::metadata(&target).unwrap().len(), MAX_BYTES);
    assert!(!rotation_path(&path, 1).exists());
}

#[cfg(unix)]
#[test]
fn append_rejects_a_fifo_before_opening_it() {
    use std::os::unix::fs::FileTypeExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo exited with {status}");

    let error = FileSink::new(path.clone())
        .try_append("{\"event\":true}")
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("not a regular file"));
    assert!(
        std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_fifo()
    );
}

#[test]
fn a_failed_rotation_still_appends_the_event_to_the_active_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-log.jsonl");
    File::create(&path).unwrap().set_len(MAX_BYTES).unwrap();
    // A non-empty directory where the oldest generation belongs makes its removal fail.
    let oldest = rotation_path(&path, KEPT_ROTATIONS);
    std::fs::create_dir(&oldest).unwrap();
    std::fs::write(oldest.join("keep"), b"k").unwrap();
    FileSink::new(path.clone()).append("{\"after\":1}");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.ends_with("{\"after\":1}\n"), "the event was lost");
    // The sparse fixture has no newline, so the marker shares the last line of NUL bytes.
    let marker = body.find(r#"{"error":"#).unwrap();
    let event = body.rfind(r#"{"after":1}"#).unwrap();
    assert!(marker < event, "the marker must come before the event");
    let line = body[marker..event].trim_end();
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["llmenv_log"], "rotation_failed", "{line}");
}
#[test]
fn a_configured_tilde_path_is_expanded() {
    let home = std::env::var("HOME").unwrap();
    let got = resolve_file_path_with(Some("~/logs/s.jsonl"), || unreachable!()).unwrap();
    assert_eq!(got, Path::new(&home).join("logs/s.jsonl"));
}

#[test]
fn a_relative_configured_path_is_rejected_with_the_key() {
    let err = resolve_file_path_with(Some("logs/s.jsonl"), || unreachable!()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("session_log.file.path"), "{msg}");
    assert!(msg.contains("logs/s.jsonl"), "{msg}");
}

#[test]
fn no_configured_path_uses_the_default() {
    let got =
        resolve_file_path_with(None, || Ok(PathBuf::from("/state/session-log.jsonl"))).unwrap();
    assert_eq!(got, PathBuf::from("/state/session-log.jsonl"));
}

mod properties {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// An absolute configured path comes back unchanged, and the default is not read.
        #[test]
        fn an_absolute_configured_path_is_kept(rest in "[a-zA-Z0-9_./-]{0,40}") {
            let raw = format!("/{rest}");
            let got = resolve_file_path_with(Some(&raw), || unreachable!()).unwrap();
            prop_assert_eq!(got, PathBuf::from(&raw));
        }

        /// A relative configured path, the empty path included, is an error that names the key.
        #[test]
        fn a_relative_configured_path_is_an_error(raw in "([a-zA-Z0-9_.-][a-zA-Z0-9_./-]{0,30})?") {
            prop_assume!(!raw.starts_with('~'));
            let err = resolve_file_path_with(Some(&raw), || unreachable!()).unwrap_err();
            let msg = format!("{err:#}");
            prop_assert!(msg.contains("session_log.file.path"), "{}", msg);
        }

        /// Each distinct path reports once, and every later failure for it does not.
        #[test]
        fn each_path_reports_its_first_failure_only(
            names in proptest::collection::btree_set("[a-z]{1,8}", 1..6),
            repeats in 1usize..4,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let paths: Vec<PathBuf> = names.iter().map(|n| dir.path().join(n)).collect();
            let firsts = paths.iter().filter(|p| first_failure(p)).count();
            prop_assert_eq!(firsts, paths.len());
            for _ in 0..repeats {
                prop_assert!(paths.iter().all(|p| !first_failure(p)));
            }
        }
    }
}

// #3136: a sink that keeps failing reports the failure once, not once per event.
#[cfg(unix)]
#[test]
fn a_repeated_append_failure_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.jsonl");
    std::fs::write(&target, "").unwrap();
    let link = dir.path().join("link.jsonl");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let log = dir.path().join("log.jsonl");
    crate::session_log::tracing_layer::capture_file_logs_at(
        &log,
        tracing_subscriber::filter::LevelFilter::TRACE,
        || {
            FileSink::new(link.clone()).append("{}");
            FileSink::new(link.clone()).append("{}");
        },
    );
    let body = std::fs::read_to_string(&log).unwrap();
    let reports = body.lines().filter(|l| l.contains("append to")).count();
    assert_eq!(reports, 1, "log body: {body}");
}
