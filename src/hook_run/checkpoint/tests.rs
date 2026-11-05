use super::*;
use proptest::prelude::*;

pub(super) fn cp(kind: JobKind, n: u32) -> Checkpoint {
    Checkpoint::new(kind, serde_json::json!({ "n": n }), Some("sess"))
}

#[test]
fn a_checkpoint_load_refuses_a_fifo_and_a_file_over_128_kib() {
    assert_eq!(MAX_CHECKPOINT_BYTES, 131_072);
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo.json");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let err = load(&fifo).unwrap_err();
    assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
    let big = dir.path().join("big.json");
    std::fs::write(&big, vec![b' '; 131_073]).unwrap();
    let err = load(&big).unwrap_err();
    assert!(format!("{err:#}").contains("more than"), "{err:#}");
}

#[test]
fn job_inputs_come_from_stdin_when_it_parses() {
    let got: serde_json::Value = inputs_or_checkpoint(r#"{"a":1}"#, None).unwrap();
    assert_eq!(got, serde_json::json!({"a": 1}));
}

#[test]
fn a_truncated_stdin_falls_back_to_the_checkpoint_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let cp = Checkpoint::new(JobKind::IcmStore, serde_json::json!({"a": 1}), Some("s"));
    let path = write(dir.path(), &cp).unwrap().unwrap();
    let got: serde_json::Value = inputs_or_checkpoint(r#"{"a":"#, Some(&path)).unwrap();
    assert_eq!(got, serde_json::json!({"a": 1}));
    let got: serde_json::Value = inputs_or_checkpoint("", Some(&path)).unwrap();
    assert_eq!(got, serde_json::json!({"a": 1}));
}

#[test]
fn unusable_stdin_without_a_usable_checkpoint_is_an_error() {
    assert!(inputs_or_checkpoint::<serde_json::Value>("{", None).is_err());
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("x.json");
    std::fs::write(&bad, "not a checkpoint").unwrap();
    assert!(inputs_or_checkpoint::<serde_json::Value>("{", Some(&bad)).is_err());
    assert!(
        inputs_or_checkpoint::<serde_json::Value>("{", Some(&dir.path().join("none.json")))
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn a_checkpoint_folder_that_cannot_be_read_is_listed_as_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("checkpoints");
    std::fs::create_dir(&folder).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A user that ignores permissions (root) can read the folder, so the case cannot arise.
    let readable = std::fs::read_dir(&folder).is_ok();
    let entries = list(dir.path());
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    if readable {
        return;
    }
    assert!(
        matches!(entries.as_slice(), [Entry::Unreadable(_, why)] if why.contains("cannot read")),
        "{entries:?}"
    );
}

#[test]
fn a_missing_checkpoint_folder_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list(dir.path()).is_empty());
}

#[test]
fn the_index_job_counts_attempts_and_logs_an_unreadable_earlier_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = serde_json::json!({ "root": "/r" });
    let begin_index = || begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();

    // No earlier file: attempt 1, and nothing is logged.
    let mut path = PathBuf::new();
    let logs = crate::test_log_capture::capture_logs(|| path = begin_index());
    assert_eq!(load(&path).unwrap().attempts, 1);
    assert!(!logs.contains("unreadable"), "{logs}");

    // A readable earlier file: the count goes up by one.
    let logs = crate::test_log_capture::capture_logs(|| path = begin_index());
    assert_eq!(load(&path).unwrap().attempts, 2);
    assert!(!logs.contains("unreadable"), "{logs}");

    // An unreadable earlier file: the count restarts, and the agent is told (#3449).
    std::fs::write(&path, "not a checkpoint").unwrap();
    path = begin_index();
    assert_eq!(load(&path).unwrap().attempts, 1);
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("earlier code index checkpoint") && notices.contains("Fix:"),
        "{notices}"
    );
}

