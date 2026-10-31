//! Tests for the trace sink, its line shapes, and the marker emitters.

use super::*;

// emit_trace_line never panics when neither env var is set.
#[test]
fn emit_trace_line_never_panics_without_env_var() {
    emit_trace_line("[LLMENV_CACHE] content_hash hit 1.234ms");
    emit_trace_line("llmenv-trace {\"v\": 1}");
}

// emit_json_trace never panics when neither env var is set.
#[test]
fn emit_json_trace_never_panics_without_env_var() {
    emit_json_trace("export", serde_json::Map::new());
}

// Every documented trace line example parses with the same code path
// the binary uses to emit them. A format change that breaks parsing
// fails this test until the docs are updated.

// [LLMENV_CACHE] line parses correctly.
#[test]
fn cache_trace_line_parses() {
    let line = "[LLMENV_CACHE] content_hash hit 1.234ms extra_data";
    emit_trace_line(line);
}

// [LLMENV_SPAWN] line parses correctly.
#[test]
fn spawn_trace_line_parses() {
    let line = "[LLMENV_SPAWN] probe:ssid 1234us exit=0";
    emit_trace_line(line);
}

// [LLMENV_MCP_CALL] handshake line parses correctly.
#[test]
fn mcp_call_trace_line_parses() {
    let line = "[LLMENV_MCP_CALL] initialize 5678us";
    emit_trace_line(line);
}

// [LLMENV_MCP_CALL] line with status parses correctly.
#[test]
fn mcp_call_trace_line_with_status_parses() {
    let line = "[LLMENV_MCP_CALL] initialize 5678us status=500";
    emit_trace_line(line);
}

// [LLMENV_MCP_CALL] handshake line with req/resp bytes parses correctly.
#[test]
fn mcp_call_trace_line_with_bytes_parses() {
    let line = "[LLMENV_MCP_CALL] initialize 5678us req=256 resp=1024";
    emit_trace_line(line);
}

// [LLMENV_MCP_CALL] handshake line with bytes and status parses correctly.
#[test]
fn mcp_call_trace_line_with_bytes_and_status_parses() {
    let line = "[LLMENV_MCP_CALL] initialize 5678us req=256 resp=1024 status=500";
    emit_trace_line(line);
}

// [LLMENV_MCP_CALL] tool call line with req/resp bytes parses correctly.
#[test]
fn mcp_call_trace_tool_line_with_bytes_parses() {
    let line = "[LLMENV_MCP_CALL] icm_memory_recall 123us req=512 resp=2048";
    emit_trace_line(line);
}

// [LLMENV_LEDGER] line parses correctly.
#[test]
fn ledger_trace_line_parses() {
    let line = "[LLMENV_LEDGER] session_start lock_attempts=5 lock_wait_us=100 read_us=200 write_us=300 bytes=1024 state_unsaved=false";
    emit_trace_line(line);
}

// [LLMENV_CONTEXT] recall-budget line parses correctly.
#[test]
fn context_trace_line_parses() {
    let line = "[LLMENV_CONTEXT] recall_entries=10 recall_bytes=2048 injected_entries=5 injected_bytes=1024 duplicate_records=2 already_sent=1 omitted=3 skipped_actions=0";
    emit_trace_line(line);
}

// The marker is `llmenv-trace ` plus a flat JSON object: consumers read timing keys at the
// top level, and `v` and `op` sit beside them.
#[test]
fn a_reserved_trace_key_is_an_error_in_every_build() {
    for key in ["v", "op"] {
        let mut fields = serde_json::Map::new();
        fields.insert(key.to_string(), serde_json::json!("caller value"));
        let err = json_trace_line("export", fields).expect_err("a reserved key must fail");
        let text = format!("{err:#}");
        assert!(text.contains(key), "the error must name the key: {text}");
    }
}

#[test]
fn json_trace_line_is_prefixed_flat_and_versioned() {
    let mut fields = serde_json::Map::new();
    fields.insert("config_load_us".to_string(), serde_json::json!(12));
    let line = json_trace_line("export", fields).expect("test");
    let json = line.strip_prefix("llmenv-trace ").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(parsed["v"], 1);
    assert_eq!(parsed["op"], "export");
    assert_eq!(parsed["config_load_us"], 12);
    assert!(parsed.get("data").is_none());
}

