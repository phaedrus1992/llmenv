use super::*;

#[test]
fn metrics_summarise_the_read_to_edit_ratio() {
    let state = tempfile::tempdir().unwrap();
    let cfg = SlippageControl {
        enabled: true,
        metrics: true,
        ..Default::default()
    };
    for tool in ["Read", "Read", "Read", "Read", "Edit", "Write"] {
        handle_post_tool_use(
            Some(&cfg),
            &serde_json::json!({ "tool_name": tool }),
            Some("s1"),
            state.path(),
        );
    }
    let summary = session_metrics_summary(Some(&cfg), Some("s1"), state.path()).unwrap();
    assert!(summary.contains("6 calls"));
    assert!(summary.contains("4 reads"));
    assert!(summary.contains("2 edits"));
    assert!(summary.contains("2.0 reads per edit"));
}

#[test]
fn a_session_id_that_is_not_a_plain_name_writes_no_state_file() {
    let state = tempfile::tempdir().unwrap();
    let cfg = SlippageControl {
        enabled: true,
        metrics: true,
        read_before_edit: true,
        ..Default::default()
    };
    for bad in ["../escape", "a/b", ""] {
        handle_post_tool_use(
            Some(&cfg),
            &serde_json::json!({ "tool_name": "Read" }),
            Some(bad),
            state.path(),
        );
        let read = payload("Read", state.path());
        handle_pre_tool_use(Some(&cfg), &read, Some(bad), state.path());
        assert!(session_metrics_summary(Some(&cfg), Some(bad), state.path()).is_none());
    }
    assert!(!state.path().join("slippage").exists());
    assert!(!state.path().parent().unwrap().join("escape.json").exists());
}

#[test]
fn metrics_report_no_edits_rather_than_dividing_by_zero() {
    let state = tempfile::tempdir().unwrap();
    let cfg = SlippageControl {
        enabled: true,
        metrics: true,
        ..Default::default()
    };
    handle_post_tool_use(
        Some(&cfg),
        &serde_json::json!({ "tool_name": "Read" }),
        Some("s1"),
        state.path(),
    );
    let summary = session_metrics_summary(Some(&cfg), Some("s1"), state.path()).unwrap();
    assert!(summary.contains("no edits"), "{summary}");
}

#[test]
fn metrics_are_silent_with_nothing_counted_or_the_layer_off() {
    let state = tempfile::tempdir().unwrap();
    let on = SlippageControl {
        enabled: true,
        metrics: true,
        ..Default::default()
    };
    assert!(session_metrics_summary(Some(&on), Some("empty"), state.path()).is_none());
    assert!(session_metrics_summary(None, Some("s1"), state.path()).is_none());
    assert!(session_metrics_summary(Some(&on), None, state.path()).is_none());

    let off = SlippageControl {
        metrics: false,
        ..on
    };
    handle_post_tool_use(
        Some(&off),
        &serde_json::json!({ "tool_name": "Read" }),
        Some("s2"),
        state.path(),
    );
    assert!(session_metrics_summary(Some(&off), Some("s2"), state.path()).is_none());
}

// The two layers share one file, so counting must not discard the read
// log and vice versa.
#[test]
fn metrics_and_the_read_log_coexist_in_one_state_file() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("f.txt");
    std::fs::write(&file, "x").unwrap();
    let cfg = SlippageControl {
        enabled: true,
        metrics: true,
        read_before_edit: true,
        ..Default::default()
    };

    handle_pre_tool_use(
        Some(&cfg),
        &payload("Read", &file),
        Some("s1"),
        state.path(),
    );
    handle_post_tool_use(
        Some(&cfg),
        &serde_json::json!({ "tool_name": "Read" }),
        Some("s1"),
        state.path(),
    );

    assert!(
        session_metrics_summary(Some(&cfg), Some("s1"), state.path()).is_some(),
        "the count survived the read being recorded"
    );
    assert_eq!(
        handle_pre_tool_use(
            Some(&cfg),
            &payload("Write", &file),
            Some("s1"),
            state.path()
        ),
        "",
        "the read record survived the count being written"
    );
}

#[test]
fn reading_one_file_does_not_authorise_writing_a_different_one() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let read_me = work.path().join("read.txt");
    let other = work.path().join("other.txt");
    std::fs::write(&read_me, "a").unwrap();
    std::fs::write(&other, "b").unwrap();

    handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &read_me),
        Some("s1"),
        state.path(),
    );
    assert!(
        handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &other),
            Some("s1"),
            state.path()
        )
        .starts_with("__DENY__:"),
        "a read of one file must not vouch for a write to another"
    );
}

