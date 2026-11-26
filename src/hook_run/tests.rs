use crate::trace::{HookTiming, hook_marker};
use serde_json::json;

#[test]
fn a_notice_with_no_channel_is_logged_at_error_level() {
    let logs = crate::test_log_capture::capture_logs(|| {
        log_notice_without_channel("", Some("the session map is locked"), "session_end");
    });
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(logs.contains("the session map is locked"), "{logs}");
}

#[test]
fn memory_with_no_channel_is_logged_and_memory_with_one_is_not() {
    let dropped = crate::test_log_capture::capture_logs(|| {
        log_memory_without_channel("", "project memory text", "session_start");
    });
    assert!(
        dropped.contains("ERROR") && dropped.contains("19 bytes"),
        "{dropped}"
    );
    let delivered = crate::test_log_capture::capture_logs(|| {
        log_memory_without_channel("{}", "project memory text", "session_start");
        log_memory_without_channel("", "  ", "session_start");
    });
    assert!(delivered.is_empty(), "{delivered}");
}

#[test]
fn a_notice_with_a_channel_is_not_logged_here() {
    let logs = crate::test_log_capture::capture_logs(|| {
        log_notice_without_channel("{}", Some("notice"), "turn_start");
    });
    assert!(logs.is_empty(), "{logs}");
}

#[test]
fn every_listed_hook_event_name_parses_and_round_trips() {
    for name in HOOK_EVENT_NAMES {
        let event: HookEvent = name.parse().unwrap();
        assert_eq!(event.to_string(), name);
    }
    let err = "nope".parse::<HookEvent>().unwrap_err().to_string();
    for name in HOOK_EVENT_NAMES {
        assert!(err.contains(name), "{name} missing from: {err}");
    }
}

use super::web_fetch::handle_web_fetch_in;
use super::*;
use crate::memory::resolve::default_memory_hook;

#[test]
fn append_pending_notice_joins_with_a_newline_when_text_is_non_empty() {
    let result = append_pending_notice(
        "existing memory context".to_string(),
        Some("config changed".to_string()),
    );
    assert_eq!(result, "existing memory context\nconfig changed");
}

#[test]
fn append_pending_notice_has_no_leading_newline_when_text_is_empty() {
    let result = append_pending_notice(String::new(), Some("config changed".to_string()));
    assert_eq!(result, "config changed");
}

#[test]
fn append_pending_notice_leaves_text_unchanged_when_there_is_no_notice() {
    let result = append_pending_notice("existing memory context".to_string(), None);
    assert_eq!(result, "existing memory context");
}

/// Every event `from_str` accepts. Kept as strings so a new variant that
/// forgets to round-trip is a test failure rather than a silent gap.
const ALL_HOOK_EVENTS: &[&str] = &[
    "session_start",
    "turn_start",
    "session_end",
    "post_session",
    "user_prompt_submit",
    "pre_tool_use",
    "post_tool_use",
    "notification",
    "stop",
    "subagent_stop",
    "pre_compact",
    "post_tool_batch",
    "post_tool_use_failure",
    "subagent_start",
    "subagent_task",
    "post_model_switch",
];

#[test]
fn only_session_end_and_post_session_start_consolidation() {
    let starting: Vec<&str> = ALL_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|e| starts_consolidation(HookEvent::from_str(e).unwrap()))
        .collect();
    // Claude Code registers `session_end` once and never `post_session`,
    // so one session end starts one child (#2355).
    assert_eq!(starting, ["session_end", "post_session"]);
}

thread_local! {
    static SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count_spawn() -> Option<std::process::Child> {
    SPAWNS.with(|c| c.set(c.get() + 1));
    None
}

/// Spawns for one `event` against a config whose only memory entry
/// (tag `mem`) has consolidation set to `enabled`.
fn consolidation_spawns(event: HookEvent, enabled: bool) -> usize {
    let config: crate::config::Config = serde_yaml::from_str(&format!(
        "features:\n  memory:\n    - server_host: h\n      port: 1\n      when: [mem]\n      \
             consolidation:\n        enabled: {enabled}\n"
    ))
    .unwrap();
    let active = crate::scope::ActiveScopes {
        tags: std::collections::BTreeSet::from(["mem".to_string()]),
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    SPAWNS.with(|c| c.set(0));
    maybe_start_consolidation(event, &config, dir.path(), &active, count_spawn);
    SPAWNS.with(std::cell::Cell::get)
}

#[test]
fn session_end_with_consolidation_enabled_spawns_exactly_one_child() {
    assert_eq!(consolidation_spawns(HookEvent::SessionEnd, true), 1);
    assert_eq!(consolidation_spawns(HookEvent::PostSession, true), 1);
}

#[test]
fn no_child_for_other_events_or_disabled_consolidation() {
    assert_eq!(consolidation_spawns(HookEvent::Stop, true), 0);
    assert_eq!(consolidation_spawns(HookEvent::SessionStart, true), 0);
    assert_eq!(consolidation_spawns(HookEvent::SessionEnd, false), 0);
}

#[test]
fn consolidation_run_command_runs_the_consolidation_subcommand() {
    let cmd = consolidation_run_command("/bin/llmenv".into(), None);
    assert_eq!(cmd.get_program(), "/bin/llmenv");
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, ["consolidation-run"]);
}

#[test]
fn each_detached_command_carries_its_checkpoint_path() {
    let path = std::path::Path::new("/s/checkpoints/x.json");
    let args = |cmd: std::process::Command| -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        args(consolidation_run_command("/bin/llmenv".into(), Some(path))),
        ["consolidation-run", "--checkpoint", "/s/checkpoints/x.json"]
    );
    assert_eq!(
        args(icm_store_command("/bin/llmenv".into(), Some(path))),
        ["icm-store", "--checkpoint", "/s/checkpoints/x.json"]
    );
    assert_eq!(
        args(icm_store_command("/bin/llmenv".into(), None)),
        ["icm-store"]
    );
}

#[test]
fn a_web_fetch_writes_a_checkpoint_with_the_store_args() {
    let dir = tempfile::tempdir().unwrap();
    let payload = json!({
        "session_id": "sess-1", "tool_use_id": "tu1", "tool_name": "WebFetch",
        "tool_input": {"url": "https://example.com"}, "tool_response": "body",
        "cwd": dir.path().join("proj-a").display().to_string(),
    });
    std::fs::create_dir(dir.path().join("proj-a")).unwrap();
    let child = handle_web_fetch_in(Some(dir.path()), &payload);
    let listed = checkpoint::checkpoint_entries_for_test(dir.path());
    assert_eq!(listed.len(), 1, "{listed:?}");
    let (kind, inputs, session) = &listed[0];
    assert_eq!(*kind, checkpoint::JobKind::IcmStore);
    assert_eq!(
        inputs["topic"], "web-fetch-proj-a",
        "the page keeps its project (#2911)"
    );
    assert!(
        !inputs[detached_store::REQUEST_ID_FIELD]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert_eq!(session.as_deref(), Some("sess-1"));
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn consolidation_spawn_writes_a_checkpoint_with_a_run_tag_and_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    post_session_consolidation_in(Some(dir.path()));
    let listed = checkpoint::list(dir.path());
    let [checkpoint::Entry::Ready(_, cp)] = &listed[..] else {
        panic!("expected one checkpoint: {listed:?}");
    };
    assert_eq!(cp.kind, checkpoint::JobKind::Consolidation);
    assert!(!cp.inputs["run_tag"].as_str().unwrap().is_empty());
    let cwd = std::env::current_dir().unwrap().display().to_string();
    assert_eq!(cp.cwd.as_deref(), Some(cwd.as_str()));
    // A second run in the same directory is a second job, not the same file.
    post_session_consolidation_in(Some(dir.path()));
    assert_eq!(checkpoint::list(dir.path()).len(), 2);
}

#[test]
fn only_a_fresh_session_start_outside_the_consolidation_child_resumes_jobs() {
    let fresh = json!({ "source": "startup" });
    let guard = std::ffi::OsStr::new("1");
    assert!(resumes_checkpoints(HookEvent::SessionStart, &fresh, None));
    assert!(!resumes_checkpoints(
        HookEvent::SessionStart,
        &json!({ "source": "resume" }),
        None
    ));
    assert!(!resumes_checkpoints(
        HookEvent::SessionStart,
        &json!({ "source": "fork" }),
        None
    ));
    assert!(!resumes_checkpoints(
        HookEvent::SessionStart,
        &fresh,
        Some(guard)
    ));
    assert!(!resumes_checkpoints(HookEvent::SessionEnd, &fresh, None));
    assert!(!resumes_checkpoints(HookEvent::PostToolUse, &fresh, None));
}

#[test]
fn the_resume_step_runs_stale_jobs_through_the_starter_and_nothing_without_a_state_dir() {
    let dir = tempfile::tempdir().unwrap();
    let mut stale =
        checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({"a": 1}), None);
    stale.started_at = 1;
    let path = checkpoint::write(dir.path(), &stale).unwrap().unwrap();
    let mut started = Vec::new();
    let resumed = resume_checkpoints_with(Some(dir.path()), |c, p| {
        started.push((c.inputs.clone(), p.to_path_buf()));
        Ok(())
    });
    assert_eq!(resumed, 1);
    assert_eq!(started, vec![(json!({"a": 1}), path.clone())]);
    assert_eq!(checkpoint::load(&path).unwrap().attempts, 2);
    assert_eq!(resume_checkpoints_with(None, |_, _| Ok(())), 0);
}

#[test]
fn the_index_job_runs_through_the_wrapper_only_with_a_checkpoint_and_an_executable() {
    let direct = || std::process::Command::new("codebase-memory-mcp");
    let path = std::path::Path::new("/s/checkpoints/cbm-index-x.json");
    let args = |cmd: &std::process::Command| -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    let wrapped = index_job_command(Some(path), Some("/bin/llmenv".into()), direct);
    assert_eq!(wrapped.get_program(), "/bin/llmenv");
    assert_eq!(
        args(&wrapped),
        [
            "cbm-index-run",
            "--checkpoint",
            "/s/checkpoints/cbm-index-x.json"
        ]
    );
    assert_eq!(
        index_job_command(None, Some("/bin/llmenv".into()), direct).get_program(),
        "codebase-memory-mcp"
    );
    assert_eq!(
        index_job_command(Some(path), None, direct).get_program(),
        "codebase-memory-mcp"
    );
}

#[test]
fn the_resume_step_starts_a_stale_job_in_its_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut stale = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({}), None);
    stale.started_at = 1;
    stale.cwd = Some(work.path().display().to_string());
    let path = checkpoint::write(dir.path(), &stale).unwrap().unwrap();
    // The child is this test binary with arguments it rejects, so it exits at once.
    resume_checkpoints(Some(dir.path()));
    assert_eq!(checkpoint::load(&path).unwrap().attempts, 2);
}