// Each session-start sub-phase shows as a `_us` field only when it ran, and holds its own
// measured duration.
#[test]
fn session_start_sub_phases_report_their_own_durations() {
    let t0 = std::time::Instant::now();
    let t_config = t0 + Duration::from_micros(5);
    let t_scope = t0 + Duration::from_micros(9);
    let t_chunk = t0 + Duration::from_micros(500);
    let json = trace_timing_json(HookTiming {
        t_scope: Some(t_scope),
        t_chunk: Some(t_chunk),
        phases: SessionStartPhases {
            mcp_probe: Some(Duration::from_micros(120)),
            stale_hint: Some(Duration::from_micros(30)),
            ..SessionStartPhases::default()
        },
        ..HookTiming::new(t0, t_config)
    });
    assert_eq!(json["mcp_probe_us"], 120);
    assert_eq!(json["stale_hint_us"], 30);
    assert!(json.get("proxy_restart_us").is_none());
    assert!(json.get("cbm_roots_us").is_none());
    let sum: u64 = ["mcp_probe_us", "stale_hint_us"]
        .iter()
        .filter_map(|k| json[*k].as_u64())
        .sum();
    assert!(sum <= json["prep_us"].as_u64().unwrap());
}

// The trace file is owner-only, and a symlink at its path is not followed.
#[cfg(unix)]
#[test]
fn the_trace_file_is_owner_only_and_refuses_a_symlink() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.log");
    append_trace_line(&path, "[LLMENV_SPAWN] x 1us exit=0");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let link = dir.path().join("link.log");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    append_trace_line(&link, "[LLMENV_SPAWN] y 1us exit=0");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "the link is not followed"
    );
}

// Each builder step puts its values in the marker, and a step that did not run leaves its keys out.
#[test]
fn hook_timing_builders_fill_their_own_fields() {
    let t0 = Instant::now();
    let at = |us| t0 + Duration::from_micros(us);
    let timing = HookTiming::new(t0, at(5))
        .through_scope(at(9))
        .through_mcp(at(30), at(80))
        .with_events(
            SessionStartPhases::probed(Duration::from_micros(7)),
            Some(11),
            Some(12),
            Some(13),
        );
    let json = hook_marker(timing);
    assert_eq!(json["scope_eval_us"], 4);
    assert_eq!(json["prep_us"], 21);
    assert_eq!(json["mcp_us"], 50);
    assert_eq!(json["mcp_probe_us"], 7);
    assert_eq!(json["session_log_us"], 11);
    assert_eq!(json["checkpoint_us"], 12);
    assert_eq!(json["spawn_us"], 13);
    assert!(json["spawned_jobs"].is_u64());
    let early = hook_marker(HookTiming::new(t0, at(5)).through_scope(at(9)));
    assert!(early.get("prep_us").is_none() && early.get("mcp_us").is_none());
}

// A probe that answered is exit 0, a probe that ran out of time is a timeout, and any other
// failure is an error.
#[test]
fn of_probe_tells_an_answer_a_timeout_and_an_error_apart() {
    assert_eq!(SpawnExit::of_probe(true, false), SpawnExit::Code(0));
    assert_eq!(SpawnExit::of_probe(false, true), SpawnExit::Timeout);
    assert_eq!(SpawnExit::of_probe(false, false), SpawnExit::Error);
}

// A detached spawn is counted and traced once, and a failed start is not counted.
#[test]
fn spawn_counted_counts_only_a_start_that_worked() {
    let before = spawned_jobs();
    let mut ok = std::process::Command::new("true");
    let mut child = spawn_counted("test:true", &mut ok).unwrap();
    child.wait().unwrap();
    assert!(spawned_jobs() > before);
    let after_ok = spawned_jobs();
    let mut missing = std::process::Command::new("llmenv-no-such-program");
    assert!(spawn_counted("test:missing", &mut missing).is_err());
    assert_eq!(spawned_jobs(), after_ok);
}

// The sink writes one JSON object per line, and appends rather than truncates.
#[test]
fn append_trace_line_writes_json_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.log");
    append_trace_line(&path, "[LLMENV_SPAWN] probe:ssid 5us exit=0");
    append_trace_line(&path, "second \"quoted\" line");
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["line"], "[LLMENV_SPAWN] probe:ssid 5us exit=0");
    assert_eq!(lines[1]["line"], "second \"quoted\" line");
    assert_eq!(lines[0]["pid"], std::process::id());
}

