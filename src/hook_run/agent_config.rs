//! The per-session agent-config document: what a session runs as.
//!
//! One JSON file per engine session at `state_dir/agent_config/{session_id}.json`.
//! Design: docs/design/issue-2398-session-agent-config.md

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session_log::scope_header::ScopeContext;

const DIR: &str = "agent_config";
const STALE_DAYS: u64 = 7;
/// A person reads this file. A long model history adds noise, so the list keeps the newest entries.
const MAX_HISTORY: usize = 20;
/// The model and effort strings come from hook input and reach the agent's context.
const MAX_FIELD_CHARS: usize = 200;

/// Text from hook input, made safe for one line of agent context: no control or invisible
/// characters, at most `MAX_FIELD_CHARS`, and `None` when nothing is left.
fn clean(text: &str) -> Option<String> {
    let text = crate::util::strip_unsafe_chars(text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(MAX_FIELD_CHARS).collect())
}

/// One `/model` switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelSwitch {
    pub at: i64,
    pub from_model: Option<String>,
    pub to_model: String,
    pub reason: Option<String>,
}

/// What one session runs as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentConfig {
    /// The config form of the engine id, such as `claude_code`.
    pub engine: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: String,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub bundles: Vec<String>,
    pub config_hash: Option<String>,
    pub llmenv_version: String,
    pub engine_version: Option<String>,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub model_history: Vec<ModelSwitch>,
}

/// What a `SessionStart` hook reports about the session.
#[derive(Debug, Clone, Default)]
pub(crate) struct StartFacts<'a> {
    pub engine: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub config_hash: Option<&'a str>,
    pub source: &'a str,
    pub now: i64,
}

impl<'a> StartFacts<'a> {
    /// Read the model, effort level, and source from a `SessionStart` payload.
    pub(crate) fn from_payload(
        payload: &'a serde_json::Value,
        engine: &'a str,
        config_hash: Option<&'a str>,
        now: i64,
    ) -> Self {
        Self {
            engine,
            model: payload["model"].as_str(),
            effort: payload["effort"]["level"].as_str(),
            config_hash,
            source: payload["source"].as_str().unwrap_or("startup"),
            now,
        }
    }
}

impl AgentConfig {
    /// Build the document for a `SessionStart` from the scope and the hook facts.
    pub(crate) fn from_scope_context(ctx: &ScopeContext, facts: &StartFacts<'_>) -> Self {
        let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Self {
            engine: facts.engine.replace('-', "_"),
            model: facts.model.and_then(clean),
            effort: facts.effort.and_then(clean),
            cwd: ctx.cwd.clone(),
            project: ctx.project.clone(),
            tags: ctx.tags.clone(),
            bundles: ctx.bundles.clone(),
            config_hash: facts.config_hash.and_then(non_empty),
            llmenv_version: ctx.llmenv_version.clone(),
            engine_version: non_empty(&ctx.claude_code_version),
            source: clean(facts.source).unwrap_or_default(),
            created_at: facts.now,
            updated_at: facts.now,
            model_history: Vec::new(),
        }
    }

    /// The one line that tells a resumed agent what it runs as.
    fn resume_line(&self) -> String {
        let project = self.project.as_deref().unwrap_or("none");
        let hash = self
            .config_hash
            .as_deref()
            .map_or("unknown", |h| h.get(..12).unwrap_or(h));
        format!(
            "[llmenv session] engine {}, model {}, effort {}, project {project}, tags {}, \
             config {hash}",
            self.engine,
            self.model.as_deref().unwrap_or("unset"),
            self.effort.as_deref().unwrap_or("unset"),
            self.tags.join(", "),
        )
    }

    /// The `running as` text for `task session summary`.
    pub(crate) fn running_as(&self) -> String {
        let model = self.model.as_deref().unwrap_or("unset");
        let effort = self.effort.as_deref().unwrap_or("unset");
        format!("{} {model}, effort {effort}", self.engine)
    }
}

