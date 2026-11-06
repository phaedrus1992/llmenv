use proptest::prelude::*;
use serde_json::{Value, json};

use super::{
    ResumeCache, api_key_helper_set, attach_system_message, field, native_warning_covers,
    resume_notice, usd, with_resume_notice,
};

fn expensive_resume() -> Value {
    json!({
        "source": "resume",
        "seconds_since_last_response": 5400,
        "context_tokens": 182_340,
        "prompt_cache_likely_expired": true,
        "estimated_cache_write_usd": 1.1396
    })
}

#[test]
fn all_four_fields_are_read() {
    let cache = ResumeCache::from_payload(&expensive_resume());

    assert_eq!(cache.seconds_since_last_response, Some(5400));
    assert_eq!(cache.context_tokens, Some(182_340));
    assert_eq!(cache.prompt_cache_likely_expired, Some(true));
    assert_eq!(cache.estimated_cache_write_usd, Some(1.1396));
}

#[test]
fn each_missing_field_is_absent_and_the_others_are_kept() {
    for key in [
        "seconds_since_last_response",
        "context_tokens",
        "prompt_cache_likely_expired",
        "estimated_cache_write_usd",
    ] {
        let mut payload = expensive_resume();
        payload.as_object_mut().unwrap().remove(key);

        let cache = ResumeCache::from_payload(&payload);

        assert!(cache.notice().is_none(), "a notice without {key}");
    }
    let cache = ResumeCache::from_payload(&json!({"context_tokens": 5}));
    assert_eq!(cache.context_tokens, Some(5));
    assert_eq!(cache.seconds_since_last_response, None);
}

#[test]
fn a_wrong_type_is_absent_for_each_field() {
    let payload = json!({
        "seconds_since_last_response": "5400",
        "context_tokens": -1,
        "prompt_cache_likely_expired": "yes",
        "estimated_cache_write_usd": "1.14"
    });

    let cache = ResumeCache::from_payload(&payload);

    assert_eq!(cache, ResumeCache::default());
}

#[test]
fn a_wrong_type_is_recorded_for_the_agent_with_its_field_name() {
    let payload = json!({ "probe_cache_cost": "1.14" });

    assert_eq!(field(&payload, "probe_cache_cost", usd), None);

    let text = crate::agent_notice::current().unwrap_or_default();
    assert!(text.contains("probe_cache_cost"), "{text}");
    assert!(text.contains("Fix:"), "{text}");
}

#[test]
fn the_resume_notice_names_a_wrong_type_before_the_notices_are_taken() {
    let payload = json!({
        "seconds_since_last_response": "probe-wrong-type-resume",
        "estimated_cache_write_usd": 1.14
    });

    let message = resume_notice("SessionStart", &payload, |_| None, || false);

    let taken = crate::agent_notice::current().unwrap_or_default();
    assert!(message.is_none(), "{message:?}");
    assert!(taken.contains("seconds_since_last_response"), "{taken}");
}

#[test]
fn a_negative_cost_is_not_a_cost() {
    let payload = json!({"estimated_cache_write_usd": -0.5});

    assert_eq!(
        ResumeCache::from_payload(&payload).estimated_cache_write_usd,
        None
    );
}

#[test]
fn a_huge_token_count_is_kept_as_read() {
    let payload = json!({"context_tokens": u64::MAX});

    assert_eq!(
        ResumeCache::from_payload(&payload).context_tokens,
        Some(u64::MAX)
    );
}

#[test]
fn an_expired_resume_above_the_threshold_gives_the_three_numbers() {
    let notice = ResumeCache::from_payload(&expensive_resume())
        .notice()
        .expect("an expired resume over 0.50 USD gives a notice");

    assert_eq!(
        notice,
        "Prompt cache expired after 90 min idle. Resuming rewrites about 182k tokens \
         (about $1.14). /compact first, or start fresh, to lower the cost."
    );
}

#[test]
fn the_threshold_is_inclusive_and_a_cheaper_resume_gives_no_notice() {
    let mut payload = expensive_resume();
    payload["estimated_cache_write_usd"] = json!(0.50);
    assert!(ResumeCache::from_payload(&payload).notice().is_some());

    payload["estimated_cache_write_usd"] = json!(0.49);
    assert!(ResumeCache::from_payload(&payload).notice().is_none());
}

#[test]
fn a_warm_cache_gives_no_notice() {
    let mut payload = expensive_resume();
    payload["prompt_cache_likely_expired"] = json!(false);

    assert!(ResumeCache::from_payload(&payload).notice().is_none());
}

