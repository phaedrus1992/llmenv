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

// #3332: the deny names the stats log and its fix. It does not point the agent at the file it
// tried to write, because reading that file does not repair the log.
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
