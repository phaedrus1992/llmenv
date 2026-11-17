use super::*;
use crate::config::Throttle;
use proptest::prelude::*;

fn cfg(max_wait: u64, soft_threshold: u64) -> Throttle {
    Throttle {
        backend: "umans".to_string(),
        when: vec!["work".to_string()],
        cache_ttl: 30,
        max_wait,
        soft_threshold,
    }
}

fn snap(remaining: Option<u64>, penalized: bool) -> UsageSnapshot {
    UsageSnapshot {
        remaining,
        limit: Some(200),
        resets_at: None,
        penalized,
    }
}

proptest::proptest! {
    // Whatever the stdin and the event, the call never panics. The name is empty only for an
    // unknown event with no usable payload name, and an empty name always adds a notice.
    #[test]
    fn hook_event_name_never_panics_and_notices_an_empty_name(
        stdin in "\\PC{0,64}",
        event in prop_oneof![Just("prompt"), Just("pre-tool"), Just("other")],
    ) {
        let mut notices = Vec::new();
        let name = hook_event_name(&stdin, event, &mut notices);
        if name.is_empty() {
            proptest::prop_assert!(!notices.is_empty());
            proptest::prop_assert_eq!(event, "other");
        }
    }

    #[test]
    fn a_string_event_name_is_returned_verbatim(name in "[A-Za-z]{1,24}") {
        let mut notices = Vec::new();
        let stdin = serde_json::json!({ "hook_event_name": name }).to_string();
        proptest::prop_assert_eq!(hook_event_name(&stdin, "prompt", &mut notices), name);
        proptest::prop_assert!(notices.is_empty());
    }
}

#[test]
fn a_non_string_event_name_falls_back_with_a_notice() {
    let mut notices = Vec::new();
    let got = hook_event_name(r#"{"hook_event_name": 7}"#, "pre-tool", &mut notices);
    assert_eq!(got, "PreToolUse");
    assert!(
        notices.len() == 1 && notices[0].contains("not a string"),
        "{notices:?}"
    );
}

#[test]
fn hook_event_name_reads_stdin_and_falls_back_with_a_notice() {
    let mut notices = Vec::new();
    assert_eq!(
        hook_event_name(
            r#"{"hook_event_name":"PreToolUse"}"#,
            "prompt",
            &mut notices
        ),
        "PreToolUse"
    );
    assert_eq!(
        hook_event_name("", "prompt", &mut notices),
        "UserPromptSubmit"
    );
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(
        hook_event_name("not json", "pre-tool", &mut notices),
        "PreToolUse"
    );
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("not valid JSON") && notices[0].contains("Fix:"));
    assert_eq!(engine_event_name("other"), "");
}