#[test]
fn the_resume_step_skips_the_index_job_and_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut stale = checkpoint::Checkpoint::new(
        checkpoint::JobKind::CbmIndex,
        json!({"project_root": "/r", "index_path": null}),
        None,
    );
    stale.started_at = 1;
    let path = checkpoint::write(dir.path(), &stale).unwrap().unwrap();
    resume_checkpoints(Some(dir.path()));
    assert_eq!(
        checkpoint::load(&path).unwrap().attempts,
        1,
        "SessionStart starts the indexer itself, so a resume would only count an attempt"
    );
}

#[test]
fn the_dedup_snapshot_is_only_for_session_end_with_a_state_dir() {
    let dir = || Ok(std::path::PathBuf::from("/s"));
    let skipped = || anyhow::bail!("state dir gone");
    let never = || -> anyhow::Result<std::path::PathBuf> { anyhow::bail!("not for Stop") };
    let chunk = std::path::PathBuf::from("/s").join(crate::paths::HOOK_STORE_CHUNK);
    assert_eq!(dedup_snapshot_path(HookEvent::SessionEnd, dir), Some(chunk));
    assert_eq!(dedup_snapshot_path(HookEvent::SessionEnd, skipped), None);
    assert_eq!(dedup_snapshot_path(HookEvent::Stop, never), None);
}

#[test]
fn only_post_tool_use_stores_web_fetch() {
    assert!(stores_web_fetch(HookEvent::PostToolUse));
    assert!(!stores_web_fetch(HookEvent::PreToolUse));
}

#[test]
fn dedup_is_written_only_for_a_changed_chunk_with_a_path() {
    let path = std::path::Path::new("/s/dedup.txt");
    assert_eq!(dedup_write_target(Some(path), false), Some(path));
    assert_eq!(dedup_write_target(Some(path), true), None);
    assert_eq!(dedup_write_target(None, false), None);
}

#[test]
fn a_section_is_appended_on_its_own_line_and_an_empty_one_is_dropped() {
    let mut out = String::new();
    append_section(&mut out, "");
    assert_eq!(out, "");
    append_section(&mut out, "first");
    assert_eq!(out, "first");
    append_section(&mut out, "second");
    assert_eq!(out, "first\nsecond");
}

#[test]
fn only_a_context_losing_session_start_resets_read_state() {
    let with = |source: &str| serde_json::json!({ "source": source });
    assert!(resets_read_state(HookEvent::SessionStart, &with("compact")));
    assert!(resets_read_state(HookEvent::SessionStart, &with("clear")));
    assert!(!resets_read_state(HookEvent::SessionStart, &with("resume")));
    assert!(!resets_read_state(
        HookEvent::SessionStart,
        &serde_json::Value::Null
    ));
    assert!(!resets_read_state(HookEvent::PostToolUse, &with("compact")));
    assert!(!resets_read_state(HookEvent::PreToolUse, &with("clear")));
}

#[test]
fn consolidation_guard_needs_a_non_empty_value() {
    assert!(!is_consolidation_child(None));
    assert!(!is_consolidation_child(Some(std::ffi::OsStr::new(""))));
    assert!(is_consolidation_child(Some(std::ffi::OsStr::new("1"))));
}

#[test]
fn adaptive_applies_only_to_its_events_with_a_valid_session() {
    let on = Some(MemoryHookSettings {
        adaptive_recall: true,
        failure_recall: true,
        global_topics: vec![],
        ..MemoryHookSettings::default()
    });
    let off = Some(MemoryHookSettings {
        global_topics: vec![],
        ..MemoryHookSettings::default()
    });
    for event in [
        HookEvent::SessionStart,
        HookEvent::TurnStart,
        HookEvent::PostToolUseFailure,
        HookEvent::SubagentStart,
    ] {
        assert!(uses_adaptive(event, on.as_ref(), Some("s1")), "{event}");
        assert!(!uses_adaptive(event, off.as_ref(), Some("s1")), "{event}");
        assert!(!uses_adaptive(event, on.as_ref(), None), "{event}");
        assert!(
            !uses_adaptive(event, on.as_ref(), Some("../x")),
            "unsafe id: {event}"
        );
    }
    assert!(!uses_adaptive(
        HookEvent::SessionEnd,
        on.as_ref(),
        Some("s1")
    ));
}

#[test]
fn local_recording_events_skip_the_memory_pipeline() {
    assert!(records_locally(HookEvent::PostToolBatch));
    assert!(records_locally(HookEvent::SubagentTask));
    assert!(
        !records_locally(HookEvent::PreToolUse),
        "the general PreToolUse hook must not write the ledger"
    );
    assert!(!records_locally(HookEvent::TurnStart));
}

#[test]
fn scope_recall_actions_is_turn_start_without_the_final_recall() {
    let tags = tag_recall_queries(&["a".to_string()]).unwrap();
    let turn = dispatch(
        HookEvent::TurnStart,
        &tags,
        &[],
        &BTreeMap::new(),
        Some(&WakeUpArgs::default()),
    );
    let scope = scope_recall_actions(&tags, &[], &BTreeMap::new());
    assert_eq!(turn.last(), Some(&Action::Recall(String::new())));
    assert_eq!(scope, turn[..turn.len() - 1]);
}

#[test]
fn adaptive_recall_events_round_trip_through_their_names() {
    for (name, event) in [
        ("post_tool_batch", HookEvent::PostToolBatch),
        ("post_tool_use_failure", HookEvent::PostToolUseFailure),
        ("subagent_start", HookEvent::SubagentStart),
        ("subagent_task", HookEvent::SubagentTask),
        ("post_model_switch", HookEvent::PostModelSwitch),
    ] {
        assert_eq!(name.parse::<HookEvent>().unwrap(), event);
        assert_eq!(event.to_string(), name);
        assert!(
            dispatch(
                event,
                &[],
                &[],
                &BTreeMap::new(),
                Some(&WakeUpArgs::default())
            )
            .is_empty()
        );
        assert_eq!(event_to_log_kind(event), None);
    }
}

#[test]
fn all_hook_events_covers_every_variant() {
    for name in ALL_HOOK_EVENTS {
        let parsed = HookEvent::from_str(name).unwrap();
        assert_eq!(&parsed.to_string(), name, "{name} does not round-trip");
    }
    // `HookEvent` derives no variant count, so this guards the list
    // against a variant added to the enum but not to `from_str`.
    assert_eq!(ALL_HOOK_EVENTS.len(), 16);
}

// The gate is fed `AgentAdapter::name` (hyphenated), not `engine_id`
// (underscored). Driving it from the real registry means a mismatch shows
// up here instead of silently disabling the check.
#[test]
fn only_claude_code_session_start_triggers_the_drift_check() {
    let mut triggered = Vec::new();
    for adapter in crate::adapter::registered_adapters() {
        for name in ALL_HOOK_EVENTS {
            let event = HookEvent::from_str(name).unwrap();
            if event == HookEvent::SessionStart && adapter.name() == "claude-code" {
                triggered.push(format!("{}/{name}", adapter.name()));
            }
        }
    }
    assert_eq!(triggered, vec!["claude-code/session_start".to_string()]);
}

// #1331: opencode's shim blocks on `code === 2`, so `hook-run` denies with exit 2 there. Claude
// Code stays on exit 0 — it honours the envelope, and this keeps a working path unchanged.

// #231/#864: returning early when a sink still wants the event drops its
// log line, and both arms return the same text — so nothing but a direct
// test of the condition can tell the two apart.
#[test]
fn short_circuit_is_refused_while_a_sink_still_wants_the_event() {
    let quiet = crate::config::SessionLog {
        file: None,
        transcript: None,
        ..Default::default()
    };
    assert!(
        can_short_circuit(HookEvent::UserPromptSubmit, &quiet),
        "no sink wants it, so the decision can return immediately"
    );
    assert!(can_short_circuit(HookEvent::PreToolUse, &quiet));

    let listening = crate::config::SessionLog::default();
    assert!(
        listening.any_sink_enabled(),
        "fixture must have a sink on, or this proves nothing"
    );
    assert!(
        !can_short_circuit(HookEvent::UserPromptSubmit, &listening),
        "a listening sink must be reached before returning"
    );
}

// #317: each layer fires on exactly one event. Inside `run_inner` these
// sit behind payload reads and state-dir resolves, so a wrong event shows
// up as "the feature quietly does nothing" rather than a failure.
#[test]
fn each_slippage_layer_fires_on_exactly_one_event() {
    let matches = |f: fn(HookEvent) -> bool| {
        ALL_HOOK_EVENTS
            .iter()
            .filter(|name| f(HookEvent::from_str(name).unwrap()))
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(matches(counts_tool_use), vec!["post_tool_use".to_string()]);
    assert_eq!(
        matches(carries_turn_digest),
        vec!["user_prompt_submit".to_string()]
    );
    assert_eq!(
        matches(stores_session_metrics),
        vec!["session_end".to_string()]
    );
}

#[test]
fn only_opencode_needs_the_exit_code_block_signal() {
    let mut by_exit_code = Vec::new();
    for adapter in crate::adapter::registered_adapters() {
        if blocks_by_exit_code(adapter.name()) {
            by_exit_code.push(adapter.name().to_string());
        }
    }
    assert_eq!(by_exit_code, vec!["opencode".to_string()]);
}

#[test]
fn drift_check_gate_rejects_the_underscored_engine_id() {
    // `engine_id` is what `--engine` and config keys use; `name` is what
    // the gate actually receives. Asserting both directions keeps a future
    // refactor from swapping one for the other unnoticed.
    // SessionStart + "claude-code" triggers the check; other engines do not.
    assert!(HookEvent::SessionStart == HookEvent::SessionStart && "claude-code" == "claude-code");
    assert!(
        !(HookEvent::SessionStart == HookEvent::SessionStart && "claude_code" == "claude-code")
    );
}

// #1128: the marker used to require reaching the full success path (only
// 4 of 11 hook events ever got there); every early-return in run_inner
// now emits it too, with whichever phases it actually reached. Each
// field is present only when the corresponding Instant was reached.
#[test]
fn trace_timing_json_includes_only_reached_phases() {
    use std::collections::BTreeSet;
    let keys_of = |v: &serde_json::Value| -> BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    };

    let t0 = std::time::Instant::now();
    let t_config = t0 + std::time::Duration::from_micros(10);

    let only_config = hook_marker(HookTiming::new(t0, t_config));
    assert_eq!(
        keys_of(&only_config),
        BTreeSet::from([
            "v".to_string(),
            "op".to_string(),
            "config_load_us".to_string()
        ]),
        "an early return before scope eval must report v and config_load_us"
    );

    let t_scope = t_config + std::time::Duration::from_micros(20);
    let through_scope = hook_marker(HookTiming {
        t_scope: Some(t_scope),
        ..HookTiming::new(t0, t_config)
    });
    assert_eq!(
        keys_of(&through_scope),
        BTreeSet::from([
            "v".to_string(),
            "op".to_string(),
            "config_load_us".to_string(),
            "scope_eval_us".to_string(),
        ]),
        "an early return after scope eval must add scope_eval_us"
    );

    let t_chunk = t_scope + std::time::Duration::from_micros(30);
    let t_end = t_chunk + std::time::Duration::from_micros(40);
    let full = hook_marker(HookTiming {
        t_scope: Some(t_scope),
        t_chunk: Some(t_chunk),
        t_end: Some(t_end),
        ..HookTiming::new(t0, t_config)
    });
    assert_eq!(
        keys_of(&full),
        BTreeSet::from([
            "v".to_string(),
            "op".to_string(),
            "config_load_us".to_string(),
            "scope_eval_us".to_string(),
            "prep_us".to_string(),
            "mcp_us".to_string(),
        ]),
        "the full success path must report v and all four phases"
    );
    assert_eq!(full["v"], 1);
    assert_eq!(full["config_load_us"], 10);
    assert_eq!(full["scope_eval_us"], 20);
    assert_eq!(full["prep_us"], 30);
    assert_eq!(full["mcp_us"], 40);
}