#[test]
fn a_startup_without_the_fields_gives_no_notice_and_the_output_is_unchanged() {
    let payload = json!({"source": "startup"});

    let message = resume_notice("SessionStart", &payload, |_| None, || false);

    assert_eq!(
        with_resume_notice("{\"x\":1}".to_string(), message.as_deref()),
        "{\"x\":1}"
    );
}

#[test]
fn the_notice_is_added_to_the_output_as_system_message() {
    let message = resume_notice("SessionStart", &expensive_resume(), |_| None, || false);
    let out = with_resume_notice(
        "{\"hookSpecificOutput\":{}}".to_string(),
        message.as_deref(),
    );
    let parsed: Value = serde_json::from_str(&out).unwrap();

    assert!(
        parsed["systemMessage"]
            .as_str()
            .unwrap()
            .contains("182k tokens")
    );
    assert!(parsed.get("hookSpecificOutput").is_some());
}

#[test]
fn an_empty_output_becomes_the_message_object_alone() {
    let out = attach_system_message("", "note");

    assert_eq!(out, "{\"systemMessage\":\"note\"}");
}

#[test]
fn other_events_never_get_the_notice() {
    let message = resume_notice("PreToolUse", &expensive_resume(), |_| None, || false);

    assert_eq!(with_resume_notice(String::new(), message.as_deref()), "");
}

#[test]
fn a_non_object_output_is_returned_unchanged() {
    assert_eq!(attach_system_message("[1,2]", "note"), "[1,2]");
}

#[test]
fn a_non_json_output_is_returned_unchanged() {
    assert_eq!(attach_system_message("not json", "note"), "not json");
}

proptest::proptest! {
    /// Any payload value never panics the parser, and a field of the wrong type is absent.
    #[test]
    fn a_wrong_type_never_fills_a_field(
        number in any::<u64>(),
        text in "[a-z0-9]{0,8}",
        flag in any::<bool>(),
    ) {
        let payload = json!({
            "seconds_since_last_response": text,
            "context_tokens": number,
            "prompt_cache_likely_expired": flag,
            "estimated_cache_write_usd": text,
        });

        let cache = ResumeCache::from_payload(&payload);

        prop_assert_eq!(cache.seconds_since_last_response, None);
        prop_assert_eq!(cache.context_tokens, Some(number));
        prop_assert_eq!(cache.prompt_cache_likely_expired, Some(flag));
        prop_assert_eq!(cache.estimated_cache_write_usd, None);
    }

    /// A cost below zero is never read, however large its magnitude.
    #[test]
    fn a_negative_cost_is_never_read(cost in -1.0e12f64..-1.0e-9) {
        let payload = json!({"estimated_cache_write_usd": cost});

        prop_assert_eq!(
            ResumeCache::from_payload(&payload).estimated_cache_write_usd,
            None
        );
    }

    /// The message lands at `systemMessage`, and every existing key survives.
    #[test]
    fn attaching_keeps_existing_keys_and_sets_the_message(
        key in "[a-z]{1,8}",
        value in any::<i64>(),
        message in "[ -~]{0,40}",
    ) {
        let mut object = serde_json::Map::new();
        object.insert(key.clone(), json!(value));
        let out = Value::Object(object).to_string();

        let attached = attach_system_message(&out, &message);
        let parsed: Value = serde_json::from_str(&attached).unwrap();

        prop_assert_eq!(&parsed["systemMessage"], &Value::String(message));
        prop_assert_eq!(&parsed[&key], &json!(value));
    }
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string())
    }
}

// #3103: Claude Code 2.1.304 warns on a subscription itself, so llmenv stays quiet there.
#[test]
fn a_subscription_on_a_new_engine_gets_no_llmenv_notice() {
    let env = env_of(&[("CLAUDE_CODE_VERSION", "2.1.304")]);
    assert_eq!(
        resume_notice("SessionStart", &expensive_resume(), env, || false),
        None
    );
}

// #3103: an API key, a gateway, or a cloud provider gets no native warning.
#[test]
fn metered_auth_keeps_the_llmenv_notice() {
    for var in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        let env = env_of(&[("CLAUDE_CODE_VERSION", "2.1.318"), (var, "1")]);
        assert!(
            resume_notice("SessionStart", &expensive_resume(), env, || false).is_some(),
            "{var}"
        );
    }
}

#[test]
fn a_version_that_is_not_a_version_keeps_the_notice_and_says_why() {
    let env = env_of(&[("CLAUDE_CODE_VERSION", "nightly-3462")]);
    assert!(resume_notice("SessionStart", &expensive_resume(), env, || false).is_some());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains("CLAUDE_CODE_VERSION is not a version (\"nightly-3462\")"),
        "{notices}"
    );
}