// #3760: a payload with no name, and an event that maps to no name, each add a notice.
#[test]
fn a_payload_with_no_event_name_adds_a_notice() {
    let mut notices = Vec::new();
    let got = hook_event_name(r#"{"session_id":"x"}"#, "prompt", &mut notices);
    assert_eq!(got, "UserPromptSubmit");
    assert!(
        notices.len() == 1
            && notices[0].contains("no hook_event_name")
            && notices[0].contains("Fix:"),
        "{notices:?}"
    );
}

#[test]
fn an_unknown_event_with_no_payload_name_adds_a_notice() {
    let mut notices = Vec::new();
    let got = hook_event_name("", "other", &mut notices);
    assert_eq!(got, "");
    assert!(
        notices.len() == 1 && notices[0].contains("not a known event"),
        "{notices:?}"
    );
}

#[test]
fn penalized_returns_max_wait() {
    let d = compute_delay(&snap(Some(100), true), &cfg(300, 20));
    assert_eq!(d, Duration::from_secs(300));
}

#[test]
fn penalized_ignores_boxed_until_in_snapshot() {
    // penalized flag is set by the backend, regardless of remaining
    let d = compute_delay(&snap(Some(150), true), &cfg(300, 20));
    assert_eq!(d, Duration::from_secs(300));
}

#[test]
fn unknown_remaining_returns_zero() {
    let d = compute_delay(&snap(None, false), &cfg(300, 20));
    assert_eq!(d, Duration::ZERO);
}

#[test]
fn remaining_zero_returns_max_wait() {
    let d = compute_delay(&snap(Some(0), false), &cfg(300, 20));
    assert_eq!(d, Duration::from_secs(300));
}

#[test]
fn healthy_remaining_returns_zero() {
    let d = compute_delay(&snap(Some(50), false), &cfg(300, 20));
    assert_eq!(d, Duration::ZERO);
}

#[test]
fn at_threshold_returns_zero() {
    // remaining == soft_threshold → not under threshold
    let d = compute_delay(&snap(Some(20), false), &cfg(300, 20));
    assert_eq!(d, Duration::ZERO);
}

#[test]
fn just_under_threshold_returns_scaled() {
    // remaining=19, threshold=20, max_wait=300 → 300 * 1 / 20 = 15
    let d = compute_delay(&snap(Some(19), false), &cfg(300, 20));
    assert_eq!(d, Duration::from_secs(15));
}

#[test]
fn remaining_one_returns_near_max() {
    // remaining=1, threshold=20, max_wait=300 → 300 * 19 / 20 = 285
    let d = compute_delay(&snap(Some(1), false), &cfg(300, 20));
    assert_eq!(d, Duration::from_secs(285));
}

#[test]
fn delay_grows_as_remaining_shrinks() {
    let c = cfg(300, 20);
    let d10 = compute_delay(&snap(Some(10), false), &c);
    let d5 = compute_delay(&snap(Some(5), false), &c);
    assert!(d5 > d10);
}

#[test]
fn single_active_entry_selected() {
    let entries = vec![cfg(300, 20)];
    let mut tags = BTreeSet::new();
    tags.insert("work".to_string());
    let result = resolve_active_throttle(&entries, &tags).unwrap();
    assert!(result.is_some());
}

#[test]
fn no_active_entry_when_tags_inactive() {
    let entries = vec![cfg(300, 20)];
    let tags = BTreeSet::new();
    let result = resolve_active_throttle(&entries, &tags).unwrap();
    assert!(result.is_none());
}

#[test]
fn multiple_active_entries_is_error() {
    let mut e2 = cfg(300, 20);
    e2.backend = "umans2".to_string();
    e2.when = vec!["work".to_string()];
    let entries = vec![cfg(300, 20), e2];
    let mut tags = BTreeSet::new();
    tags.insert("work".to_string());
    let result = resolve_active_throttle(&entries, &tags);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("multiple entries active simultaneously"));
}

#[test]
fn throttle_config_defaults() {
    let yaml = "backend: umans\nwhen: [work]";
    let t: Throttle = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(t.cache_ttl, 30);
    assert_eq!(t.max_wait, 300);
    assert_eq!(t.soft_threshold, 20);
}

proptest! {
    #[test]
    fn prop_compute_delay_capped(
        max_wait in 0u64..3600,
        soft_threshold in 1u64..1000,
        remaining in any::<Option<u64>>(),
        penalized in any::<bool>(),
    ) {
        let c = cfg(max_wait, soft_threshold);
        let s = snap(remaining, penalized);
        let d = compute_delay(&s, &c);
        prop_assert!(d <= Duration::from_secs(max_wait));
    }

    #[test]
    fn prop_compute_delay_monotone(
        max_wait in 1u64..3600,
        soft_threshold in 1u64..1000,
        r1 in 0u64..2000,
        r2 in 0u64..2000,
    ) {
        // Higher remaining → smaller or equal delay (non-increasing).
        let c = cfg(max_wait, soft_threshold);
        let (lo, hi) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
        let d_lo = compute_delay(&snap(Some(lo), false), &c);
        let d_hi = compute_delay(&snap(Some(hi), false), &c);
        prop_assert!(
            d_lo >= d_hi,
            "delay should be non-increasing as remaining increases: \
             lo={lo} d={d_lo:?} hi={hi} d={d_hi:?}"
        );
    }
}

#[test]
fn read_active_throttle_returns_none_when_no_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let result = read_active_throttle_with_state_dir(tmp.path()).unwrap();
    assert!(result.is_none());
}

proptest::proptest! {
    /// Content that does not parse as a throttle is an error that names the file, for any
    /// such content, and never a silent "throttling off" (#2609).
    #[test]
    fn any_non_throttle_state_is_an_error_naming_the_file(junk in "#[ -~]{1,64}") {
        let tmp = tempfile::tempdir().unwrap();
        let path = throttle_state_path(tmp.path());
        std::fs::write(&path, &junk).unwrap();
        let err = read_active_throttle_with_state_dir(tmp.path())
            .expect_err("junk content must not read as no throttle");
        let text = format!("{:#}", err);
        proptest::prop_assert!(text.contains(&path.display().to_string()));
    }
}

#[test]
fn read_active_throttle_errors_on_corrupt_state_file_with_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = throttle_state_path(tmp.path());
    std::fs::write(&path, b"{ not json").unwrap();

    // Corrupt state is an error, not "throttling off". Otherwise the
    // statusline would report no throttle for a file that exists.
    let err = read_active_throttle_with_state_dir(tmp.path()).unwrap_err();
    assert!(format!("{err:#}").contains(&path.display().to_string()));
}