proptest! {
    // #1128: trace_timing_json must never panic for any gap between
    // phases, including a zero gap (Instants captured in the same tick)
    // and a gap large enough to push the microsecond count past u64::MAX
    // (~585,000 years — saturating_duration_since + the try_from/
    // unwrap_or(u64::MAX) fallback must absorb it rather than panic).
    #[test]
    fn trace_timing_json_never_panics_for_arbitrary_gaps(
        config_gap_secs in 0u64..1_000_000_000_000,
        scope_gap_secs in 0u64..1_000_000_000_000,
        chunk_gap_secs in 0u64..1_000_000_000_000,
        end_gap_secs in 0u64..1_000_000_000_000,
    ) {
        let t0 = std::time::Instant::now();
        let t_config = t0 + std::time::Duration::from_secs(config_gap_secs);
        let t_scope = t_config + std::time::Duration::from_secs(scope_gap_secs);
        let t_chunk = t_scope + std::time::Duration::from_secs(chunk_gap_secs);
        let t_end = t_chunk + std::time::Duration::from_secs(end_gap_secs);

        let v = hook_marker(HookTiming {
        t_scope: Some(t_scope),
        t_chunk: Some(t_chunk),
        t_end: Some(t_end),
        ..HookTiming::new(t0, t_config)
    });
        prop_assert!(v.get("config_load_us").is_some());
        prop_assert!(v.get("scope_eval_us").is_some());
        prop_assert!(v.get("prep_us").is_some());
        prop_assert!(v.get("mcp_us").is_some());
    }
}

// #920: `memory_url` must use the disk-persisted merge cache when its key
// matches, instead of redoing the live merge. Proven behaviorally (not by
// spying on `merge()`): the bundle's `bundle.yaml` declares no memory/host
// at all, so a live merge would yield `all_memory = []`, `all_host = {}`,
// and `memory_url` would resolve to `Ok(None)` (no `still` entry to
// select). The persisted cache is seeded — under the exact key
// `memory_url` will independently recompute — with a `still` memory/host
// pair a live merge could never produce from this bundle. If `memory_url`
// returns that persisted URL, the disk-cache path was taken.
#[test]
fn memory_url_uses_persisted_cache_when_key_matches() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    // No `features`/`host` block — a live merge of this bundle contributes
    // no memory/host entries at all.
    std::fs::write(bundle_dir.join("bundle.yaml"), "permissions: {}\n").expect("test");

    let cache_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };

    let active = crate::scope::ActiveScopes {
        unknown_content: Default::default(),
        scopes: vec![],
        tags: std::collections::BTreeSet::from(["mytag".to_string(), "network-home".to_string()]),
        extra_tags: std::collections::BTreeSet::new(),
    };

    // Seed the persisted cache under the exact key `memory_url` will
    // independently recompute for this config/bundle set — derived via
    // `crate::bundle_select::build_bundle_refs`, the same ref-builder `memory_url`
    // itself calls, so this test can't drift from production behavior.
    let firing = crate::bundle_select::firing_bundles(&config.bundle, &active, None);
    let bundle_refs = crate::bundle_select::build_bundle_refs(config_root.path(), &active, &firing);
    let key = crate::merge::merge_signature(&config.capabilities, &config.native, &bundle_refs)
        .expect("test");
    let persisted_memory = vec![crate::config::Memory {
        always_load: None,
        server_host: "still".into(),
        port: 7878,
        listen_host: "127.0.0.1".into(),
        when: vec!["network-home".into()],
        default_topics: vec![],
        default_type: None,
        default_importance: None,
        type_importance: std::collections::BTreeMap::new(),
        retention: None,
        auto_prune: false,
        consolidation: None,
        mcp_permissions: None,
        wakeup_max_tokens: None,
        adaptive_recall: true,
        failure_recall: true,
        global_topics: crate::config::Memory::default().global_topics,
    }];
    let mut persisted_host = std::collections::BTreeMap::new();
    persisted_host.insert(
        "still".to_string(),
        crate::config::HostEntry {
            addr: "still.local".into(),
        },
    );
    crate::materialize::merge_cache::write(
        cache_dir.path(),
        &key,
        &persisted_memory,
        &persisted_host,
    )
    .expect("test");

    let url = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        url,
        crate::memory::MemoryEndpoint::Active {
            url: "http://still.local:7878/mcp".into(),
            settings: default_memory_hook(),
        },
        "memory_url must read the persisted merge cache instead of falling \
             back to a live merge of a bundle that declares no memory/host"
    );
}

// Static MCPs resolve ahead of the memory backend, so an unnamed match
// would hand the hooks another server's URL.
#[test]
fn memory_url_skips_other_remote_mcps() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        concat!(
            "features:\n",
            "  memory:\n",
            "    - server_host: still\n",
            "      port: 7878\n",
            "      when: [mytag]\n",
            "host:\n",
            "  still:\n",
            "    addr: still.local\n",
        ),
    )
    .expect("test");
    let cache_dir = tempfile::tempdir().expect("test");
    let other: crate::config::McpServer = serde_json::from_value(serde_json::json!({
        "name": "aaa-remote", "when": ["mytag"], "type": "http",
        "url": "http://other.invalid/mcp"
    }))
    .expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        mcp: vec![other],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        unknown_content: Default::default(),
        scopes: vec![],
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        extra_tags: std::collections::BTreeSet::new(),
    };
    let url = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        url,
        crate::memory::MemoryEndpoint::Active {
            url: "http://still.local:7878/mcp".into(),
            settings: default_memory_hook(),
        }
    );
}

// #920: a stale/mismatched persisted cache must never be trusted — a false
// hit here would silently resolve to the wrong ICM memory endpoint. This
// seeds a persisted cache under a key that does NOT match the live
// signature (bundle.yaml content differs from what was hashed) and
// confirms `memory_url` falls back to a correct live merge instead.
#[test]
fn memory_url_ignores_persisted_cache_on_key_mismatch() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        concat!(
            "features:\n",
            "  memory:\n",
            "    - server_host: still\n",
            "      port: 7878\n",
            "      when: [network-home]\n",
            "host:\n",
            "  still:\n",
            "    addr: still.local\n",
        ),
    )
    .expect("test");

    let cache_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        unknown_content: Default::default(),
        scopes: vec![],
        tags: std::collections::BTreeSet::from(["mytag".to_string(), "network-home".to_string()]),
        extra_tags: std::collections::BTreeSet::new(),
    };

    // Seed a cache entry under a deliberately wrong key, pointing at a
    // host a correct live merge would never resolve to.
    let mut bogus_host = std::collections::BTreeMap::new();
    bogus_host.insert(
        "still".to_string(),
        crate::config::HostEntry {
            addr: "bogus.invalid".into(),
        },
    );
    crate::materialize::merge_cache::write(cache_dir.path(), "wrong-key", &[], &bogus_host)
        .expect("test");

    let url = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        url,
        crate::memory::MemoryEndpoint::Active {
            url: "http://still.local:7878/mcp".into(),
            settings: default_memory_hook(),
        },
        "a key mismatch must fall back to a correct live merge, never the stale cache"
    );
}

#[test]
fn an_unavailable_state_folder_is_noticed_for_adaptive_recall() {
    let got = adaptive_state_dir(|| Err(anyhow::anyhow!("probe no HOME")));
    assert_eq!(got, None);

    let text = crate::agent_notice::current().unwrap_or_default();
    assert!(text.contains("adaptive recall"), "{text}");
    assert!(text.contains("probe no HOME"), "{text}");
}

/// Fixture for the two #1132 failure modes: a firing bundle whose
/// `bundle.yaml` is a *directory*, so every read of it fails with something
/// other than NotFound — the portable stand-in for the unreadable or
/// permission-denied bundle file whose error used to be swallowed.
fn unreadable_bundle_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    crate::config::Config,
    crate::scope::ActiveScopes,
) {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(bundle_dir.join("bundle.yaml")).expect("test");

    let cache_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        unknown_content: Default::default(),
        scopes: vec![],
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        extra_tags: std::collections::BTreeSet::new(),
    };
    (config_root, cache_dir, config, active)
}

// #1132: both of `resolve_bundle_memory_host`'s failure modes used to be
// discarded — the live merge via `unwrap_or_default()` (so a malformed or
// unreadable `bundle.yaml` collapsed to "no bundle memory" and the user was
// told `no memory backend active for this scope`, pointing them at their
// scope config, the one place the problem wasn't), and the cache-key
// computation via `.ok()` (so the #920 optimization silently never engaged,
// visible only as unexplained hook latency).
//
// Both are asserted in one test on purpose: `tracing` caches a callsite's
// interest globally on first hit, so a sibling test reaching the same
// `warn!` outside any subscriber would make a separate log-capture test
// order-dependent.
#[test]
fn memory_url_reports_both_bundle_merge_failure_modes() {
    let (config_root, _cache, config, active) = unreadable_bundle_fixture();
    let log_dir = tempfile::tempdir().expect("test");
    let log = log_dir.path().join("events.jsonl");
    // ERROR-only, matching main.rs's `EnvFilter::from_default_env()` with
    // `RUST_LOG` unset (#1139): a `warn!` here would prove nothing about
    // what an operator actually sees by default.
    let result = crate::session_log::tracing_layer::capture_file_logs_at(
        &log,
        tracing_subscriber::filter::LevelFilter::ERROR,
        || crate::memory::memory_url(&config, config_root.path(), &active),
    );

    let err =
        result.expect_err("a bundle merge failure must reach the caller, not be defaulted away");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("bundle 'b'"),
        "the error must name the bundle whose merge failed: {msg}"
    );

    let body = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        body.contains("merge signature"),
        "a signature failure must be logged at a level the default EnvFilter \
             passes, before falling back to a live merge: {body}"
    );
}

// #1139: `crate::memory::memory_url(...)?.into_url()` at the call site put `?` directly
// after `memory_url`, propagating *its* Err (a bundle-merge failure, #1132)
// out of the enclosing function before `.into_url()` ever ran — bypassing
// the fail-soft handling and aborting the whole hook event (session
// logging included) on a merge failure. `resolve_memory_client` returns
// `Option`, not `Result`, so it cannot repeat that mistake by construction.
#[test]
fn resolve_memory_client_does_not_propagate_a_bundle_merge_failure() {
    let (config_root, _cache, config, active) = unreadable_bundle_fixture();
    static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

    let client = resolve_memory_client(
        &config,
        config_root.path(),
        &active,
        MemoryCallContext {
            event: "test-event",
            state_dir: None,
            session_id: None,
        },
        &CACHE,
    );

    assert!(
        client.is_none(),
        "a bundle merge failure must degrade to no client, never propagate"
    );
}