/// The content hash of the booted config folder, when the engine sets one.
pub(crate) fn booted_config_hash() -> Option<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty())?;
    match crate::materialize::manifest::CacheManifest::read(Path::new(&dir)) {
        Ok(manifest) => manifest.map(|m| m.content_hash),
        Err(e) => {
            tracing::warn!("cannot read the booted config manifest, config hash unknown: {e}");
            None
        }
    }
}

/// Write the document for a `SessionStart`. A `resume` or `compact` also returns the resume line.
pub(crate) fn on_session_start(
    state_dir: &Path,
    session_id: &str,
    ctx: &ScopeContext,
    facts: &StartFacts<'_>,
) -> Option<String> {
    let doc = AgentConfig::from_scope_context(ctx, facts);
    let line = matches!(facts.source, "resume" | "compact").then(|| doc.resume_line());
    write_session_start(state_dir, session_id, doc);
    line
}

/// Handle a `PostModelSwitch` payload. A payload without `to_model` changes nothing.
pub(crate) fn record_model_switch(
    state_dir: &Path,
    session_id: &str,
    payload: &serde_json::Value,
    engine: &str,
    now: i64,
) {
    let Some(to_model) = payload["to_model"].as_str().and_then(clean) else {
        tracing::debug!("post_model_switch payload has no to_model, skipped");
        return;
    };
    let text = |key: &str| payload[key].as_str().and_then(clean);
    let switch = ModelSwitch {
        at: now,
        from_model: text("from_model"),
        to_model,
        reason: text("reason"),
    };
    let effort = payload["effort"]["level"].as_str().and_then(clean);
    // A document missing here means the hook arrived mid-session: keep what the payload gives.
    apply_model_switch(state_dir, session_id, switch, effort.clone(), || {
        AgentConfig {
            engine: engine.replace('-', "_"),
            model: None,
            effort,
            cwd: text("cwd").unwrap_or_default(),
            project: None,
            tags: Vec::new(),
            bundles: Vec::new(),
            config_hash: None,
            llmenv_version: env!("CARGO_PKG_VERSION").to_string(),
            engine_version: None,
            source: "post_model_switch".to_string(),
            created_at: now,
            updated_at: now,
            model_history: Vec::new(),
        }
    });
}

/// The file for `session_id`. `None` for an id that is not safe in a path.
fn path(state_dir: &Path, session_id: &str) -> Option<PathBuf> {
    crate::paths::is_valid_short_name(session_id)
        .then(|| state_dir.join(DIR).join(format!("{session_id}.json")))
}

/// Read the document. A missing file gives `None` quietly. A corrupt or unreadable file gives
/// `None` and a warning, because the next write then replaces it.
pub(crate) fn load(state_dir: &Path, session_id: &str) -> Option<AgentConfig> {
    let Some(path) = path(state_dir, session_id) else {
        tracing::debug!("agent config skipped: the session id is not safe in a path");
        return None;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("cannot read agent config {}: {e}", path.display());
            return None;
        }
    };
    serde_json::from_str(&text)
        .inspect_err(|e| tracing::warn!("corrupt agent config {}: {e}", path.display()))
        .ok()
}

/// Replace the document for a `SessionStart`. A restart keeps `created_at` and the model history.
/// A failure is logged and does not stop the hook.
pub(crate) fn write_session_start(state_dir: &Path, session_id: &str, mut doc: AgentConfig) {
    let dir = state_dir.join(DIR);
    let Some(_lock) = super::session_state::lock_state_file(&dir, session_id, "agent config")
    else {
        return;
    };
    if let Some(old) = load(state_dir, session_id) {
        doc.created_at = old.created_at;
        doc.model_history = old.model_history;
    }
    store(state_dir, session_id, &doc);
}

