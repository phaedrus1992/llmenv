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

/// A settings file larger than this is not a real one. A repo controls the project files.
const SETTINGS_MAX_BYTES: u64 = 1 << 20;

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
    files.into_iter().flatten().any(|file| {
        match llmenv_paths::read_text_bounded(&file, SETTINGS_MAX_BYTES) {
            Ok(Some(text)) => serde_json::from_str::<Value>(&text).map_or(true, |v| {
                v.get("apiKeyHelper").is_some_and(|h| !h.is_null())
            }),
            Ok(None) => false,
            // A repo can plant a pipe, a link, or a huge file here. The notice stays on.
            Err(e) => {
                tracing::warn!(
                    "cannot read {}: {e}, so the helper counts as set",
                    file.display()
                );
                true
            }
        }
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
    let Some(version) = env("CLAUDE_CODE_VERSION").filter(|v| !v.is_empty()) else {
        tracing::debug!("resume notice kept: CLAUDE_CODE_VERSION is not set or empty");
        return false;
    };
    match crate::util::semver::is_release_at_least(&version, NATIVE_RESUME_WARNING_SINCE) {
        Some(newer) => newer && !api_key_helper(),
        None => {
            // The notice stays on, so the user sees that the version check failed. The engine
            // drops hook stderr, so only the agent notice reaches them (#3506).
            crate::agent_notice::record_logged(format!(
                "llmenv: the resume cost notice stays on, because CLAUDE_CODE_VERSION is \
                 not a version ({version:?}). Fix: report the version string to llmenv."
            ));
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
mod tests;

#[cfg(test)]
mod notice_tests;