/// A project scope that disables every bundle in `disable`.
fn disabling_project_scope(disable: &[&str]) -> crate::scope::ActiveScope {
    crate::scope::ActiveScope {
        id: "project".into(),
        kind: "project",
        tags: vec![],
        project_root: None,
        enable_bundles: vec![],
        disable_bundles: disable.iter().map(|s| (*s).to_string()).collect(),
        name: None,
        description: None,
        unknown_fields: vec![],
    }
}

// #1131: with no bundles and no `features.memory` at all, nothing could have
// supplied a backend — distinct from "a bundle would have, but is disabled".
#[test]
fn memory_url_reports_no_bundles_fired_when_nothing_is_configured() {
    let config_root = tempfile::tempdir().expect("test");
    let config = crate::config::Config::default();
    let active = crate::scope::ActiveScopes::default();

    assert_eq!(
        crate::memory::memory_url(&config, config_root.path(), &active).expect("test"),
        crate::memory::MemoryEndpoint::NoBundlesFired
    );
}

// #1216: a top-level `features.memory` entry's configured
// `wakeup_max_tokens` must survive resolution and be readable off the
// active endpoint, so hook_run's dispatch pipeline can pass it to
// icm_wake_up.
#[test]
fn memory_url_surfaces_configured_wakeup_max_tokens() {
    let config_root = tempfile::tempdir().expect("test");
    let mut host = std::collections::BTreeMap::new();
    host.insert(
        "still".to_string(),
        crate::config::HostEntry {
            addr: "still.local".into(),
        },
    );
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            memory: vec![crate::config::Memory {
                server_host: "still".into(),
                port: 7878,
                listen_host: "127.0.0.1".into(),
                when: vec!["network-home".into()],
                wakeup_max_tokens: Some(750),
                global_topics: vec![],
                ..crate::config::Memory::default()
            }],
            ..Default::default()
        }),
        host,
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        tags: std::collections::BTreeSet::from(["network-home".to_string()]),
        ..Default::default()
    };

    let endpoint = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        endpoint.settings().and_then(|s| s.wakeup_max_tokens),
        Some(750)
    );
}

// #1140: a top-level `features.memory` entry exists but its `when` isn't
// in the active scope. Before this fix, `resolve_mcps`'s `0 => {}` arm
// dropped it silently and this collapsed into `NoBundlesFired` — telling
// the user "config.yaml declares no features.memory" when it plainly
// does, just gated on a tag that isn't active right now.
#[test]
fn memory_url_reports_tag_inactive_when_declared_entry_is_ungated() {
    let config_root = tempfile::tempdir().expect("test");
    let mut host = std::collections::BTreeMap::new();
    host.insert(
        "still".to_string(),
        crate::config::HostEntry {
            addr: "still.local".into(),
        },
    );
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            memory: vec![crate::config::Memory {
                server_host: "still".into(),
                port: 7878,
                listen_host: "127.0.0.1".into(),
                when: vec!["network-home".into()],
                global_topics: vec![],
                ..crate::config::Memory::default()
            }],
            ..Default::default()
        }),
        host,
        ..Default::default()
    };
    // Active scope carries no tags at all — `network-home` isn't among them.
    let active = crate::scope::ActiveScopes::default();

    let resolved = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        resolved,
        crate::memory::MemoryEndpoint::TagInactive {
            server_hosts: vec!["still".to_string()]
        },
        "a declared-but-tag-inactive entry must not collapse into \
             NoBundlesFired, got {resolved:?}"
    );
    let msg = resolved
        .into_url()
        .expect_err("test")
        .to_string()
        .to_lowercase();
    assert!(
        msg.contains("still") && msg.contains("when"),
        "the message must name the server_host and the tag gating: {msg}"
    );
}

// #1131: bundles fired, they just declare no memory — the benign case, and
// the only one the old `Ok(None)` reading was ever right about.
#[test]
fn memory_url_reports_not_declared_when_firing_bundle_has_no_memory() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(bundle_dir.join("bundle.yaml"), "permissions: {}\n").expect("test");

    let cache_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        ..Default::default()
    };

    assert_eq!(
        crate::memory::memory_url(&config, config_root.path(), &active).expect("test"),
        crate::memory::MemoryEndpoint::NotDeclared {
            skipped_bundles: vec![]
        }
    );
}

// #1133: a firing bundle with no content directory is dropped by
// `build_bundle_refs` with a `warn!` the default `EnvFilter` discards — a
// third unlit road to "no memory backend". Named here instead, where it has
// a consequence the user can see.
#[test]
fn memory_url_names_firing_bundles_skipped_for_missing_content_dir() {
    let config_root = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "ghost".into(),
            when: vec!["mytag".into()],
        }],
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        ..Default::default()
    };

    assert_eq!(
        crate::memory::memory_url(&config, config_root.path(), &active).expect("test"),
        crate::memory::MemoryEndpoint::NotDeclared {
            skipped_bundles: vec!["ghost".to_string()]
        }
    );
}

// #1142: `build_bundle_refs` drops a firing bundle for two distinct
// reasons — no content directory, or an unsafe/traversal name — and the
// old message asserted the first reason unconditionally. The message must
// no longer claim a specific cause it can't actually attribute.
#[test]
fn memory_url_reports_notdeclared_message_covers_rejected_names_too() {
    let config_root = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "../evil".into(),
            when: vec!["mytag".into()],
        }],
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        ..Default::default()
    };

    let resolved = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        resolved,
        crate::memory::MemoryEndpoint::NotDeclared {
            skipped_bundles: vec!["../evil".to_string()]
        }
    );
    let msg = resolved
        .into_url()
        .expect_err("test")
        .to_string()
        .to_lowercase();
    assert!(
        msg.contains("rejected") && msg.contains("content directory"),
        "the message must not claim 'missing content directory' as the sole \
             cause when a rejected name is just as plausible: {msg}"
    );
}

// #1131: the disabled-bundle case must name the bundle that would have
// supplied the backend, so the resulting message can point at
// `disable_bundles` instead of the user's scope/tag config.
#[test]
fn memory_url_reports_disable_bundles_as_the_cause() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        concat!(
            "features:\n",
            "  memory:\n",
            "    - server_host: still\n",
            "      port: 7878\n",
            "      when: [network-home]\n",
            "host:\n",
            "  still:\n",
            "    addr: still.local\n",
        ),
    )
    .expect("test");

    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        scopes: vec![disabling_project_scope(&["b"])],
        tags: std::collections::BTreeSet::from(["mytag".to_string(), "network-home".to_string()]),
        ..Default::default()
    };

    let resolved = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        resolved,
        crate::memory::MemoryEndpoint::SuppressedByDisableBundles(vec!["b".to_string()]),
        "the sole source of features.memory being disabled must be reported \
             as such, not collapsed into `no memory backend active`"
    );
    let msg = resolved
        .into_url()
        .expect_err("test")
        .to_string()
        .to_lowercase();
    assert!(
        msg.contains("disable_bundles") && msg.contains('b'),
        "the message must name disable_bundles and the bundle: {msg}"
    );
}

// #1140/#1141 (pre-pr-review of #1234): a disabled bundle whose only
// `features.memory` entry is itself tag-inactive supplies nothing even if
// re-enabled, so it must not be named as the cause. Before this fix,
// `suppressed_memory_bundles` checked mere presence rather than
// tag-activity — the exact bug the sibling `declares_active_memory` fix
// (doctor.rs) exists to prevent, just reached through the shared helper.
#[test]
fn memory_url_ignores_disabled_bundle_whose_memory_entry_is_tag_inactive() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        concat!(
            "features:\n",
            "  memory:\n",
            "    - server_host: still\n",
            "      port: 7878\n",
            "      when: [othertag]\n",
            "host:\n",
            "  still:\n",
            "    addr: still.local\n",
        ),
    )
    .expect("test");

    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        scopes: vec![disabling_project_scope(&["b"])],
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        ..Default::default()
    };

    assert_eq!(
        crate::memory::memory_url(&config, config_root.path(), &active).expect("test"),
        crate::memory::MemoryEndpoint::NoBundlesFired,
        "a disabled bundle whose sole memory entry is gated on an inactive \
             tag would supply nothing even if re-enabled, so it must not be \
             reported as the (fixable) cause"
    );
}

// #1131: a top-level `features.memory` whose `server_host` lives only in a
// disabled bundle's `host:` table hard-errors with a host the user can see
// in their own config and nothing pointing at `disable_bundles`.
#[test]
fn memory_url_unknown_server_host_error_mentions_disable_bundles() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        "host:\n  still:\n    addr: still.local\n",
    )
    .expect("test");

    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        features: Some(crate::config::Features {
            memory: vec![crate::config::Memory {
                server_host: "still".into(),
                port: 7878,
                listen_host: "127.0.0.1".into(),
                when: vec!["mytag".into()],
                global_topics: vec![],
                ..crate::config::Memory::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        scopes: vec![disabling_project_scope(&["b"])],
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        ..Default::default()
    };

    let msg = format!(
        "{:#}",
        crate::memory::memory_url(&config, config_root.path(), &active).expect_err("test")
    );
    assert!(
        msg.contains("disable_bundles") && msg.contains("still"),
        "an unknown server_host supplied by a disabled bundle must say so: {msg}"
    );
}

// #1125: `memory_url` used to compute its firing-bundle set with an inline
// filter that checked tag-match and `enable_bundles` but never
// `disable_bundles`, so a bundle a project scope explicitly turns off still
// contributed its `features.memory`/`host` entries to ICM endpoint
// resolution — diverging from the materialized manifest, which excludes it.
#[test]
fn memory_url_ignores_bundle_disabled_by_project_scope() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        concat!(
            "features:\n",
            "  memory:\n",
            "    - server_host: still\n",
            "      port: 7878\n",
            "      when: [network-home]\n",
            "host:\n",
            "  still:\n",
            "    addr: still.local\n",
        ),
    )
    .expect("test");

    let cache_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };

    // The aggregated tag set fires the bundle; the project scope disables it.
    let active = crate::scope::ActiveScopes {
        scopes: vec![crate::scope::ActiveScope {
            id: "project".into(),
            kind: "project",
            tags: vec![],
            project_root: None,
            enable_bundles: vec![],
            disable_bundles: vec!["b".into()],
            name: None,
            description: None,
            unknown_fields: vec![],
        }],
        tags: std::collections::BTreeSet::from(["mytag".to_string(), "network-home".to_string()]),
        ..Default::default()
    };

    let url = crate::memory::memory_url(&config, config_root.path(), &active).expect("test");
    assert!(
        !matches!(url, crate::memory::MemoryEndpoint::Active { .. }),
        "a bundle disabled via `disable_bundles` must not contribute its \
             memory/host entries to memory_url resolution, got {url:?}"
    );
}