// The request id of a detached job comes from its payload, and a missing one is None.
#[test]
fn json_str_reads_a_string_field_or_none() {
    assert_eq!(
        json_str(r#"{"request_id":"abc"}"#, "request_id").as_deref(),
        Some("abc")
    );
    assert_eq!(json_str(r#"{"request_id":3}"#, "request_id"), None);
    assert_eq!(json_str("not json", "request_id"), None);
}

// [LLMENV_DETACHED] line parses correctly.
#[test]
fn detached_trace_line_parses() {
    let line = "[LLMENV_DETACHED] consolidation 12345us cpu_us=10000 mcp_calls=3 exit=0";
    emit_trace_line(line);
}

// [LLMENV_DETACHED] line with error exit parses correctly.
#[test]
fn detached_trace_line_error_exit_parses() {
    let line = "[LLMENV_DETACHED] icm-store 5678us cpu_us=4000 mcp_calls=1 exit=1";
    emit_trace_line(line);
}

// trace_timing_json always yields a JSON object with "v": 1 and
// "config_load_us" present, and all timing values are non-negative.
proptest::proptest! {
    #[test]
    fn trace_timing_json_has_version_and_non_negative_timing(
        config_us in proptest::prelude::any::<u64>(),
        scope_us in proptest::prelude::any::<u64>(),
        chunk_us in proptest::prelude::any::<u64>(),
        end_us in proptest::prelude::any::<u64>(),
    ) {
        let t0 = std::time::Instant::now();
        let t_config = t0 + std::time::Duration::from_micros(config_us);
        let t_scope = t0 + std::time::Duration::from_micros(scope_us);
        let t_chunk = t0 + std::time::Duration::from_micros(chunk_us);
        let t_end = t0 + std::time::Duration::from_micros(end_us);
        let json = hook_marker(HookTiming {
            t_scope: Some(t_scope),
            t_chunk: Some(t_chunk),
            t_end: Some(t_end),
            ..HookTiming::new(t0, t_config)
        });
        proptest::prop_assert_eq!(&json["v"], &serde_json::json!(1));
        proptest::prop_assert!(json.get("config_load_us").is_some());
        proptest::prop_assert!(json["config_load_us"].is_u64());
        if let Some(scope) = json.get("scope_eval_us") {
            proptest::prop_assert!(scope.is_u64());
        }
        if let Some(prep) = json.get("prep_us") {
            proptest::prop_assert!(prep.is_u64());
        }
        if let Some(mcp) = json.get("mcp_us") {
            proptest::prop_assert!(mcp.is_u64());
        }
    }

    // trace_timing_json with all-None phases still carries "v": 1.
    #[test]
    fn trace_timing_json_with_none_phases_has_version(
        config_us in proptest::prelude::any::<u64>(),
    ) {
        let t0 = std::time::Instant::now();
        let t_config = t0 + std::time::Duration::from_micros(config_us);
        let json = hook_marker(HookTiming::new(t0, t_config));
        proptest::prop_assert_eq!(&json["v"], &serde_json::json!(1));
        proptest::prop_assert!(json.get("config_load_us").is_some());
    }
}

proptest::proptest! {
    // For any op and any fields that do not use the reserved keys, the marker is the prefix
    // plus one JSON object that keeps every field and adds `v` and `op`.
    #[test]
    fn json_trace_line_keeps_every_field_and_adds_v_and_op(
        op in "\\PC{0,20}",
        fields in proptest::collection::btree_map("[a-z_]{1,12}", 0u64..1_000_000, 0..8),
    ) {
        let mut map = serde_json::Map::new();
        for (key, value) in &fields {
            if key != "v" && key != "op" {
                map.insert(key.clone(), serde_json::json!(value));
            }
        }
        let line = json_trace_line(&op, map.clone()).unwrap();
        let json = line.strip_prefix("llmenv-trace ").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        proptest::prop_assert_eq!(&parsed["v"], &serde_json::json!(1));
        proptest::prop_assert_eq!(&parsed["op"], &serde_json::json!(op));
        for (key, value) in &map {
            proptest::prop_assert_eq!(&parsed[key.as_str()], value);
        }
        proptest::prop_assert!(!line.contains('\n'), "a marker is one line");
    }

    // `json_str` never panics, and it reads back any string field of a JSON object.
    #[test]
    fn json_str_never_panics_and_reads_back_a_string_field(
        text in "\\PC{0,60}",
        key in "[a-z_]{1,10}",
        value in "\\PC{0,30}",
    ) {
        let _ = json_str(&text, &key);
        let payload = serde_json::json!({ key.clone(): value.clone() }).to_string();
        proptest::prop_assert_eq!(json_str(&payload, &key), Some(value));
    }
}

proptest::proptest! {
    // The reserved keys must refuse the line for any other fields and any value, not only for the
    // fixed example above. The other property above filters these keys out, so it cannot show this.
    #[test]
    fn json_trace_line_refuses_any_map_that_holds_a_reserved_key(
        op in "[a-z_]{1,12}",
        reserved in proptest::sample::select(vec!["v", "op"]),
        value in proptest::prelude::any::<u64>(),
        extra in proptest::collection::btree_map("[a-z_]{1,12}", 0u64..1_000_000, 0..8),
    ) {
        let mut map = serde_json::Map::new();
        for (key, extra_value) in &extra {
            if key != "v" && key != "op" {
                map.insert(key.clone(), serde_json::json!(extra_value));
            }
        }
        map.insert(reserved.to_string(), serde_json::json!(value));
        let result = json_trace_line(&op, map);
        proptest::prop_assert!(result.is_err(), "a reserved key must refuse the line");
    }
}