#[test]
fn read_active_throttle_round_trips_stored_value() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Throttle {
        backend: "anthropic".to_string(),
        when: vec!["dev".to_string()],
        cache_ttl: 30,
        max_wait: 60,
        soft_threshold: 80,
    };

    // Write via the real store path (write_owner_only_atomic), not a
    // hand-serialized fixture, so this genuinely exercises the
    // store -> read round trip rather than just deserialization.
    store_active_throttle_with_state_dir(Some(&cfg), tmp.path()).unwrap();

    let result = read_active_throttle_with_state_dir(tmp.path()).unwrap();
    assert_eq!(result.unwrap().backend, "anthropic");
}

#[test]
fn store_active_throttle_with_state_dir_removes_file_on_none() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Throttle {
        backend: "anthropic".to_string(),
        when: vec!["dev".to_string()],
        cache_ttl: 30,
        max_wait: 60,
        soft_threshold: 80,
    };
    store_active_throttle_with_state_dir(Some(&cfg), tmp.path()).unwrap();
    assert!(
        read_active_throttle_with_state_dir(tmp.path())
            .unwrap()
            .is_some()
    );

    store_active_throttle_with_state_dir(None, tmp.path()).unwrap();
    assert!(
        read_active_throttle_with_state_dir(tmp.path())
            .unwrap()
            .is_none()
    );
}

proptest! {
    // Any config that the store writes reads back unchanged.
    #[test]
    fn stored_throttle_reads_back_unchanged(
        backend in "[a-z]{1,8}",
        when in proptest::collection::vec("[a-z]{1,8}", 0..4),
        cache_ttl in 0u64..100_000,
        max_wait in 0u64..100_000,
        soft_threshold in 0u64..=100,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Throttle { backend, when, cache_ttl, max_wait, soft_threshold };
        store_active_throttle_with_state_dir(Some(&cfg), tmp.path()).unwrap();
        let back = read_throttle_config(&throttle_state_path(tmp.path())).unwrap();
        prop_assert_eq!(serde_json::to_value(back.unwrap()).unwrap(), serde_json::to_value(&cfg).unwrap());
    }
}

#[test]
fn store_none_with_no_state_file_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    store_active_throttle_with_state_dir(None, tmp.path()).unwrap();
}

#[test]
fn store_none_on_directory_at_state_path_returns_error_with_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = throttle_state_path(tmp.path());
    std::fs::create_dir(&path).unwrap();

    let err = store_active_throttle_with_state_dir(None, tmp.path()).unwrap_err();
    assert!(format!("{err:#}").contains(&path.display().to_string()));
}

#[test]
fn read_throttle_config_missing_file_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let got = read_throttle_config(&throttle_state_path(tmp.path())).unwrap();
    assert!(got.is_none());
}

#[test]
fn read_throttle_config_unreadable_path_returns_error_with_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = throttle_state_path(tmp.path());
    std::fs::create_dir(&path).unwrap();

    let err = read_throttle_config(&path).unwrap_err();
    assert!(format!("{err:#}").contains(&path.display().to_string()));
}

#[test]
fn read_throttle_config_corrupt_file_returns_error_with_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = throttle_state_path(tmp.path());
    std::fs::write(&path, b"{ not json").unwrap();

    let err = read_throttle_config(&path).unwrap_err();
    assert!(format!("{err:#}").contains(&path.display().to_string()));
}

/// A sink whose every write fails with the given error kind.
struct FailingWriter(std::io::ErrorKind);

impl std::io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(self.0))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn hook_context_write_ignores_a_closed_reader() {
    let mut sink = FailingWriter(std::io::ErrorKind::BrokenPipe);
    assert!(write_hook_context(&mut sink, "ctx").is_ok());
}

#[test]
fn hook_context_write_returns_any_other_error() {
    let mut sink = FailingWriter(std::io::ErrorKind::StorageFull);
    let err = write_hook_context(&mut sink, "ctx").expect_err("a full disk must surface");
    assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
}

#[cfg(unix)]
#[test]
fn a_fifo_throttle_state_fails_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = throttle_state_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    let err = read_active_throttle_with_state_dir(dir.path()).unwrap_err();
    assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
}

#[test]
fn an_empty_event_name_falls_back_with_a_notice() {
    let mut notices = Vec::new();
    let got = hook_event_name(r#"{"hook_event_name":""}"#, "pre-tool", &mut notices);
    assert_eq!(got, "PreToolUse");
    assert!(
        notices.len() == 1 && notices[0].contains("no hook_event_name"),
        "{notices:?}"
    );
}