fn index_repository_clobber_payload() -> serde_json::Value {
    serde_json::json!({
        "tool_name": llmenv_mcp::resolve::INDEX_REPOSITORY_TOOL,
        "tool_input": { "repo_path": "/repo", "name": "some-other-project" },
    })
}

// #1331: `repeat_detect` matches every tool, so it can produce advisory
// text for this exact call. If the two were joined, the advisory would
// land ahead of `__DENY__:` and `run()`'s prefix check would miss it —
// the deny would silently become an allow.
#[test]
fn index_repository_clobber_deny_is_not_diluted_by_repeat_detect() {
    let state_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: true,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = index_repository_clobber_payload();
    // Twice, so repeat_detect has a prior call to match against.
    for _ in 0..2 {
        let text = resolve_pre_tool_decision(
            &payload,
            Some("clobber"),
            &config,
            false,
            Ok(state_dir.path().to_path_buf()),
        );
        let text = text.expect("the guard must decide this call");
        assert!(
            text.starts_with("__DENY__:"),
            "the deny must lead the string; got {} bytes",
            text.len()
        );
    }
}

// The guard reads only the call's arguments, so losing the state dir must
// not be able to turn a clobbering call into an allowed one.
#[test]
fn index_repository_clobber_deny_survives_a_state_dir_failure() {
    let text = resolve_pre_tool_decision(
        &index_repository_clobber_payload(),
        Some("clobber"),
        &crate::config::Config::default(),
        true,
        Err(anyhow::anyhow!("no state dir")),
    );
    assert!(
        text.is_some_and(|t| t.starts_with("__DENY__:")),
        "a state-dir failure must not disable the clobber guard"
    );
}

#[test]
fn index_repository_without_a_name_still_reaches_the_normal_pipeline() {
    let state_dir = tempfile::tempdir().expect("test");
    let text = resolve_pre_tool_decision(
        &serde_json::json!({
            "tool_name": llmenv_mcp::resolve::INDEX_REPOSITORY_TOOL,
            "tool_input": { "repo_path": "/repo" },
        }),
        Some("plain"),
        &crate::config::Config::default(),
        false,
        Ok(state_dir.path().to_path_buf()),
    );
    assert!(
        !text.is_some_and(|t| t.starts_with("__DENY__:")),
        "llmenv's own auto-index shape must not be denied"
    );
}

/// #1006: `resolve_pre_tool_text` must not let an enabled `read_once`
/// permanently mask `repeat_detect` for tool calls `read_once` doesn't
/// cover — regression test for the exact bug fp-check confirmed during
/// pre-pr-review (read_once's handler returning `""` for non-`Read`
/// tools used to still count as "a decision was made").
#[test]
fn repeat_detect_fires_even_when_read_once_is_also_enabled() {
    let session_id = "mask";
    let state_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            read_once: Some(crate::config::ReadOnce {
                enabled: true,
                mode: crate::config::ReadOnceMode::Warn,
                ttl_seconds: 1200,
            }),
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: true,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": "echo hi" },
    });
    let text = resolve_pre_tool_text(&payload, Some(session_id), &config, false, state_dir.path());
    assert!(
        text.is_some_and(|t| !t.is_empty()),
        "repeat_detect must still fire for a non-Read tool when read_once is also enabled"
    );
}

// #976: cd_guard is on by default and composes with repeat_detect, same
// as read_once does — both texts must survive, joined.
#[test]
fn cd_guard_fires_by_default_for_a_cd_command() {
    let state_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config::default();
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": "ls && cd /tmp" },
    });
    let text = resolve_pre_tool_text(&payload, Some("s1"), &config, false, state_dir.path());
    assert!(
        text.is_some_and(|t| t.contains("cd")),
        "cd_guard must be on by default"
    );
}

#[test]
fn cd_guard_disabled_via_config_stays_silent() {
    let state_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            cd_guard: Some(crate::config::CdGuard { enabled: false }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": "ls && cd /tmp" },
    });
    let text = resolve_pre_tool_text(&payload, Some("s1"), &config, false, state_dir.path());
    assert!(
        text.is_none(),
        "cd_guard disabled and nothing else configured must produce no advisory"
    );
}

#[test]
fn cd_guard_composes_with_repeat_detect_text() {
    let state_dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: true,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": "ls && cd /tmp" },
    });
    let text = resolve_pre_tool_text(&payload, Some("s2"), &config, false, state_dir.path())
        .expect("test");
    assert!(
        text.contains("cd"),
        "cd_guard text must be present; got {} bytes",
        text.len()
    );
    assert!(
        text.contains("identical input"),
        "repeat_detect text must also be present; got {} bytes",
        text.len()
    );
}

#[test]
fn read_once_still_wins_for_read_tool_over_repeat_detect() {
    // Both features want to say something about the 2nd identical Read
    // (read_once: "already read"; repeat_detect, threshold 1: fires on
    // every call). read_once is checked first, so its message must win.
    let session_id = "rw";
    let dir = tempfile::tempdir().expect("test");
    let file_path = dir.path().join("f.txt");
    std::fs::write(&file_path, "hello").expect("test");
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            read_once: Some(crate::config::ReadOnce {
                enabled: true,
                mode: crate::config::ReadOnceMode::Warn,
                ttl_seconds: 1200,
            }),
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: true,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let payload = serde_json::json!({
        "tool_name": "Read",
        "tool_input": { "file_path": file_path.to_str().unwrap() },
    });
    resolve_pre_tool_text(&payload, Some(session_id), &config, false, dir.path());
    let second = resolve_pre_tool_text(&payload, Some(session_id), &config, false, dir.path());
    assert!(
        second.is_some_and(|t| t.contains("already read")),
        "read_once's advisory must win over repeat_detect for a Read tool call"
    );
}

/// #1109: the task-tracker branch must honor the injected `state_dir`,
/// matching the isolation #1089 gave `read_once`/`repeat_detect`. Without
/// it this test writes a task into the developer's real `llmenv task`
/// tracker instead of the tempdir.
#[test]
fn task_tracker_redirect_writes_only_to_injected_state_dir() {
    let dir = tempfile::tempdir().expect("test");
    let config = crate::config::Config::default();
    let payload = serde_json::json!({
        "tool_name": "TaskCreate",
        "tool_input": { "subject": "isolated task" },
    });
    let text = resolve_pre_tool_text(&payload, Some("iso"), &config, true, dir.path())
        .expect("the task-tool redirect always decides");
    let tasks = crate::task::list_tasks(dir.path());
    assert_eq!(tasks.len(), 1, "task must land in the injected state_dir");
    assert_eq!(tasks[0].title, "isolated task");
    // Pins the success arm specifically: every failure arm also denies, so
    // the `__DENY__:` prefix alone wouldn't tell them apart.
    assert!(
        text.starts_with("__DENY__:") && text.contains(&tasks[0].slug),
        "deny must name the created task; got {} bytes",
        text.len()
    );
}

#[test]
fn index_repository_command_sets_args_and_no_scoping_env() {
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: None,
        mcp_permissions: None,
    };
    let cmd = index_command(
        std::path::Path::new("codebase-memory-mcp"),
        std::path::Path::new("/repos/proj"),
        cm.index_path.as_deref(),
        cm.mem_budget_mb,
    );
    assert_eq!(cmd.get_program().to_string_lossy(), "codebase-memory-mcp");
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert_eq!(args[0], "cli");
    assert_eq!(args[1], "index_repository");
    assert!(args[2].contains("/repos/proj"));
    let envs: std::collections::BTreeMap<String, String> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().to_string(),
                    v.to_string_lossy().to_string(),
                )
            })
        })
        .collect();
    assert!(
        !envs.contains_key("CBM_ALLOWED_ROOT"),
        "llmenv must not set CBM_ALLOWED_ROOT (#1495) — \
             codebase-memory-mcp applies its own default scoping"
    );
    assert!(
        !envs.contains_key("CBM_CACHE_DIR"),
        "llmenv must not set CBM_CACHE_DIR by default (#1493) — \
             codebase-memory-mcp falls back to its own default cache location"
    );
}

#[test]
fn index_repository_command_index_path_override_wins() {
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: Some("/custom/path".to_string()),
        mcp_permissions: None,
    };
    let cmd = index_command(
        std::path::Path::new("codebase-memory-mcp"),
        std::path::Path::new("/repos/proj"),
        cm.index_path.as_deref(),
        cm.mem_budget_mb,
    );
    let envs: std::collections::BTreeMap<String, String> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().to_string(),
                    v.to_string_lossy().to_string(),
                )
            })
        })
        .collect();
    assert_eq!(
        envs.get("CBM_CACHE_DIR").map(String::as_str),
        Some("/custom/path")
    );
}

#[test]
fn index_checkpoint_is_not_opened_when_the_executable_is_unknown() {
    let state = tempfile::tempdir().expect("test");
    let inputs = detached_cbm::IndexInputs {
        project_root: "/repos/proj".to_string(),
        index_path: None,
        mem_budget_mb: None,
        result_path: None,
    };
    // Without the executable no wrapper can settle the checkpoint, so none is written.
    assert_eq!(begin_index_checkpoint(state.path(), &inputs, false), None);
    assert_eq!(
        std::fs::read_dir(state.path()).expect("test").count(),
        0,
        "no checkpoint file may be written"
    );
}

#[test]
fn index_checkpoint_is_opened_when_the_executable_is_known() {
    let state = tempfile::tempdir().expect("test");
    let inputs = detached_cbm::IndexInputs {
        project_root: "/repos/proj".to_string(),
        index_path: None,
        mem_budget_mb: None,
        result_path: None,
    };
    let path = begin_index_checkpoint(state.path(), &inputs, true)
        .expect("a checkpoint opens when the executable is known");
    assert!(path.exists(), "the checkpoint file must be written");
}

#[test]
fn index_skipped_notice_names_the_skip_and_its_cause() {
    let notice = index_skipped_notice("cannot expand index_path: HOME is not set");
    assert!(
        notice.contains("index skipped"),
        "unexpected notice: {notice}"
    );
    assert!(
        notice.contains("HOME is not set"),
        "unexpected notice: {notice}"
    );
    assert!(notice.ends_with('\n'), "a notice line ends with a newline");
}

#[test]
fn expanded_index_path_removes_a_leading_tilde() {
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: Some("~/cbm".to_string()),
        mcp_permissions: None,
    };
    let expanded = expanded_index_path(&cm).ok().flatten();
    assert!(
        expanded
            .as_deref()
            .is_some_and(|p| !p.starts_with('~') && p.ends_with("/cbm")),
        "a literal tilde must not reach the indexer: {expanded:?}"
    );
}

#[test]
fn expanded_index_path_passes_an_absolute_path_and_none_through() {
    let mut cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: Some("/custom/path".to_string()),
        mcp_permissions: None,
    };
    assert_eq!(
        expanded_index_path(&cm).ok().flatten().as_deref(),
        Some("/custom/path")
    );
    cm.index_path = None;
    assert_eq!(expanded_index_path(&cm).unwrap(), None);
}

