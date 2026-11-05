//! The prompt-cache fields that Claude Code reports on a resumed session, and the notice for an
//! expensive resume (#2496).
//!
//! A resume after the cache lifetime rewrites the whole prompt at a cost. The notice goes to the
//! person, as `systemMessage`, because the agent does not need it and the context would grow.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A resume that rewrites at least this many USD of prompt cache gets a notice. Below it, the
/// notice costs more attention than the cache write costs money.
const DEFAULT_CACHE_NOTICE_MIN_USD: f64 = 0.50;

/// The four `SessionStart` fields that Claude Code sends on `resume` or `fork`. Each one is absent
/// when the field is missing or has the wrong type, so a bad field never fails the hook.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResumeCache {
    seconds_since_last_response: Option<u64>,
    context_tokens: Option<u64>,
    prompt_cache_likely_expired: Option<bool>,
    estimated_cache_write_usd: Option<f64>,
}

impl ResumeCache {
    /// Read the four fields from a `SessionStart` payload.
    pub(crate) fn from_payload(payload: &Value) -> Self {
        Self {
            seconds_since_last_response: field(
                payload,
                "seconds_since_last_response",
                Value::as_u64,
            ),
            context_tokens: field(payload, "context_tokens", Value::as_u64),
            prompt_cache_likely_expired: field(
                payload,
                "prompt_cache_likely_expired",
                Value::as_bool,
            ),
            estimated_cache_write_usd: field(payload, "estimated_cache_write_usd", usd),
        }
    }

    /// The one-line notice for an expensive resume. `None` when any field is absent, when the
    /// cache is still warm, or when the cost is under [`DEFAULT_CACHE_NOTICE_MIN_USD`].
    fn notice(&self) -> Option<String> {
        if self.prompt_cache_likely_expired != Some(true) {
            return None;
        }
        let cost = self
            .estimated_cache_write_usd
            .filter(|cost| *cost >= DEFAULT_CACHE_NOTICE_MIN_USD)?;
        let idle_min = self.seconds_since_last_response? / 60;
        let tokens_k = self.context_tokens? / 1000;
        Some(format!(
            "Prompt cache expired after {idle_min} min idle. Resuming rewrites about {tokens_k}k \
             tokens (about ${cost:.2}). /compact first, or start fresh, to lower the cost."
        ))
    }
}

/// Read one field. A missing field is absent. A field of the wrong type is absent too, and the
/// wrong type is logged, so a bad engine payload stays visible without a failed hook.
fn field<T>(payload: &Value, key: &str, read: fn(&Value) -> Option<T>) -> Option<T> {
    let value = payload.get(key)?;
    let parsed = read(value);
    if parsed.is_none() {
        // The default log filter hides warn, and hook stderr is dropped, so the agent is told.
        crate::agent_notice::record_logged(format!(
            "llmenv: SessionStart field \"{key}\" has the wrong type, so the resume cost notice \
             is not shown this session. Fix: update llmenv to match the engine's field type."
        ));
    }
    parsed
}

/// A cost in USD. A negative or non-finite number is not a cost.
fn usd(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// Add `message` as the top-level `systemMessage` of the hook output `out`. An empty `out`
/// becomes the object that holds only the message. An `out` that is not a JSON object is logged
/// and returned unchanged, so the message is lost only with an error in the log.
fn attach_system_message(out: &str, message: &str) -> String {
    let mut object = if out.is_empty() {
        serde_json::Map::new()
    } else {
        match serde_json::from_str::<Value>(out) {
            Ok(Value::Object(object)) => object,
            Ok(_) => {
                crate::agent_notice::record_logged(
                    "llmenv: hook output is not a JSON object, so the cache notice is not shown. Fix: check the hook that prints this output.",
                );
                return out.to_string();
            }
            Err(e) => {
                crate::agent_notice::record_logged(format!(
                    "llmenv: hook output is not JSON ({e}), so the cache notice is not shown. Fix: check the hook that prints this output."
                ));
                return out.to_string();
            }
        }
    };
    object.insert(
        "systemMessage".to_string(),
        Value::String(message.to_string()),
    );
    Value::Object(object).to_string()
}

/// The hook output for a `SessionStart` event. A resume with an expensive, expired cache adds
/// the notice as `systemMessage`. Every other event and payload returns `out` unchanged.
/// The resume cost notice for this event, or `None`. A wrong-type field is recorded for the agent
/// as a side effect, so the caller must take the notice list after this call.
///
/// `env` reads a variable of the hook process, such as [`hook_env`], and `api_key_helper` says
/// whether a settings file sets `apiKeyHelper`.
fn resume_notice(
    hook_event_name: &str,
    payload: &Value,
    env: impl Fn(&str) -> Option<String>,
    api_key_helper: impl FnOnce() -> bool,
) -> Option<String> {
    if hook_event_name != "SessionStart" {
        return None;
    }
    let notice = ResumeCache::from_payload(payload).notice()?;
    // Claude Code warns about an expensive resume itself on a subscription, so a second notice
    // would repeat it (#3103).
    (!native_warning_covers(&env, api_key_helper)).then_some(notice)
}

/// [`resume_notice`] for the hook process: its environment, and the settings files of its
/// session, found through `CLAUDE_CONFIG_DIR` and the payload `cwd`.
pub(crate) fn session_resume_notice(hook_event_name: &str, payload: &Value) -> Option<String> {
    resume_notice(hook_event_name, payload, hook_env, || {
        let config_dir = hook_env("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from);
        let cwd = payload["cwd"].as_str().map(std::path::Path::new);
        api_key_helper_set(config_dir.as_deref(), cwd)
    })
}

/// A variable of the hook process. A value that is not UTF-8 counts as set, so it can only keep
/// the notice.
fn hook_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => Some("(not UTF-8)".into()),
    }
}

