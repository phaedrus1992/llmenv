use super::*;

#[test]
fn the_stats_lock_is_taken_once_per_session() {
    let dir = tempfile::tempdir().unwrap();
    let held = stats_lock(dir.path(), "s1");
    assert!(held.is_some(), "a free lock must be taken");
    assert!(
        stats_lock(dir.path(), "s1").is_none(),
        "a held lock must not be taken twice"
    );
    drop(held);
}

#[test]
fn emits_the_checklist_when_the_layer_is_on() {
    let out = handle_stop(Some(&cfg(true, true)));
    assert!(out.contains("tests"), "{out}");
    assert!(
        !out.starts_with("__DENY__:"),
        "the layer is advisory, never blocking"
    );
}

// The master switch has to win over the per-layer default, or "turn this
// feature off" wouldn't actually turn it off — `self_critique` defaults to
// true, so an inert `enabled: false` is the only thing standing between a
// user who disabled the feature and a checklist on every turn.
#[test]
fn master_switch_off_silences_a_layer_that_is_on() {
    assert_eq!(handle_stop(Some(&cfg(false, true))), "");
}

#[test]
fn layer_off_silences_it_while_the_feature_stays_on() {
    assert_eq!(handle_stop(Some(&cfg(true, false))), "");
}

#[test]
fn absent_config_is_silent() {
    assert_eq!(handle_stop(None), "");
}

#[test]
fn default_config_is_silent_because_the_feature_is_opt_in() {
    assert_eq!(handle_stop(Some(&SlippageControl::default())), "");
}
#[test]
fn turn_digest_follows_its_own_toggle_and_the_master_switch() {
    let on = SlippageControl {
        enabled: true,
        rule_reinjection: true,
        ..Default::default()
    };
    assert!(handle_turn(Some(&on)).contains("whole task"));

    let layer_off = SlippageControl {
        rule_reinjection: false,
        ..on.clone()
    };
    assert_eq!(handle_turn(Some(&layer_off)), "");

    let master_off = SlippageControl {
        enabled: false,
        ..on
    };
    assert_eq!(handle_turn(Some(&master_off)), "");
    assert_eq!(handle_turn(None), "");
    assert_eq!(handle_turn(Some(&SlippageControl::default())), "");
}

// The digest is re-sent on every turn, so its size is a recurring cost
// rather than a one-off. Keeping it short is the design, and a cap makes
// that explicit instead of leaving it to whoever edits the string next.
#[test]
fn turn_digest_stays_small_enough_to_repeat_every_turn() {
    let text = handle_turn(Some(&SlippageControl {
        enabled: true,
        rule_reinjection: true,
        ..Default::default()
    }));
    assert!(
        text.len() < 600,
        "the per-turn digest is {} bytes; it is sent on every turn",
        text.len()
    );
}
#[test]
fn a_monitor_started_cat_counts_as_a_read() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();
    let cat = serde_json::json!({
        "tool_name": "Monitor",
        "tool_input": { "command": format!("cat {}", file.display()) },
    });
    handle_post_tool_use(Some(&on()), &cat, Some("s1"), state.path());

    let out = handle_pre_tool_use(
        Some(&on()),
        &payload("Write", &file),
        Some("s1"),
        state.path(),
    );
    assert!(!out.starts_with("__DENY__:"), "{out}");
}

#[test]
fn denies_writing_over_an_existing_file_that_was_never_read() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();

    let out = handle_pre_tool_use(
        Some(&on()),
        &payload("Write", &file),
        Some("s1"),
        state.path(),
    );
    assert!(out.starts_with("__DENY__:"), "{out}");
    assert!(out.contains("existing.txt"), "{out}");
}

#[test]
fn allows_the_write_once_the_file_has_been_read() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();

    handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &file),
        Some("s1"),
        state.path(),
    );
    assert_eq!(
        handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &file),
            Some("s1"),
            state.path()
        ),
        ""
    );
}

// Creating a file is not a mistake. Denying it would make the layer refuse
// ordinary work, which is how a guard gets switched off entirely.
#[test]
fn allows_writing_a_file_that_does_not_exist_yet() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    assert_eq!(
        handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &work.path().join("new.txt")),
            Some("s1"),
            state.path()
        ),
        ""
    );
}

// The log is per session, so one session's read must not vouch for
// another's write — sessions run concurrently.
#[test]
fn a_read_in_one_session_does_not_authorise_a_write_in_another() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("shared.txt");
    std::fs::write(&file, "x").unwrap();

    handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &file),
        Some("a"),
        state.path(),
    );
    assert!(
        handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &file),
            Some("b"),
            state.path()
        )
        .starts_with("__DENY__:")
    );
}

#[test]
fn edit_is_left_to_claude_codes_own_guard() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "x").unwrap();
    assert_eq!(
        handle_pre_tool_use(
            Some(&on()),
            &payload("Edit", &file),
            Some("s1"),
            state.path()
        ),
        "",
        "Edit already requires a prior read; duplicating that only adds false denials"
    );
}