#[test]
fn codebase_memory_cache_dir_defaults_under_state_dir() {
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: None,
        mcp_permissions: None,
    };
    assert_eq!(
        codebase_memory_cache_dir(&cm, std::path::Path::new("/state")).ok(),
        Some(std::path::PathBuf::from("/state/codebase-memory"))
    );
}

#[test]
fn codebase_memory_cache_dir_honors_index_path_override() {
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: Some("/custom/path".to_string()),
        mcp_permissions: None,
    };
    assert_eq!(
        codebase_memory_cache_dir(&cm, std::path::Path::new("/state")).ok(),
        Some(std::path::PathBuf::from("/custom/path"))
    );
}

/// #1091: the indexer's stderr must land in a size-bounded, owner-only
/// log file rather than `/dev/null`, so a failing multi-minute index run
/// is diagnosable. Exercises the real `trigger_codebase_memory_index`
/// entry point — the `codebase-memory-mcp` binary need not actually be
/// installed: whether `spawn()` succeeds or fails soft, the log file
/// must already exist with the right properties, since it's opened
/// before the spawn is attempted.
#[cfg(unix)]
#[test]
fn the_index_command_and_the_mcp_server_agree_on_the_memory_budget() {
    let env_of = |cmd: &std::process::Command, key: &str| {
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    };
    let with = crate::config::CodebaseMemory {
        when: vec!["p".into()],
        mem_budget_mb: Some(8192),
        ..Default::default()
    };
    let cmd = index_command(
        std::path::Path::new("codebase-memory-mcp"),
        std::path::Path::new("/r"),
        with.index_path.as_deref(),
        with.mem_budget_mb,
    );
    assert_eq!(env_of(&cmd, "CBM_MEM_BUDGET_MB").as_deref(), Some("8192"));
    let without = crate::config::CodebaseMemory {
        when: vec!["p".into()],
        ..Default::default()
    };
    let cmd = index_command(
        std::path::Path::new("codebase-memory-mcp"),
        std::path::Path::new("/r"),
        without.index_path.as_deref(),
        without.mem_budget_mb,
    );
    assert_eq!(env_of(&cmd, "CBM_MEM_BUDGET_MB"), None);
}

// PRELOADED_CONFIG is a process-global cache; these two tests populate and
// clear it via `reset_preloaded_config_for_test`, so they must not run
// concurrently with each other (cargo test runs test fns in parallel by
// default) or they'd race on the shared state. This lock — test-only,
// unrelated to PRELOADED_CONFIG's own storage — serializes just these two.
static PRELOADED_CONFIG_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn load_cached_config_falls_back_to_disk_when_nothing_preloaded() {
    let _guard = PRELOADED_CONFIG_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reset_preloaded_config_for_test();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        serde_yaml::to_string(&crate::config::Config::default()).unwrap(),
    )
    .unwrap();
    let loaded = load_cached_config(&path).unwrap();
    assert_eq!(loaded, crate::config::Config::default());
}

#[test]
fn load_cached_config_returns_preloaded_config_when_set() {
    let _guard = PRELOADED_CONFIG_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reset_preloaded_config_for_test();

    let mut preloaded = crate::config::Config::default();
    preloaded.disabled_engines.push("preload-marker".into());
    set_preloaded_config(preloaded.clone());

    // The path on disk holds a *different* config, so a correct result
    // proves load_cached_config took the preload, not a fresh disk read.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        serde_yaml::to_string(&crate::config::Config::default()).unwrap(),
    )
    .unwrap();

    let loaded = load_cached_config(&path).unwrap();
    assert_eq!(loaded, preloaded);

    reset_preloaded_config_for_test();
}

proptest::proptest! {
    // #365: the repo_path JSON arg must always be valid JSON that
    // round-trips to the exact project_root string, for any path shape
    // (quotes, backslashes, unicode, whitespace) — directly validates
    // that this is safe against injection into the subprocess argv
    // (pre-pr-review finding).
    #[test]
    fn index_repository_command_json_arg_always_valid_and_roundtrips(
        path_str in "[\\PC]{0,60}"
    ) {
        let cm = crate::config::CodebaseMemory { allowed_roots: vec![], mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        let project_root = std::path::PathBuf::from(&path_str);
        let cmd = index_command(
            std::path::Path::new("codebase-memory-mcp"),
            &project_root,
            cm.index_path.as_deref(),
            cm.mem_budget_mb,
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let parsed: serde_json::Value = serde_json::from_str(&args[2])
            .expect("repo_path arg must always be valid JSON");
        let expected = project_root.display().to_string();
        proptest::prop_assert_eq!(parsed["repo_path"].as_str(), Some(expected.as_str()));
    }

    // #1493/#1495: index_command must never set
    // CBM_ALLOWED_ROOT, and must never set CBM_CACHE_DIR when
    // index_path is unset — mirrors resolve_codebase_memory's contract
    // (src/mcp/resolve.rs), for arbitrary project_root content.
    #[test]
    fn index_repository_command_no_scoping_env_when_index_path_none(
        path_str in "[\\PC]{0,60}"
    ) {
        let cm = crate::config::CodebaseMemory { allowed_roots: vec![], mem_budget_mb: None,
            when: vec!["proj".to_string()],
            index_path: None,
            mcp_permissions: None,
        };
        let project_root = std::path::PathBuf::from(&path_str);
        let cmd = index_command(
            std::path::Path::new("codebase-memory-mcp"),
            &project_root,
            cm.index_path.as_deref(),
            cm.mem_budget_mb,
        );
        let envs: std::collections::BTreeMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().to_string(),
                        v.to_string_lossy().to_string(),
                    )
                })
            })
            .collect();
        proptest::prop_assert!(!envs.contains_key("CBM_ALLOWED_ROOT"));
        proptest::prop_assert!(!envs.contains_key("CBM_CACHE_DIR"));
    }
}

#[test]
fn append_read_once_result_appends_advisory_to_empty_out() {
    let mut out = String::new();
    append_read_once_result(&mut out, "advisory text");
    assert_eq!(out, "advisory text");
}

#[test]
fn append_read_once_result_appends_deny_to_empty_out() {
    let mut out = String::new();
    append_read_once_result(&mut out, "__DENY__:already read");
    assert_eq!(out, "__DENY__:already read");
}

#[test]
fn append_read_once_result_appends_advisory_to_nonempty_out() {
    let mut out = String::from("existing content");
    append_read_once_result(&mut out, "advisory text");
    assert_eq!(out, "existing content\nadvisory text");
}

// #868: `out` being non-empty before a deny append means dispatch()
// started producing PreToolUse actions, which run()'s positional
// __DENY__: prefix check can't safely combine with — the deny must
// replace `out` entirely (in release builds too) rather than silently
// downgrading to an allow.
#[test]
fn append_read_once_result_deny_replaces_nonempty_out() {
    let mut out = String::from("existing content");
    append_read_once_result(&mut out, "__DENY__:already read");
    assert_eq!(out, "__DENY__:already read");
}

#[test]
fn parses_neutral_event_names() {
    assert_eq!(
        "session_start".parse::<HookEvent>().unwrap(),
        HookEvent::SessionStart
    );
    assert_eq!(
        "turn_start".parse::<HookEvent>().unwrap(),
        HookEvent::TurnStart
    );
    assert_eq!(
        "session_end".parse::<HookEvent>().unwrap(),
        HookEvent::SessionEnd
    );
    assert_eq!(
        "post_session".parse::<HookEvent>().unwrap(),
        HookEvent::PostSession
    );
}

#[test]
fn rejects_unknown_event() {
    assert!("nope".parse::<HookEvent>().is_err());
}

#[test]
fn parses_verbose_event_names() {
    assert_eq!(
        "user_prompt_submit".parse::<HookEvent>().unwrap(),
        HookEvent::UserPromptSubmit
    );
    assert_eq!(
        "pre_tool_use".parse::<HookEvent>().unwrap(),
        HookEvent::PreToolUse
    );
    assert_eq!(
        "post_tool_use".parse::<HookEvent>().unwrap(),
        HookEvent::PostToolUse
    );
    assert_eq!(
        "notification".parse::<HookEvent>().unwrap(),
        HookEvent::Notification
    );
    assert_eq!("stop".parse::<HookEvent>().unwrap(), HookEvent::Stop);
    assert_eq!(
        "subagent_stop".parse::<HookEvent>().unwrap(),
        HookEvent::SubagentStop
    );
    assert_eq!(
        "pre_compact".parse::<HookEvent>().unwrap(),
        HookEvent::PreCompact
    );
}

#[test]
fn verbose_event_display_round_trips_through_from_str() {
    for ev in [
        HookEvent::SessionStart,
        HookEvent::TurnStart,
        HookEvent::SessionEnd,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Notification,
        HookEvent::Stop,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
    ] {
        assert_eq!(ev.to_string().parse::<HookEvent>().unwrap(), ev);
    }
}

#[test]
fn verbose_events_map_to_log_kinds() {
    assert_eq!(
        event_to_log_kind(HookEvent::UserPromptSubmit).unwrap(),
        (EventKind::Prompt, "user")
    );
    assert_eq!(
        event_to_log_kind(HookEvent::PreToolUse).unwrap(),
        (EventKind::ToolUse, "tool")
    );
    assert_eq!(
        event_to_log_kind(HookEvent::PostToolUse).unwrap(),
        (EventKind::ToolResult, "tool")
    );
    assert_eq!(
        event_to_log_kind(HookEvent::Notification).unwrap(),
        (EventKind::Notification, "system")
    );
    assert_eq!(
        event_to_log_kind(HookEvent::Stop).unwrap(),
        (EventKind::Stop, "assistant")
    );
    assert_eq!(
        event_to_log_kind(HookEvent::SubagentStop).unwrap(),
        (EventKind::Stop, "assistant")
    );
    assert_eq!(
        event_to_log_kind(HookEvent::PreCompact).unwrap(),
        (EventKind::Notification, "system")
    );
}

#[test]
fn lifecycle_and_memory_events_have_no_log_kind() {
    assert_eq!(event_to_log_kind(HookEvent::SessionStart), None);
    assert_eq!(event_to_log_kind(HookEvent::TurnStart), None);
    assert_eq!(event_to_log_kind(HookEvent::SessionEnd), None);
}

#[test]
fn dispatch_emits_no_memory_actions_for_verbose_events() {
    for ev in [
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Notification,
        HookEvent::Stop,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
    ] {
        assert_eq!(
            dispatch(ev, &[], &[], &BTreeMap::new(), Some(&WakeUpArgs::default())),
            Vec::<Action>::new()
        );
    }
}

#[test]
fn verbose_content_extracts_prompt_text() {
    let payload = serde_json::json!({"prompt": "fix the bug"});
    let (tool_name, content) = event_content(HookEvent::UserPromptSubmit, &payload);
    assert_eq!(tool_name, None);
    assert_eq!(content, "fix the bug");
}

