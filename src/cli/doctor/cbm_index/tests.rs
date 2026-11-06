use super::*;

const LOG: &str = "/state/codebase-memory/index.log";

fn run(text: &str) -> (CheckLevel, String) {
    classify(text, "2026-10-03 12:00 UTC", Path::new(LOG))
}

#[test]
fn an_over_budget_result_names_the_numbers_and_the_setting() {
    let (level, text) = run(
        r#"{"status":"error","reason":"over_memory_budget","previous_index":"preserved",
            "budget_mb":128,"peak_rss_mb":900,"suggested_budget_mb":1024}"#,
    );
    assert_eq!(level, CheckLevel::Warn);
    assert_eq!(
        text,
        "codebase-memory: last index (2026-10-03 12:00 UTC) stopped at the memory budget: \
         budget 128 MB, peak 900 MB. The previous index is still served. Set \
         codebase_memory.mem_budget_mb to 1024."
    );
}

#[test]
fn an_over_budget_result_leaves_out_a_missing_number() {
    let (_, text) = run(r#"{"reason":"over_memory_budget","budget_mb":128}"#);
    assert!(
        text.contains("budget 128 MB.") && !text.contains("peak"),
        "{text}"
    );
    assert!(!text.contains("Set codebase_memory"), "{text}");
    let (_, bare) = run(r#"{"reason":"over_memory_budget"}"#);
    assert_eq!(
        bare,
        "codebase-memory: last index (2026-10-03 12:00 UTC) stopped at the memory budget. \
         The previous index is still served."
    );
}

#[test]
fn every_failed_status_warns_with_reason_hint_and_log() {
    for status in FAILED_STATUSES {
        let (level, text) = run(&format!(
            r#"{{"status":"{status}","reason":"disk","hint":"free some space"}}"#
        ));
        assert_eq!(level, CheckLevel::Warn, "{status}");
        assert_eq!(
            text,
            format!(
                "codebase-memory: last index (2026-10-03 12:00 UTC) ended with status \
                 {status}, reason disk. free some space. Log: {LOG}"
            )
        );
    }
    let (_, plain) = run(r#"{"status":"error"}"#);
    assert!(
        plain.ends_with(&format!("status error. Log: {LOG}")),
        "{plain}"
    );
}

#[test]
fn a_success_result_passes() {
    for text in [r#"{"status":"ok","nodes":10}"#, r#"{"nodes":10}"#, "{}"] {
        assert_eq!(
            run(text),
            (
                CheckLevel::Pass,
                "codebase-memory: last index finished 2026-10-03 12:00 UTC".to_string()
            ),
            "{text}"
        );
    }
}

#[test]
fn an_unreadable_result_is_information() {
    for text in ["", "   \n", "not json", "{\"status\":"] {
        let (level, message) = run(text);
        assert_eq!(level, CheckLevel::Info, "{text:?}");
        assert!(message.contains("result not readable"), "{message}");
    }
}

#[test]
fn server_text_loses_control_characters() {
    let (_, text) = run("{\"status\":\"error\",\"hint\":\"a\\u001b]52;c;x\\u0007b\"}");
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
}

#[test]
fn the_check_reads_the_file_of_this_project_only() {
    let dir = tempfile::tempdir().unwrap();
    let (mine, other) = (Path::new("/work/mine"), Path::new("/work/other"));
    assert_eq!(
        check(dir.path(), mine),
        (
            CheckLevel::Info,
            "codebase-memory: no index result for this project yet".to_string()
        )
    );
    let path = crate::hook_run::index_result_path(dir.path(), other);
    std::fs::write(path, r#"{"status":"error"}"#).unwrap();
    assert_eq!(check(dir.path(), mine).0, CheckLevel::Info);
    let own = crate::hook_run::index_result_path(dir.path(), mine);
    std::fs::write(own, r#"{"status":"ok"}"#).unwrap();
    let (level, text) = check(dir.path(), mine);
    assert_eq!(level, CheckLevel::Pass);
    assert!(
        text.starts_with("codebase-memory: last index finished 20"),
        "{text}"
    );
    assert!(text.ends_with(" UTC"), "{text}");
}

#[test]
fn json_that_is_not_an_object_is_not_a_result() {
    for text in ["[]", "null", "\"x\"", "7", "true"] {
        assert_eq!(run(text).0, CheckLevel::Info, "{text}");
    }
}

#[test]
fn a_result_path_that_is_not_a_regular_file_is_reported_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let project = Path::new("/work/p");
    let path = crate::hook_run::index_result_path(dir.path(), project);
    let target = dir.path().join("elsewhere");
    std::fs::write(&target, r#"{"status":"error"}"#).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let (level, text) = check(dir.path(), project);
    assert_eq!(level, CheckLevel::Info);
    assert!(text.contains("not a regular file"), "{text}");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(check(dir.path(), project).1.contains("not a regular file"));
}

#[test]
fn a_huge_result_file_is_read_up_to_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let project = Path::new("/work/p");
    let path = crate::hook_run::index_result_path(dir.path(), project);
    let mut text = String::from(r#"{"status":"ok","pad":""#);
    text.push_str(&"x".repeat(200_000));
    text.push_str("\"}");
    std::fs::write(path, text).unwrap();
    // The bound cuts the JSON, so it does not parse and the line says so.
    assert!(check(dir.path(), project).1.contains("result not readable"));
}

#[test]
fn a_result_cut_inside_its_last_character_is_classified_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    let project = Path::new("/work/p");
    let path = crate::hook_run::index_result_path(dir.path(), project);
    std::fs::write(&path, b"{\"status\":\"ok\"}\n\xe2\x82").unwrap();
    let (level, text) = check(dir.path(), project);
    assert_eq!(level, CheckLevel::Pass, "{text}");
    std::fs::write(&path, b"{\"hint\":\"caf\xc3").unwrap();
    let (_, text) = check(dir.path(), project);
    assert!(text.contains("result not readable"), "{text}");
}

proptest::proptest! {
    #[test]
    fn no_text_makes_the_classification_panic(text in "\\PC{0,200}") {
        let (_, message) = run(&text);
        proptest::prop_assert!(message.starts_with("codebase-memory: "));
    }

    #[test]
    fn any_object_of_scalars_classifies_without_panic(
        status in proptest::option::of("[a-z_]{0,12}"),
        reason in proptest::option::of("[a-z_]{0,12}"),
        budget in proptest::option::of(proptest::prelude::any::<u64>()),
        hint in proptest::option::of("\\PC{0,30}"),
    ) {
        let value = serde_json::json!({
            "status": status, "reason": reason, "budget_mb": budget, "hint": hint,
        });
        let (level, message) = run(&value.to_string());
        proptest::prop_assert!(message.starts_with("codebase-memory: "));
        if reason.as_deref() == Some("over_memory_budget") {
            proptest::prop_assert_eq!(level, CheckLevel::Warn);
        }
    }
}

#[test]
fn the_time_is_formatted_in_utc() {
    let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
    assert_eq!(format_time(time), "2026-09-21 14:13 UTC");
}