/// Whether one of the settings files that Claude Code reads for this session sets
/// `apiKeyHelper`, which gives an API key with no variable: the rendered settings in
/// `CLAUDE_CONFIG_DIR`, and the project's `.claude/settings.json` and `.claude/settings.local.json`
/// under `cwd`. A file that exists but cannot be read or parsed counts as set, so it keeps the
/// notice.
fn api_key_helper_set(config_dir: Option<&std::path::Path>, cwd: Option<&std::path::Path>) -> bool {
    let project = cwd.map(|c| c.join(".claude"));
    let files = [
        config_dir.map(|d| d.join("settings.json")),
        project.as_ref().map(|p| p.join("settings.json")),
        project.as_ref().map(|p| p.join("settings.local.json")),
    ];
    files
        .into_iter()
        .flatten()
        .any(|file| match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str::<Value>(&text).map_or(true, |v| {
                v.get("apiKeyHelper").is_some_and(|h| !h.is_null())
            }),
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        })
}

/// The first Claude Code release with its own warning when a resume would use a noticeable share
/// of the 5-hour limit on a Pro or Max plan (changelog entry for 2.1.304, #3103).
const NATIVE_RESUME_WARNING_SINCE: (u64, u64, u64) = (2, 1, 304);

/// The variables that put a session on metered or third-party auth: an API key, a gateway, or a
/// cloud provider. Claude Code gives such a session no resume warning of its own.
const METERED_AUTH_VARS: [&str; 3] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// The provider switches. Each one is on unless it is empty, `0`, or `false`.
const PROVIDER_FLAGS: [&str; 3] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Whether Claude Code shows its own resume warning for this session: subscription auth on the
/// 2.1.304 release or later. Every doubt keeps the llmenv notice, because a second notice costs
/// less than none.
///
/// - Any metered auth variable that is not empty, any provider switch that is on, or an
///   `apiKeyHelper` in the settings means metered auth.
/// - llmenv cannot see the plan, so a Team plan on subscription auth also counts.
/// - A missing or unparseable version, or a pre-release of 2.1.304, keeps the notice.
fn native_warning_covers(
    env: &impl Fn(&str) -> Option<String>,
    api_key_helper: impl FnOnce() -> bool,
) -> bool {
    let metered = METERED_AUTH_VARS
        .iter()
        .any(|name| env(name).is_some_and(|v| !v.is_empty()));
    let provider = PROVIDER_FLAGS
        .iter()
        .any(|name| env(name).is_some_and(|v| !matches!(v.as_str(), "" | "0" | "false")));
    if metered || provider {
        return false;
    }
    let Some(version) = env("CLAUDE_CODE_VERSION") else {
        tracing::debug!("resume notice kept: CLAUDE_CODE_VERSION is not set");
        return false;
    };
    match crate::util::semver::is_release_at_least(&version, NATIVE_RESUME_WARNING_SINCE) {
        Some(newer) => newer && !api_key_helper(),
        None => {
            tracing::warn!("resume notice kept: CLAUDE_CODE_VERSION {version:?} is not a version");
            false
        }
    }
}

/// Add `message` as the system message of `out`. A `None` message leaves `out` unchanged.
pub(crate) fn with_resume_notice(out: String, message: Option<&str>) -> String {
    match message {
        Some(message) => attach_system_message(&out, message),
        None => out,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}

#[cfg(test)]
mod notice_tests;
