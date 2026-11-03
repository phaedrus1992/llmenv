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