#[test]
fn key_is_stable_for_equal_inputs_and_differs_otherwise() {
    let a = serde_json::json!({ "x": 1 });
    assert_eq!(
        key_for(JobKind::IcmStore, &a),
        key_for(JobKind::IcmStore, &a)
    );
    assert_ne!(
        key_for(JobKind::IcmStore, &a),
        key_for(JobKind::SessionLogRecord, &a)
    );
    assert_ne!(
        key_for(JobKind::IcmStore, &a),
        key_for(JobKind::IcmStore, &serde_json::json!({ "x": 2 }))
    );
}

#[test]
fn write_list_complete_round_trip_and_double_complete_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let checkpoint = cp(JobKind::IcmStore, 1);
    let path = write(dir.path(), &checkpoint).unwrap().unwrap();
    assert_eq!(
        list(dir.path()),
        vec![Entry::Ready(path.clone(), checkpoint)]
    );
    complete(&path).unwrap();
    complete(&path).unwrap();
    assert!(list(dir.path()).is_empty());
}

#[test]
fn the_same_job_maps_to_one_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &cp(JobKind::IcmStore, 1)).unwrap();
    write(dir.path(), &cp(JobKind::IcmStore, 1)).unwrap();
    assert_eq!(list(dir.path()).len(), 1);
}

#[test]
fn inputs_up_to_the_cap_are_checkpointed_and_one_byte_more_is_not() {
    let dir = tempfile::tempdir().unwrap();
    // The JSON text of a string is the string plus two quotes.
    let at_cap = serde_json::json!("x".repeat(MAX_INPUT_BYTES - 2));
    let over = serde_json::json!("x".repeat(MAX_INPUT_BYTES - 1));
    assert_eq!(MAX_INPUT_BYTES, 65_536);
    assert!(
        write(
            dir.path(),
            &Checkpoint::new(JobKind::IcmStore, at_cap, None)
        )
        .unwrap()
        .is_some()
    );
    assert!(
        write(dir.path(), &Checkpoint::new(JobKind::IcmStore, over, None))
            .unwrap()
            .is_none()
    );
    let modest = serde_json::json!({ "content": "y".repeat(10_000) });
    assert!(
        write(
            dir.path(),
            &Checkpoint::new(JobKind::SessionLogRecord, modest, None)
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn deadlines_are_the_longest_timeout_plus_the_margin() {
    assert_eq!(JobKind::Consolidation.deadline_secs(), 150 + 60);
    assert_eq!(JobKind::IcmStore.deadline_secs(), 5 + 60);
    assert_eq!(JobKind::SessionLogRecord.deadline_secs(), 5 + 60);
    assert_eq!(JobKind::CbmIndex.deadline_secs(), 1800 + 60);
}

#[test]
fn now_secs_is_the_current_time() {
    assert!(now_secs() > 1_700_000_000, "{}", now_secs());
    assert!(now_secs() < 4_000_000_000);
}

#[test]
fn completing_a_path_that_cannot_be_removed_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_file = dir.path().join("adir");
    std::fs::create_dir_all(not_a_file.join("inner")).unwrap();
    assert!(complete(&not_a_file).is_err());
    assert!(complete(&dir.path().join("missing.json")).is_ok());
}

#[test]
fn inputs_above_the_cap_are_not_checkpointed() {
    let dir = tempfile::tempdir().unwrap();
    let big = Checkpoint::new(
        JobKind::IcmStore,
        serde_json::json!({ "blob": "x".repeat(MAX_INPUT_BYTES + 1) }),
        None,
    );
    assert_eq!(write(dir.path(), &big).unwrap(), None);
    assert!(list(dir.path()).is_empty());
}

#[test]
fn a_corrupt_file_is_listed_as_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(super::dir(dir.path())).unwrap();
    std::fs::write(super::dir(dir.path()).join("icm-store-bad.json"), "{ nope").unwrap();
    assert!(matches!(&list(dir.path())[..], [Entry::Unreadable(..)]));
}

#[test]
fn update_changes_the_phase_and_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), &cp(JobKind::Consolidation, 1))
        .unwrap()
        .unwrap();
    update(&path, |c| {
        c.phase = "summarized".into();
        c.inputs["summary"] = serde_json::json!("rules");
    })
    .unwrap();
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.phase, "summarized");
    assert_eq!(loaded.inputs["summary"], "rules");
}