#[test]
fn verbose_content_extracts_pre_tool_use_name_and_input() {
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
    });
    let (tool_name, content) = event_content(HookEvent::PreToolUse, &payload);
    assert_eq!(tool_name.as_deref(), Some("Bash"));
    assert!(content.contains("\"command\":\"ls\""));
}

#[test]
fn verbose_content_extracts_post_tool_use_response() {
    let payload = serde_json::json!({
        "tool_name": "Write",
        "tool_input": {"file_path": "/tmp/x"},
        "tool_response": {"filePath": "/tmp/x"},
    });
    let (tool_name, content) = event_content(HookEvent::PostToolUse, &payload);
    assert_eq!(tool_name.as_deref(), Some("Write"));
    assert!(content.contains("filePath"));
}

#[test]
fn verbose_content_extracts_notification_message() {
    let payload = serde_json::json!({"message": "needs your attention"});
    let (_, content) = event_content(HookEvent::Notification, &payload);
    assert_eq!(content, "needs your attention");
}

#[test]
fn verbose_content_extracts_stop_last_assistant_message() {
    let payload = serde_json::json!({"last_assistant_message": "done"});
    let (_, content) = event_content(HookEvent::Stop, &payload);
    assert_eq!(content, "done");
    let (_, content) = event_content(HookEvent::SubagentStop, &payload);
    assert_eq!(content, "done");
}

#[test]
fn verbose_content_extracts_pre_compact_trigger() {
    let payload = serde_json::json!({"trigger": "manual", "custom_instructions": ""});
    let (_, content) = event_content(HookEvent::PreCompact, &payload);
    assert_eq!(content, "manual");
}

#[test]
fn verbose_content_is_empty_for_missing_fields() {
    let (tool_name, content) = event_content(HookEvent::UserPromptSubmit, &serde_json::Value::Null);
    assert_eq!(tool_name, None);
    assert_eq!(content, "");
}

#[test]
fn dispatch_maps_events_to_actions() {
    assert_eq!(
        dispatch(
            HookEvent::SessionStart,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&WakeUpArgs::default())
        ),
        vec![Action::WakeUp(WakeUpArgs::default())]
    );
    assert_eq!(
        dispatch(
            HookEvent::TurnStart,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&WakeUpArgs::default())
        ),
        vec![Action::Recall(String::new())]
    );
    assert_eq!(
        dispatch(
            HookEvent::SessionEnd,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&WakeUpArgs::default())
        ),
        vec![Action::Store]
    );
    assert_eq!(
        dispatch(
            HookEvent::PostSession,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&WakeUpArgs::default())
        ),
        vec![],
        "PostSession defers to consolidation module, no dispatch actions"
    );
}

// #2142: a continued session (resume, fork) gets no wake-up call.
#[test]
fn dispatch_skips_the_wake_up_without_wake_args() {
    assert_eq!(
        dispatch(HookEvent::SessionStart, &[], &[], &BTreeMap::new(), None),
        vec![]
    );
}

#[test]
fn continues_session_only_for_resume_and_fork() {
    for (payload, want) in [
        (json!({"source": "resume"}), true),
        (json!({"source": "fork"}), true),
        (json!({"source": "startup"}), false),
        (json!({"source": "clear"}), false),
        (json!({"source": "compact"}), false),
        (json!({"source": 7}), false),
        (json!({}), false),
    ] {
        assert_eq!(continues_session(&payload), want, "{payload}");
    }
}

#[test]
fn dispatch_threads_wakeup_max_tokens_into_session_start_only() {
    let wake_750 = WakeUpArgs {
        max_tokens: Some(750),
        project: Some("llmenv".into()),
    };
    assert_eq!(
        dispatch(
            HookEvent::SessionStart,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&wake_750)
        ),
        vec![Action::WakeUp(wake_750.clone())]
    );
    // WakeUp only fires on SessionStart; TurnStart takes just the project (#2253).
    assert_eq!(
        dispatch(
            HookEvent::TurnStart,
            &[],
            &[],
            &BTreeMap::new(),
            Some(&wake_750)
        ),
        vec![Action::Recall("llmenv".into())]
    );
}

#[test]
fn chunk_unchanged_true_when_prev_matches_current() {
    assert!(chunk_unchanged(Some("same"), "same"));
}

#[test]
fn chunk_unchanged_false_when_prev_differs_from_current() {
    assert!(!chunk_unchanged(Some("old"), "new"));
}

#[test]
fn chunk_unchanged_false_when_no_prior_snapshot() {
    // No snapshot yet (first SessionEnd, or a read failure) is always
    // "changed" — never skip the very first store.
    assert!(!chunk_unchanged(None, "anything"));
}

#[test]
fn store_content_for_event_session_end_uses_storage_chunk() {
    assert_eq!(
        store_content_for_event(HookEvent::SessionEnd, "full", "minimal"),
        "minimal"
    );
}

#[test]
fn store_content_for_event_other_events_use_full_chunk() {
    assert_eq!(
        store_content_for_event(HookEvent::TurnStart, "full", "minimal"),
        "full"
    );
    assert_eq!(
        store_content_for_event(HookEvent::SessionStart, "full", "minimal"),
        "full"
    );
}

#[tokio::test]
async fn run_memory_actions_joins_deduplicated_recall_records() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "content": [{ "type": "text", "text": crate::hook_run::test_recall::recall_json("[t] one\n[t] two\n[t] one") }] }
    });
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("URL");

    let text = run_memory_actions(
        &client,
        vec![Action::Recall(String::new()), Action::Recall(String::new())],
        "q",
        "chunk",
    )
    .await;

    assert_eq!(text.text(), "[t] one\n\n[t] two");
}

#[test]
fn turn_start_expands_one_recall_tag_per_active_tag() {
    let tags = vec!["rust".to_string(), "work-vpn".to_string()];
    let queries = tag_recall_queries(&tags).expect("valid tags");
    let actions = dispatch(
        HookEvent::TurnStart,
        &queries,
        &[],
        &BTreeMap::new(),
        Some(&WakeUpArgs::default()),
    );
    assert_eq!(
        actions,
        vec![
            Action::RecallTag(TagRecallQuery {
                tag: "rust".to_string(),
                keyword: "llmenv-tag:rust".to_string(),
            }),
            Action::RecallTag(TagRecallQuery {
                tag: "work-vpn".to_string(),
                keyword: "llmenv-tag:work-vpn".to_string(),
            }),
            Action::Recall(String::new()),
        ],
        "TurnStart must run one tag recall per active tag, then the project recall"
    );
}

#[test]
fn turn_start_expands_one_recall_bundle_per_active_bundle() {
    let bundles = vec!["base".to_string(), "rust-defaults".to_string()];
    let queries = bundle_recall_queries(&bundles).expect("valid bundles");
    let actions = dispatch(
        HookEvent::TurnStart,
        &[],
        &queries,
        &BTreeMap::new(),
        Some(&WakeUpArgs::default()),
    );
    assert_eq!(
        actions,
        vec![
            Action::RecallBundle(BundleRecallQuery {
                bundle: "base".to_string(),
                keyword: "llmenv-bundle:base".to_string(),
            }),
            Action::RecallBundle(BundleRecallQuery {
                bundle: "rust-defaults".to_string(),
                keyword: "llmenv-bundle:rust-defaults".to_string(),
            }),
            Action::Recall(String::new()),
        ],
        "TurnStart must emit one bundle recall per active bundle"
    );
}

#[test]
fn turn_start_interleaves_tag_and_bundle_recalls() {
    let tag_qs = tag_recall_queries(&["rust".to_string()]).expect("valid");
    let bundle_qs = bundle_recall_queries(&["base".to_string()]).expect("valid");
    let ranks = BTreeMap::from([("rust".to_string(), 1)]);
    let actions = dispatch(
        HookEvent::TurnStart,
        &tag_qs,
        &bundle_qs,
        &ranks,
        Some(&WakeUpArgs::default()),
    );
    // Order: rank-1 tag recalls, then bundle recalls, then project recall.
    assert!(matches!(actions[0], Action::RecallTag(_)));
    assert!(matches!(actions[1], Action::RecallBundle(_)));
    assert_eq!(actions[2], Action::Recall(String::new()));
    assert_eq!(actions.len(), 3);
}

#[test]
fn validate_tag_accepts_valid_tags() {
    assert!(validate_tag("base").is_ok());
    assert!(validate_tag("rust-lang").is_ok());
    assert!(validate_tag("work_project").is_ok());
    assert!(validate_tag("tag123").is_ok());
    assert!(validate_tag("my-tag_123").is_ok());
}

#[test]
fn validate_tag_rejects_empty() {
    assert!(validate_tag("").is_err());
}

#[test]
fn validate_tag_rejects_special_characters() {
    assert!(validate_tag("tag:space").is_err());
    assert!(validate_tag("tag space").is_err());
    assert!(validate_tag("tag/path").is_err());
    assert!(validate_tag("tag.dot").is_err());
    assert!(validate_tag("tag@at").is_err());
    assert!(validate_tag("tag#hash").is_err());
    assert!(validate_tag("tag$dollar").is_err());
    assert!(validate_tag("tag\"quote").is_err());
}

#[test]
fn validate_tag_rejects_query_injection_attempts() {
    // Attempts to inject ICM query syntax
    assert!(validate_tag("tag,malicious").is_err());
    assert!(validate_tag("tag OR other").is_err());
    assert!(validate_tag("tag AND other").is_err());
}

#[test]
fn dispatch_tag_and_bundle_with_same_name_produce_distinct_recalls() {
    // A name valid as both a tag and a bundle must produce two separate
    // recalls keyed on different prefixes — no cross-contamination.
    let tag_qs = tag_recall_queries(&["foo".to_string()]).expect("valid");
    let bundle_qs = bundle_recall_queries(&["foo".to_string()]).expect("valid");
    let ranks = BTreeMap::from([("foo".to_string(), 1)]);
    let actions = dispatch(
        HookEvent::TurnStart,
        &tag_qs,
        &bundle_qs,
        &ranks,
        Some(&WakeUpArgs::default()),
    );
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        Action::RecallTag(q) => assert_eq!(q.keyword, "llmenv-tag:foo"),
        other => panic!("expected RecallTag, got {other:?}"),
    }
    match &actions[1] {
        Action::RecallBundle(q) => assert_eq!(q.keyword, "llmenv-bundle:foo"),
        other => panic!("expected RecallBundle, got {other:?}"),
    }
}

#[test]
fn bundle_recall_queries_validates_bundle_names() {
    assert!(bundle_recall_queries(&["".to_string()]).is_err());
    assert!(bundle_recall_queries(&["bundle:invalid".to_string()]).is_err());
    assert!(bundle_recall_queries(&["bundle space".to_string()]).is_err());
    assert!(bundle_recall_queries(&["bundle/path".to_string()]).is_err());
}

#[test]
fn validate_bundle_rejects_empty() {
    assert!(validate_bundle("").is_err());
}

#[test]
fn validate_bundle_rejects_special_characters() {
    assert!(validate_bundle("bundle:invalid").is_err());
    assert!(validate_bundle("bundle space").is_err());
    assert!(validate_bundle("bundle/path").is_err());
    assert!(validate_bundle("bundle.dot").is_err());
}