#[test]
fn an_empty_version_keeps_the_notice_like_an_unset_one() {
    let env = env_of(&[("CLAUDE_CODE_VERSION", "")]);
    assert!(resume_notice("SessionStart", &expensive_resume(), env, || false).is_some());
    let notices = crate::agent_notice::current().unwrap_or_default();
    // The notice list is shared by the tests of one process, so only the empty value counts here.
    assert!(
        !notices.contains("CLAUDE_CODE_VERSION is not a version (\"\")"),
        "{notices}"
    );
}

#[test]
fn an_old_or_unknown_engine_or_an_off_flag_keeps_the_llmenv_notice() {
    for env in [
        env_of(&[("CLAUDE_CODE_VERSION", "2.1.303")]),
        env_of(&[("CLAUDE_CODE_VERSION", "not a version")]),
        env_of(&[]),
    ] {
        assert!(resume_notice("SessionStart", &expensive_resume(), env, || false).is_some());
    }
    // A provider flag set to 0 or empty is off, so the session is still a subscription.
    let env = env_of(&[
        ("CLAUDE_CODE_VERSION", "2.1.304"),
        ("CLAUDE_CODE_USE_BEDROCK", "0"),
        ("ANTHROPIC_API_KEY", ""),
    ]);
    assert_eq!(
        resume_notice("SessionStart", &expensive_resume(), env, || false),
        None
    );
}

// An apiKeyHelper in any settings file of the session is metered auth (#3103).
#[test]
fn an_api_key_helper_in_any_settings_file_counts() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    assert!(!api_key_helper_set(Some(&config), Some(&project)));
    std::fs::write(config.join("settings.json"), r#"{"hooks":{}}"#).unwrap();
    assert!(!api_key_helper_set(Some(&config), Some(&project)));
    std::fs::write(
        project.join(".claude/settings.local.json"),
        r#"{"apiKeyHelper":"k.sh"}"#,
    )
    .unwrap();
    assert!(api_key_helper_set(Some(&config), Some(&project)));
    // A settings file that cannot be parsed keeps the notice.
    std::fs::write(project.join(".claude/settings.local.json"), "{not json").unwrap();
    assert!(api_key_helper_set(Some(&config), Some(&project)));
    assert!(!api_key_helper_set(None, None));
}

#[test]
fn a_prerelease_of_2_1_304_keeps_the_notice() {
    let env = env_of(&[("CLAUDE_CODE_VERSION", "2.1.304-beta")]);
    assert!(resume_notice("SessionStart", &expensive_resume(), env, || false).is_some());
    let env = env_of(&[("CLAUDE_CODE_VERSION", "2.1.304")]);
    assert!(resume_notice("SessionStart", &expensive_resume(), env, || true).is_some());
}

proptest! {
    /// The native warning covers the session exactly when no metered variable is set, no
    /// provider switch is on, no apiKeyHelper is set, and the version is a release at or
    /// after 2.1.304.
    #[test]
    fn the_native_warning_rule_holds_for_any_environment(
        minor in 0u64..12, patch in 0u64..400,
        key in prop::sample::select(vec!["", "sk-x", "0"]),
        flag in prop::sample::select(vec!["", "0", "false", "1", "true"]),
        helper in any::<bool>(),
    ) {
        let version = format!("2.{minor}.{patch}");
        let env = env_of(&[
            ("CLAUDE_CODE_VERSION", version.as_str()),
            ("ANTHROPIC_API_KEY", key),
            ("CLAUDE_CODE_USE_VERTEX", flag),
        ]);
        let new_enough = minor > 1 || (minor == 1 && patch >= 304);
        let subscription = key.is_empty() && matches!(flag, "" | "0" | "false") && !helper;
        prop_assert_eq!(native_warning_covers(&env, || helper), new_enough && subscription);
    }
}

#[test]
fn a_process_variable_is_read_as_it_is_and_an_unset_one_is_none() {
    assert_eq!(super::hook_env("PATH"), std::env::var("PATH").ok());
    assert_eq!(
        super::hook_env("LLMENV_TEST_VARIABLE_THAT_IS_NEVER_SET"),
        None
    );
}

#[test]
fn the_process_notice_only_exists_for_a_session_start() {
    let payload = expensive_resume();
    assert_eq!(super::session_resume_notice("Stop", &payload), None);
    // Claude Code sets the version in its own hook processes, and the notice may then be
    // covered by its own warning. Without it, nothing covers the notice.
    if std::env::var_os("CLAUDE_CODE_VERSION").is_none() {
        let notice = super::session_resume_notice("SessionStart", &payload);
        assert!(notice.is_some_and(|n| !n.is_empty()));
    }
}