#[test]
fn staleness_follows_the_deadline_of_the_kind() {
    let c = cp(JobKind::IcmStore, 1);
    let deadline = JobKind::IcmStore.deadline_secs();
    assert!(!c.is_stale(c.started_at + deadline));
    assert!(c.is_stale(c.started_at + deadline + 1));
    assert!(JobKind::CbmIndex.deadline_secs() > JobKind::Consolidation.deadline_secs());
}

#[test]
fn run_tags_differ_and_the_first_attempt_records_the_working_directory() {
    assert_ne!(run_tag(), run_tag());
    let c = cp(JobKind::IcmStore, 1);
    let cwd = std::env::current_dir().unwrap().display().to_string();
    assert_eq!(c.cwd.as_deref(), Some(cwd.as_str()));
}

#[test]
fn only_a_regular_checkpoint_file_inside_the_checkpoint_dir_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let good = write(dir.path(), &cp(JobKind::IcmStore, 1))
        .unwrap()
        .unwrap();
    assert_eq!(validated_path(dir.path(), &good).unwrap(), good);
    let outside = dir.path().join("elsewhere.json");
    std::fs::write(&outside, "{}").unwrap();
    assert!(
        validated_path(dir.path(), &outside).is_err(),
        "outside the directory"
    );
    let not_json = super::dir(dir.path()).join("x.txt");
    std::fs::write(&not_json, "{}").unwrap();
    assert!(
        validated_path(dir.path(), &not_json).is_err(),
        "not a .json name"
    );
    let link = super::dir(dir.path()).join("icm-store-link.json");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(
        validated_path(dir.path(), &link).is_err(),
        "a symlink is refused"
    );
    let traversal = super::dir(dir.path()).join("../elsewhere.json");
    assert!(
        validated_path(dir.path(), &traversal).is_err(),
        "a .. path is refused"
    );
}

#[test]
fn describe_names_kind_age_attempts_phase_and_log() {
    let mut c = cp(JobKind::Consolidation, 1);
    c.started_at = 100;
    c.attempts = 3;
    let text = describe(&c, 100 + 7300, Path::new("/s/detached-hook.log"));
    assert_eq!(
        text,
        "consolidation started 2h 1m 40s ago, 3/3 attempts, phase started; log: /s/detached-hook.log"
    );
}

proptest! {
    #[test]
    fn format_age_is_never_empty_and_ends_in_a_unit(secs in 0u64..10_000_000) {
        let text = format_age(secs);
        prop_assert!(!text.is_empty());
        prop_assert!(text.ends_with(['s', 'm', 'h', 'd']), "{text}");
    }

    #[test]
    fn a_checkpoint_survives_serialization(
        n in 0u32..1000, attempts in 0u32..10, started in 0u64..u64::MAX / 2,
        phase in "[a-z]{1,10}", session in proptest::option::of("[a-z0-9-]{1,12}"),
    ) {
        let mut c = cp(JobKind::SessionLogRecord, n);
        c.attempts = attempts;
        c.started_at = started;
        c.phase = phase;
        c.session_id = session;
        let back: Checkpoint = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        prop_assert_eq!(back, c);
    }

    #[test]
    fn attempts_never_decrease_through_resume(attempts in 1u32..6, age in 0u64..10_000) {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cp(JobKind::IcmStore, 1);
        c.started_at = 1000;
        c.attempts = attempts;
        let path = write(dir.path(), &c).unwrap().unwrap();
        resume_pending(dir.path(), 1000 + age, |_, _| Ok(()));
        prop_assert!(load(&path).unwrap().attempts >= attempts);
    }
}

#[expect(clippy::unwrap_used, reason = "test code")]
mod truncation;