/// Record a `PostModelSwitch`. A missing document is created from the payload alone. `effort`,
/// when the payload has one, replaces the stored level, which belongs to the old model.
fn apply_model_switch(
    state_dir: &Path,
    session_id: &str,
    switch: ModelSwitch,
    effort: Option<String>,
    fallback: impl FnOnce() -> AgentConfig,
) {
    let dir = state_dir.join(DIR);
    let Some(_lock) = super::session_state::lock_state_file(&dir, session_id, "agent config")
    else {
        return;
    };
    let mut doc = load(state_dir, session_id).unwrap_or_else(fallback);
    doc.model = Some(switch.to_model.clone());
    if effort.is_some() {
        doc.effort = effort;
    }
    doc.updated_at = switch.at;
    doc.model_history.push(switch);
    let excess = doc.model_history.len().saturating_sub(MAX_HISTORY);
    doc.model_history.drain(..excess);
    store(state_dir, session_id, &doc);
}

fn store(state_dir: &Path, session_id: &str, doc: &AgentConfig) {
    let Some(path) = path(state_dir, session_id) else {
        return;
    };
    if let Some(dir) = path.parent() {
        super::session_state::prune_stale_json_files(dir, STALE_DAYS);
        super::session_state::prune_orphan_locks(dir, STALE_DAYS);
    }
    let result = serde_json::to_vec_pretty(doc)
        .map_err(std::io::Error::other)
        .and_then(|bytes| crate::paths::write_owner_only_atomic(&path, &bytes));
    if let Err(e) = result {
        tracing::warn!("cannot save agent config {}: {e}", path.display());
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ctx() -> ScopeContext {
        ScopeContext {
            tags: vec!["os-macos".into(), "user-ranger".into()],
            bundles: vec!["base".into()],
            project: Some("llmenv".into()),
            cwd: "/work/llmenv".into(),
            adapter: "claude-code".into(),
            llmenv_version: "3.12.0".into(),
            claude_code_version: "2.1.251".into(),
        }
    }

    fn facts<'a>() -> StartFacts<'a> {
        StartFacts {
            engine: "claude-code",
            model: Some("claude-opus-5"),
            effort: Some("high"),
            config_hash: Some("0123456789abcdef0123"),
            source: "startup",
            now: 100,
        }
    }

    fn doc() -> AgentConfig {
        AgentConfig::from_scope_context(&ctx(), &facts())
    }

    fn switch(at: i64, to: &str) -> ModelSwitch {
        ModelSwitch {
            at,
            from_model: None,
            to_model: to.into(),
            reason: Some("user_requested".into()),
        }
    }

    #[test]
    fn from_scope_context_fills_every_field() {
        let d = doc();
        assert_eq!(d.engine, "claude_code");
        assert_eq!(d.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(d.effort.as_deref(), Some("high"));
        assert_eq!(d.cwd, "/work/llmenv");
        assert_eq!(d.project.as_deref(), Some("llmenv"));
        assert_eq!(d.tags, ["os-macos", "user-ranger"]);
        assert_eq!(d.bundles, ["base"]);
        assert_eq!(d.config_hash.as_deref(), Some("0123456789abcdef0123"));
        assert_eq!(d.llmenv_version, "3.12.0");
        assert_eq!(d.engine_version.as_deref(), Some("2.1.251"));
        assert_eq!(d.source, "startup");
        assert_eq!((d.created_at, d.updated_at), (100, 100));
        assert!(d.model_history.is_empty());
    }

    #[test]
    fn empty_model_and_effort_read_as_missing() {
        let mut f = facts();
        f.model = Some("");
        f.effort = None;
        let d = AgentConfig::from_scope_context(&ctx(), &f);
        assert_eq!((d.model, d.effort), (None, None));
    }

    #[test]
    fn resume_line_renders_every_field() {
        assert_eq!(
            doc().resume_line(),
            "[llmenv session] engine claude_code, model claude-opus-5, effort high, \
             project llmenv, tags os-macos, user-ranger, config 0123456789ab"
        );
    }

    #[test]
    fn resume_line_marks_missing_values() {
        let mut d = doc();
        d.model = None;
        d.effort = None;
        d.project = None;
        d.config_hash = None;
        let line = d.resume_line();
        assert!(
            line.contains("model unset, effort unset, project none"),
            "{line}"
        );
        assert!(line.ends_with("config unknown"), "{line}");
    }

    #[test]
    fn resume_line_keeps_a_short_hash_whole() {
        let mut d = doc();
        d.config_hash = Some("abc".into());
        assert!(d.resume_line().ends_with("config abc"));
    }

    #[test]
    fn running_as_names_engine_model_and_effort() {
        assert_eq!(doc().running_as(), "claude_code claude-opus-5, effort high");
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "sess-1", doc());
        assert_eq!(load(dir.path(), "sess-1"), Some(doc()));
    }

    #[test]
    fn an_unsafe_session_id_writes_nothing_and_loads_none() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "../evil", doc());
        assert!(path(dir.path(), "../evil").is_none());
        assert!(load(dir.path(), "../evil").is_none());
        assert!(!dir.path().join(DIR).exists());
    }

    #[test]
    fn a_corrupt_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "s", doc());
        std::fs::write(path(dir.path(), "s").unwrap(), "{not json").unwrap();
        assert!(load(dir.path(), "s").is_none());
    }

    #[test]
    fn a_restart_keeps_created_at_and_history() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "s", doc());
        apply_model_switch(dir.path(), "s", switch(150, "claude-sonnet-5"), None, doc);
        let mut again = doc();
        again.source = "compact".into();
        again.created_at = 200;
        again.updated_at = 200;
        write_session_start(dir.path(), "s", again);
        let got = load(dir.path(), "s").unwrap();
        assert_eq!((got.created_at, got.updated_at), (100, 200));
        assert_eq!(got.source, "compact");
        assert_eq!(got.model_history.len(), 1);
    }

    #[test]
    fn a_model_switch_updates_the_model_and_history() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "s", doc());
        apply_model_switch(dir.path(), "s", switch(150, "claude-sonnet-5"), None, doc);
        let got = load(dir.path(), "s").unwrap();
        assert_eq!(got.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(got.updated_at, 150);
        assert_eq!(got.model_history, [switch(150, "claude-sonnet-5")]);
    }

    #[test]
    fn a_model_switch_without_a_document_creates_one() {
        let dir = tempfile::tempdir().unwrap();
        apply_model_switch(dir.path(), "s", switch(150, "claude-sonnet-5"), None, doc);
        let got = load(dir.path(), "s").unwrap();
        assert_eq!(got.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(got.tags, ["os-macos", "user-ranger"]);
    }

    #[test]
    fn the_model_history_keeps_the_newest_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_HISTORY + 5) {
            let at = i64::try_from(i).unwrap();
            apply_model_switch(dir.path(), "s", switch(at, &format!("m{i}")), None, doc);
        }
        let got = load(dir.path(), "s").unwrap();
        assert_eq!(got.model_history.len(), MAX_HISTORY);
        assert_eq!(got.model_history[0].to_model, "m5");
        assert_eq!(got.model.as_deref(), Some("m24"));
    }

    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "s", doc());
        let mode = std::fs::metadata(path(dir.path(), "s").unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn session_start_gives_the_resume_line_only_for_resume_and_compact() {
        for (source, want) in [
            ("startup", false),
            ("clear", false),
            ("fork", false),
            ("resume", true),
            ("compact", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut f = facts();
            f.source = source;
            let line = on_session_start(dir.path(), "s", &ctx(), &f);
            assert_eq!(line.is_some(), want, "{source}");
            assert_eq!(load(dir.path(), "s").unwrap().source, source);
            if let Some(line) = line {
                assert!(
                    line.starts_with("[llmenv session] engine claude_code"),
                    "{line}"
                );
            }
        }
    }

    #[test]
    fn facts_come_from_the_session_start_payload() {
        let payload = serde_json::json!({
            "model": "claude-opus-5",
            "effort": { "level": "high" },
            "source": "resume",
        });
        let f = StartFacts::from_payload(&payload, "claude-code", Some("h"), 7);
        assert_eq!(f.model, Some("claude-opus-5"));
        assert_eq!(f.effort, Some("high"));
        assert_eq!((f.source, f.config_hash, f.now), ("resume", Some("h"), 7));
        let empty = serde_json::json!({});
        let bare = StartFacts::from_payload(&empty, "claude-code", None, 7);
        assert_eq!(
            (bare.model, bare.effort, bare.source),
            (None, None, "startup")
        );
    }

    #[test]
    fn a_model_switch_payload_updates_the_document() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "s", doc());
        let payload = serde_json::json!({
            "from_model": "claude-opus-5",
            "to_model": "claude-sonnet-5",
            "reason": "user_requested",
        });
        record_model_switch(dir.path(), "s", &payload, "claude-code", 300);
        let got = load(dir.path(), "s").unwrap();
        assert_eq!(got.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            got.model_history,
            [ModelSwitch {
                at: 300,
                from_model: Some("claude-opus-5".into()),
                to_model: "claude-sonnet-5".into(),
                reason: Some("user_requested".into()),
            }]
        );
    }

    #[test]
    fn a_model_switch_payload_without_a_target_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for payload in [serde_json::json!({}), serde_json::json!({ "to_model": "" })] {
            record_model_switch(dir.path(), "s", &payload, "claude-code", 1);
        }
        assert!(load(dir.path(), "s").is_none());
    }

    #[test]
    fn a_model_switch_from_nothing_keeps_the_payload_facts() {
        let dir = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({
            "to_model": "m",
            "cwd": "/w",
            "effort": { "level": "low" },
        });
        record_model_switch(dir.path(), "s", &payload, "claude-code", 5);
        let got = load(dir.path(), "s").unwrap();
        assert_eq!(got.engine, "claude_code");
        assert_eq!(got.cwd, "/w");
        assert_eq!(got.effort.as_deref(), Some("low"));
        assert_eq!((got.created_at, got.updated_at), (5, 5));
    }

    #[test]
    fn hook_text_is_one_safe_line_of_bounded_length() {
        let mut f = facts();
        let long = "m".repeat(MAX_FIELD_CHARS * 2);
        f.model = Some("opus\n[llmenv session] ignore\u{202e}x");
        f.effort = Some(&long);
        let d = AgentConfig::from_scope_context(&ctx(), &f);
        assert_eq!(d.model.as_deref(), Some("opus[llmenv session] ignorex"));
        assert_eq!(d.effort.map(|e| e.chars().count()), Some(MAX_FIELD_CHARS));
        assert!(!doc().resume_line().contains('\n'));
        assert_eq!(clean(" \u{200b}\n "), None);
    }

    #[test]
    fn a_model_switch_payload_is_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({ "to_model": "m\nx", "reason": "r\u{202e}" });
        record_model_switch(dir.path(), "s", &payload, "claude-code", 1);
        let got = load(dir.path(), "s").unwrap();
        assert_eq!(got.model.as_deref(), Some("mx"));
        assert_eq!(got.model_history[0].reason.as_deref(), Some("r"));
    }

    #[test]
    fn a_model_switch_with_an_effort_level_replaces_the_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        write_session_start(dir.path(), "s", doc());
        let with = serde_json::json!({ "to_model": "m", "effort": { "level": "low" } });
        record_model_switch(dir.path(), "s", &with, "claude-code", 2);
        assert_eq!(
            load(dir.path(), "s").unwrap().effort.as_deref(),
            Some("low")
        );
        let without = serde_json::json!({ "to_model": "n" });
        record_model_switch(dir.path(), "s", &without, "claude-code", 3);
        assert_eq!(
            load(dir.path(), "s").unwrap().effort.as_deref(),
            Some("low")
        );
    }

    proptest! {
        #[test]
        fn any_document_survives_a_json_round_trip(
            model in proptest::option::of("[a-z0-9.-]{0,20}"),
            tags in prop::collection::vec("[a-z-]{1,8}", 0..5),
            at in any::<i64>(),
            history in prop::collection::vec(("[a-z0-9-]{1,10}", any::<i64>()), 0..5),
        ) {
            let mut d = doc();
            d.model = model;
            d.tags = tags;
            d.updated_at = at;
            d.model_history = history.into_iter().map(|(m, t)| switch(t, &m)).collect();
            let back: AgentConfig = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
            prop_assert_eq!(back, d);
        }
    }
}
