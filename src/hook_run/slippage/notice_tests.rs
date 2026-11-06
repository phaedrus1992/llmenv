//! Agent notices for the slippage stats log: an unreadable, a corrupt, and an unsaved log, and a
//! session id that is not a plain name.

use super::*;

fn notices() -> String {
    crate::agent_notice::current().unwrap_or_default()
}

#[test]
fn an_unreadable_stats_log_names_the_path_and_the_fix() {
    let state = tempfile::tempdir().unwrap();
    // A folder where the log should be cannot be read as text.
    std::fs::create_dir_all(stats_path(state.path(), "s-unreadable")).unwrap();
    load_stats(state.path(), "s-unreadable");
    assert!(
        notices().contains("cannot read the slippage stats log"),
        "{}",
        notices()
    );
}

#[test]
fn a_corrupt_stats_log_names_the_path_and_the_fix() {
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(state.path().join("slippage")).unwrap();
    std::fs::write(stats_path(state.path(), "s-corrupt"), b"{not json").unwrap();
    load_stats(state.path(), "s-corrupt");
    assert!(notices().contains("is corrupt"), "{}", notices());
}

#[test]
fn an_unsaved_stats_log_names_the_fix() {
    let state = tempfile::tempdir().unwrap();
    // A file where the slippage folder should be stops the save before it writes.
    std::fs::write(state.path().join("slippage"), b"x").unwrap();
    save_stats(state.path(), "s-unsaved", &SessionStats::default());
    assert!(
        notices().contains("cannot save the slippage stats"),
        "{}",
        notices()
    );
}

#[test]
fn a_session_id_that_is_not_a_plain_name_names_the_fix() {
    assert!(safe_session(Some("../escape")).is_none());
    assert!(notices().contains("not a plain file name"), "{}", notices());
}

// #3332: the deny names the stats log that caused it, and not the file the agent tried to write.
#[test]
fn a_write_deny_on_a_corrupt_stats_log_names_the_log() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();
    std::fs::create_dir_all(state.path().join("slippage")).unwrap();
    let log = stats_path(state.path(), "s-deny");
    std::fs::write(&log, b"{not json").unwrap();
    let cfg = SlippageControl {
        enabled: true,
        read_before_edit: true,
        ..Default::default()
    };
    let payload = serde_json::json!({
        "tool_name": "Write",
        "tool_input": { "file_path": file.to_str().unwrap() },
    });

    let deny = handle_pre_tool_use(Some(&cfg), &payload, Some("s-deny"), state.path());

    assert!(deny.starts_with("__DENY__:"), "{deny}");
    assert!(deny.contains(&log.display().to_string()), "{deny}");
    assert!(deny.contains("Fix:"), "{deny}");
    assert!(!deny.contains("haven't read"), "{deny}");
}

// #3348 review: a stats log reads back exactly what was saved.
#[test]
fn a_saved_stats_log_reads_back_unchanged() {
    use proptest::prelude::*;
    proptest!(|(paths in proptest::collection::btree_set("[a-z/.]{1,12}", 0..6))| {
        let state = tempfile::tempdir().unwrap();
        let stats = SessionStats {
            paths,
            ..Default::default()
        };
        save_stats(state.path(), "s-round", &stats);
        let back = read_stats(state.path(), "s-round").unwrap();
        prop_assert_eq!(back.paths, stats.paths);
    });
}

// #3348 review: a stats log reads as Ok exactly when serde_json parses its bytes.
#[test]
fn read_stats_is_ok_exactly_when_the_log_parses() {
    use proptest::prelude::*;
    proptest!(|(bytes in proptest::collection::vec(any::<u8>(), 0..200))| {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("slippage")).unwrap();
        std::fs::write(stats_path(state.path(), "s-prop"), &bytes).unwrap();
        let parses = serde_json::from_slice::<SessionStats>(&bytes).is_ok();
        prop_assert_eq!(read_stats(state.path(), "s-prop").is_ok(), parses);
    });
}

// #3348 CI: a missing stats log is the first-use case. It reads as Ok and empty, and it is not
// an error, so a Write deny does not name a stats log problem.
#[test]
fn a_missing_stats_log_reads_as_empty() {
    let state = tempfile::tempdir().unwrap();
    let stats = read_stats(state.path(), "s-missing").unwrap();
    assert!(stats.paths.is_empty());
}

fn write_payload(path: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "tool_name": "Write",
        "tool_input": { "file_path": path.to_str().unwrap() },
    })
}

fn enabled() -> SlippageControl {
    SlippageControl {
        enabled: true,
        read_before_edit: true,
        ..Default::default()
    }
}

// #3389: a Write deny for an unread file is one plain sentence run, with no stray spaces.
#[test]
fn an_unread_write_deny_has_no_run_of_spaces_and_no_resolve_note() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();
    let deny = handle_pre_tool_use(
        Some(&enabled()),
        &write_payload(&file),
        Some("s-plain"),
        state.path(),
    );
    assert!(deny.starts_with("__DENY__:"), "{deny}");
    assert!(deny.contains("haven't read it"), "{deny}");
    assert!(!deny.contains("  "), "{deny}");
    assert!(!deny.contains("cannot resolve"), "{deny}");
}

// #3389: a path that exists but cannot be resolved, such as a dangling link, names that cause.
#[cfg(unix)]
#[test]
fn a_write_deny_for_an_unresolvable_path_names_the_cause() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let link = work.path().join("dangling");
    std::os::unix::fs::symlink(work.path().join("missing-target"), &link).unwrap();
    let deny = handle_pre_tool_use(
        Some(&enabled()),
        &write_payload(&link),
        Some("s-link"),
        state.path(),
    );
    assert!(deny.starts_with("__DENY__:"), "{deny}");
    assert!(deny.contains("cannot resolve"), "{deny}");
    assert!(deny.contains("read it again by this path"), "{deny}");
}