// A read recorded under one spelling and a write checked under another
// would deny a write to a file that *was* read — the false positive most
// likely to get the layer switched off.
#[test]
fn a_read_by_one_spelling_authorises_a_write_by_another() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("f.txt");
    std::fs::write(&file, "x").unwrap();

    let nested = work.path().join("sub/..").join("f.txt");
    std::fs::create_dir_all(work.path().join("sub")).unwrap();

    handle_pre_tool_use(
        Some(&on()),
        &payload("Read", &nested),
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
        "the same file by a different spelling must count as read"
    );
}
// Each layer's gate is `!enabled || !layer`. Flipping the `||` to `&&`
// means the master switch alone no longer stops the layer, and the
// existing tests can't see it: they turn the *layer* off while leaving
// `enabled` true, which both spellings handle the same way. These turn the
// master switch off with the layer on — the only shape that separates them.
#[test]
fn master_switch_off_stops_counting_even_with_metrics_on() {
    let state = tempfile::tempdir().unwrap();
    let master_off = SlippageControl {
        enabled: false,
        metrics: true,
        ..Default::default()
    };
    handle_post_tool_use(
        Some(&master_off),
        &serde_json::json!({ "tool_name": "Read" }),
        Some("s1"),
        state.path(),
    );

    // Read the file directly: asking `session_metrics_summary` would gate
    // on the same config and hide whether anything was written.
    let stats = load_stats(state.path(), "s1");
    assert!(
        stats.tools.is_empty(),
        "nothing should have been counted, got {:?}",
        stats.tools
    );
}

#[test]
fn master_switch_off_suppresses_a_summary_that_has_counts() {
    let state = tempfile::tempdir().unwrap();
    let on = SlippageControl {
        enabled: true,
        metrics: true,
        ..Default::default()
    };
    for _ in 0..3 {
        handle_post_tool_use(
            Some(&on),
            &serde_json::json!({ "tool_name": "Read" }),
            Some("s1"),
            state.path(),
        );
    }
    assert!(
        session_metrics_summary(Some(&on), Some("s1"), state.path()).is_some(),
        "fixture must have counts, or the master-switch check proves nothing"
    );

    let master_off = SlippageControl {
        enabled: false,
        ..on
    };
    assert!(
        session_metrics_summary(Some(&master_off), Some("s1"), state.path()).is_none(),
        "the master switch suppresses a summary that would otherwise exist"
    );
}

#[test]
fn master_switch_off_stops_the_transcript_scan_with_layers_on() {
    let file = transcript_with(&[user_line("should I?")]);
    let master_off = SlippageControl {
        enabled: false,
        answer_before_act: true,
        explain_before_act: true,
        ..Default::default()
    };
    assert_eq!(
        handle_transcript_scan(Some(&master_off), &bash_payload(&file, "chmod 777 x")),
        "",
        "the master switch stops layers that would otherwise both fire"
    );
}

// `explain_before_act && !assistant_spoke_since`: flipping to `||` makes
// the layer fire whenever the assistant hasn't spoken, regardless of its
// own toggle. Needs a case where the assistant is silent, the command is
// modifying, and only `answer_before_act` is on — with a statement rather
// than a question, so the answer layer stays quiet too.
#[test]
fn explain_layer_stays_off_when_only_the_answer_layer_is_enabled() {
    let file = transcript_with(&[user_line("clean up the build directory")]);
    assert_eq!(
        handle_transcript_scan(
            Some(&scan_cfg(true, false)),
            &bash_payload(&file, "chmod 777 build")
        ),
        "",
        "a disabled explain layer must not deny a modifying command"
    );
}

#[test]
fn explain_layer_names_the_nesting_limit_for_a_command_too_deep_to_split() {
    let file = transcript_with(&[user_line("clean up the build directory")]);
    let nested = format!("{}ls{}", "$(".repeat(40), ")".repeat(40));
    let denied =
        handle_transcript_scan(Some(&scan_cfg(false, true)), &bash_payload(&file, &nested));
    assert!(denied.starts_with("__DENY__:"), "{denied}");
    assert!(denied.contains("32 levels deep"), "{denied}");
    assert!(denied.contains("Split the command"), "{denied}");
    assert!(!denied.contains("changes something"), "{denied}");
}