#[test]
fn validate_bundle_rejects_query_injection_attempts() {
    assert!(validate_bundle("bundle,malicious").is_err());
    assert!(validate_bundle("bundle OR other").is_err());
    assert!(validate_bundle("bundle AND other").is_err());
}

use proptest::prelude::*;

fn valid_name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,24}"
}

fn arb_hook_event() -> impl Strategy<Value = HookEvent> {
    prop_oneof![
        Just(HookEvent::SessionStart),
        Just(HookEvent::TurnStart),
        Just(HookEvent::SessionEnd),
        Just(HookEvent::UserPromptSubmit),
        Just(HookEvent::PreToolUse),
        Just(HookEvent::PostToolUse),
        Just(HookEvent::Notification),
        Just(HookEvent::Stop),
        Just(HookEvent::SubagentStop),
        Just(HookEvent::PreCompact),
    ]
}

/// Arbitrary Claude hook stdin payload shapes: present-and-string,
/// present-and-wrong-type, and absent, for each field `event_content`
/// reads. Exercises the adversarial/malformed-payload path (#509 item 5).
fn arb_verbose_payload() -> impl Strategy<Value = serde_json::Value> {
    let field = |key: &'static str| {
        prop_oneof![
            "[a-zA-Z0-9 _-]{0,16}".prop_map(move |s| (key, serde_json::json!(s))),
            Just((key, serde_json::json!(42))),
            Just((key, serde_json::json!({"nested": "object"}))),
            Just((key, serde_json::Value::Null)),
        ]
    };
    prop::collection::vec(
        prop_oneof![
            field("prompt"),
            field("tool_name"),
            field("tool_input"),
            field("tool_response"),
            field("message"),
            field("last_assistant_message"),
            field("trigger"),
        ],
        0..7,
    )
    .prop_map(|pairs| {
        serde_json::Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    })
}

proptest! {
    #[test]
    fn prop_verbose_event_display_round_trips_through_from_str(ev in arb_hook_event()) {
        prop_assert_eq!(ev.to_string().parse::<HookEvent>().unwrap(), ev);
    }

    #[test]
    fn prop_verbose_content_never_panics(
        ev in arb_hook_event(),
        payload in arb_verbose_payload(),
    ) {
        let _ = event_content(ev, &payload);
    }
}

proptest! {
    // With no ranks, dispatch(TurnStart) always produces
    // [M×RecallBundle, N×RecallTag, Recall] regardless of N and M.
    #[test]
    fn prop_dispatch_turn_start_ordering(
        tags in proptest::collection::vec(valid_name(), 0..8),
        bundles in proptest::collection::vec(valid_name(), 0..8),
    ) {
        let tag_qs = tag_recall_queries(&tags).expect("valid tags");
        let bundle_qs = bundle_recall_queries(&bundles).expect("valid bundles");
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs, &BTreeMap::new(), Some(&WakeUpArgs::default()));

        prop_assert_eq!(actions.len(), 1 + tags.len() + bundles.len());
        for a in &actions[..bundles.len()] {
            prop_assert!(
                matches!(a, Action::RecallBundle(_)),
                "expected RecallBundle, got {a:?}"
            );
        }
        for a in &actions[bundles.len()..bundles.len() + tags.len()] {
            prop_assert!(matches!(a, Action::RecallTag(_)), "expected RecallTag, got {a:?}");
        }
        prop_assert!(matches!(actions.last(), Some(Action::Recall(_))));
    }
}

#[test]
fn turn_start_orders_tags_by_specificity_with_bundles_after_rank_one() {
    let tags: Vec<String> = [
        "aaa-host",
        "bbb-project",
        "ccc-user",
        "ddd-project",
        "eee-loose",
    ]
    .map(String::from)
    .to_vec();
    let ranks = BTreeMap::from([
        ("aaa-host".to_string(), 5),
        ("bbb-project".to_string(), 1),
        ("ccc-user".to_string(), 4),
        ("ddd-project".to_string(), 1),
    ]);
    let tag_qs = tag_recall_queries(&tags).expect("valid tags");
    let bundle_qs = bundle_recall_queries(&["base".to_string()]).expect("valid bundle");
    let order: Vec<String> = dispatch(
        HookEvent::TurnStart,
        &tag_qs,
        &bundle_qs,
        &ranks,
        Some(&WakeUpArgs::default()),
    )
    .iter()
    .map(|a| match a {
        Action::RecallTag(q) => q.tag.clone(),
        Action::RecallBundle(q) => format!("bundle:{}", q.bundle),
        other => format!("{other:?}"),
    })
    .collect();
    assert_eq!(
        order,
        [
            "bbb-project",
            "ddd-project",
            "bundle:base",
            "ccc-user",
            "aaa-host",
            "eee-loose",
            "Recall(\"\")",
        ]
    );
}

// ===== #1143: crate::memory::MemoryEndpoint::into_url() message formatting =====

/// Every `into_url()` variant that carries a name list uses `.join(", ")`
/// unconditionally — shared assertion so the three variant-specific
/// proptests below don't each restate it (pre-pr-review finding for
/// #1234).
fn assert_message_preserves_names(
    msg: &str,
    names: &[String],
) -> Result<(), proptest::test_runner::TestCaseError> {
    for name in names {
        if !msg.contains(name.as_str()) {
            return Err(proptest::test_runner::TestCaseError::fail(format!(
                "message must preserve {name:?}: {msg}"
            )));
        }
    }
    if !names.is_empty() && !msg.contains(&names.join(", ")) {
        return Err(proptest::test_runner::TestCaseError::fail(format!(
            "names must appear joined with \", \": {msg}"
        )));
    }
    Ok(())
}

proptest! {
    // NotDeclared's message must preserve every skipped-bundle name and
    // join them with the same separator regardless of list length or the
    // exact (valid) characters in each name.
    #[test]
    fn prop_notdeclared_message_preserves_every_skipped_bundle_name(
        names in proptest::collection::vec(valid_name(), 0..8),
    ) {
        let msg = crate::memory::MemoryEndpoint::NotDeclared { skipped_bundles: names.clone() }
            .into_url()
            .unwrap_err()
            .to_string();
        assert_message_preserves_names(&msg, &names)?;
    }

    #[test]
    fn prop_suppressed_message_preserves_every_bundle_name(
        names in proptest::collection::vec(valid_name(), 1..8),
    ) {
        let msg = crate::memory::MemoryEndpoint::SuppressedByDisableBundles(names.clone())
            .into_url()
            .unwrap_err()
            .to_string();
        assert_message_preserves_names(&msg, &names)?;
    }

    #[test]
    fn prop_tag_inactive_message_preserves_every_server_host(
        hosts in proptest::collection::vec(valid_name(), 1..8),
    ) {
        let msg = crate::memory::MemoryEndpoint::TagInactive { server_hosts: hosts.clone() }
            .into_url()
            .unwrap_err()
            .to_string();
        assert_message_preserves_names(&msg, &hosts)?;
    }
}

// ===== #592: apply_memory_config_defaults idempotence =====

fn memory_settings(default_type: Option<llmenv_config::MemoryType>) -> MemoryHookSettings {
    MemoryHookSettings {
        adaptive_recall: true,
        failure_recall: true,
        default_type,
        global_topics: vec![],
        ..MemoryHookSettings::default()
    }
}

#[test]
fn apply_memory_defaults_idempotent_no_type() {
    let settings = memory_settings(None);
    let input = "## context\nno markers".to_string();
    let once = apply_memory_config_defaults(input, Some(&settings));
    let twice = apply_memory_config_defaults(once.clone(), Some(&settings));
    assert_eq!(once, twice, "applying defaults twice must be idempotent");
}

#[test]
fn apply_memory_defaults_adds_type_marker_when_present() {
    let settings = memory_settings(Some(llmenv_config::MemoryType::Semantic));
    let input = "## context".to_string();
    let out = apply_memory_config_defaults(input, Some(&settings));
    assert!(
        out.contains("<!-- llmenv-type: semantic -->"),
        "should add semantic type marker: {out}"
    );
}

#[test]
fn apply_memory_defaults_adds_importance_marker_once() {
    let settings = MemoryHookSettings {
        default_importance: Some(llmenv_config::ImportanceLevel::High),
        ..memory_settings(None)
    };
    let out = apply_memory_config_defaults("## context".to_string(), Some(&settings));
    assert!(out.contains("<!-- llmenv-importance: high -->"), "{out}");
    let kept = apply_memory_config_defaults(
        "## context\n<!-- llmenv-importance: low -->".to_string(),
        Some(&settings),
    );
    assert!(!kept.contains("high"), "an existing marker wins: {kept}");
    assert!(apply_memory_config_defaults("x".to_string(), None) == "x");
}

#[test]
fn apply_memory_defaults_skips_existing_marker() {
    let settings = memory_settings(Some(llmenv_config::MemoryType::Semantic));
    let input = "## context\n<!-- llmenv-type: episodic -->".to_string();
    let out = apply_memory_config_defaults(input, Some(&settings));
    assert!(
        !out.contains("semantic"),
        "must not override existing episodic marker"
    );
    assert!(
        out.contains("episodic"),
        "existing marker must survive: {out}"
    );
}

#[test]
fn handle_web_fetch_post_tool_use_does_not_block() {
    let payload = serde_json::json!({
        "tool_name": "WebFetch",
        "tool_input": {"url": "https://example.com"},
        "tool_response": "fetched content",
    });
    // The child process (re-invoking the current, test-harness executable
    // with args it doesn't understand) is expected to exit non-zero almost
    // instantly; the parent never waits on it, so this call itself must
    // return promptly.
    let start = std::time::Instant::now();
    let child = handle_web_fetch_post_tool_use(&payload);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "handle_web_fetch_post_tool_use must not block on the child"
    );
    // A well-formed WebFetch payload must actually spawn a child —
    // this is the one assertion that would catch the whole function
    // being replaced with an unconditional `None` (#1465).
    let mut child = child.expect("a well-formed WebFetch payload must spawn a detached child");
    // Reap: production deliberately never waits (the child is
    // process-group-detached), but this test process is still its OS
    // parent — leaving it un-waited leaks a zombie for the rest of the
    // cargo-test run (#1095).
    reap_test_child(&mut child, std::time::Duration::from_secs(5));
}

#[test]
fn post_session_consolidation_spawns_a_child() {
    // `current_exe()` always resolves under the test harness, so this
    // must spawn — the one assertion that would catch the whole
    // function being replaced with an unconditional `None` (#1465).
    let start = std::time::Instant::now();
    let child = post_session_consolidation();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "post_session_consolidation must not block on the child"
    );
    let mut child =
        child.expect("current_exe() resolving must spawn a detached consolidation child");
    reap_test_child(&mut child, std::time::Duration::from_secs(5));
}

/// Wait for `child` to exit, bounded by `timeout`; force-kill and wait
/// again if it doesn't exit in time. Used only to keep test-spawned
/// children from outliving the test run (#1095).
fn reap_test_child(child: &mut std::process::Child, timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}