#[test]
fn a_corrupt_log_allows_the_write_rather_than_wedging_the_agent() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("f.txt");
    std::fs::write(&file, "x").unwrap();
    std::fs::create_dir_all(state.path().join("slippage")).unwrap();
    std::fs::write(state.path().join("slippage/s1.json"), b"{not json").unwrap();

    // Fail-soft means the deny still fires (nothing is known to have been
    // read) but nothing panics and the file is rewritten cleanly on the
    // next Read.
    let out = handle_pre_tool_use(
        Some(&on()),
        &payload("Write", &file),
        Some("s1"),
        state.path(),
    );
    assert!(out.starts_with("__DENY__:"), "{out}");
    handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &file),
        Some("s1"),
        state.path(),
    );
    assert_eq!(
        handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &file),
            Some("s1"),
            state.path()
        ),
        "",
        "a corrupt log must not be permanently poisoned"
    );
}

#[test]
fn write_guard_follows_the_master_switch_and_its_own_toggle() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("f.txt");
    std::fs::write(&file, "x").unwrap();
    let p = payload("Write", &file);

    let master_off = SlippageControl {
        enabled: false,
        ..on()
    };
    assert_eq!(
        handle_pre_tool_use(Some(&master_off), &p, Some("s1"), state.path()),
        ""
    );
    let layer_off = SlippageControl {
        read_before_edit: false,
        ..on()
    };
    assert_eq!(
        handle_pre_tool_use(Some(&layer_off), &p, Some("s1"), state.path()),
        ""
    );
    assert_eq!(handle_pre_tool_use(None, &p, Some("s1"), state.path()), "");
    // No session id: nothing to scope the log to, so the layer stands down
    // rather than sharing one log across concurrent sessions.
    assert_eq!(handle_pre_tool_use(Some(&on()), &p, None, state.path()), "");
}

// #3349: a Read rewrites the stats log. A corrupt log must survive in a `.corrupt` copy, and the
// notice must name that copy.
#[test]
fn a_read_keeps_a_corrupt_stats_log_before_it_rewrites_the_log() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();
    let log = stats_path(state.path(), "s1");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"{not json").unwrap();
    let kept = std::path::PathBuf::from(format!("{}.corrupt", log.display()));

    handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &file),
        Some("s1"),
        state.path(),
    );

    assert_eq!(std::fs::read(&kept).unwrap(), b"{not json");
    let notes = crate::agent_notice::current().unwrap_or_default();
    assert!(notes.contains(&kept.display().to_string()), "{notes}");
}

// #3349: a second corruption must not replace the copy kept by the first one.
#[test]
fn a_repeat_corruption_keeps_the_earlier_copy() {
    let state = tempfile::tempdir().unwrap();
    let log = stats_path(state.path(), "s1");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"{not json").unwrap();
    let first = format!("{}.corrupt", log.display());
    std::fs::write(&first, b"first copy").unwrap();

    let kept = load_stats_for_rewrite(state.path(), "s1", true);

    assert!(kept.is_some());
    assert_eq!(std::fs::read(&first).unwrap(), b"first copy");
    assert_eq!(std::fs::read(format!("{first}.1")).unwrap(), b"{not json");
}

// #3349: a copy name that is taken by a folder is skipped, and the bytes go to the next name.
#[test]
fn a_blocked_copy_name_moves_the_bytes_to_the_next_free_copy() {
    let state = tempfile::tempdir().unwrap();
    let log = stats_path(state.path(), "s1");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"{not json").unwrap();
    let first = format!("{}.corrupt", log.display());
    std::fs::create_dir(&first).unwrap();

    assert!(load_stats_for_rewrite(state.path(), "s1", true).is_some());

    assert_eq!(std::fs::read(format!("{first}.1")).unwrap(), b"{not json");
    assert!(std::path::Path::new(&first).is_dir());
}

// #3349: a file that cannot be read is not corrupt. Its bytes stay in place, and the Read is not
// recorded.
#[cfg(unix)]
#[test]
fn an_unreadable_stats_log_stays_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("existing.txt");
    std::fs::write(&file, "important").unwrap();
    let log = stats_path(state.path(), "s1");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"{not json").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &file),
        Some("s1"),
        state.path(),
    );

    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read(&log).unwrap(), b"{not json");
    assert!(!std::path::Path::new(&format!("{}.corrupt", log.display())).exists());
    assert_eq!(out, "", "the Read itself is still allowed");
}

// #3349: without the session's stats lock, another hook can write a new log between the read and
// the rename. The move is refused, and the rewrite is skipped.
#[test]
fn an_unlocked_call_does_not_move_a_corrupt_stats_log() {
    let state = tempfile::tempdir().unwrap();
    let log = stats_path(state.path(), "s1");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"{not json").unwrap();

    assert!(load_stats_for_rewrite(state.path(), "s1", false).is_none());

    assert_eq!(std::fs::read(&log).unwrap(), b"{not json");
    assert!(!std::path::Path::new(&format!("{}.corrupt", log.display())).exists());
}
