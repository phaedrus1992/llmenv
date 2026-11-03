//! Per-session recall ledger for adaptive ICM recall (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) use super::record_hash::{hash_prefix, record_hash};
use super::resume_cache::ResumeCache;
use serde::{Deserialize, Serialize};

/// The agent key for the parent conversation.
pub(crate) const MAIN_AGENT: &str = "main";
const ACTIVITY_CAP: usize = 20;
const ERRORS_CAP: usize = 5;
const PENDING_CAP: usize = 8;
/// Every subagent adds an entry; an evicted one can only cause a repeat to that subagent.
const MAX_AGENTS: usize = 32;
/// A queued task that no `SubagentStart` took within this time is stale.
const PENDING_TTL_SECS: i64 = 300;
const ERROR_HEAD_BYTES: usize = 300;
const TASK_HEAD_CHARS: usize = 600;
const STALE_DAYS: u64 = 7;

/// One injected-record set per model context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct AgentState {
    sent: BTreeSet<String>,
    scope_sent: bool,
}

/// One tool call, reduced to the part that carries a topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Activity {
    pub(crate) tool: String,
    pub(crate) target: Option<String>,
    pub(crate) at: i64,
}

/// One failed tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolError {
    pub(crate) tool: String,
    pub(crate) head: String,
    pub(crate) at: i64,
}

/// A subagent task seen on the `Agent` tool call, waiting for its `SubagentStart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingSubagent {
    tool_use_id: String,
    subagent_type: String,
    pub(crate) task: String,
    at: i64,
}

/// The recall state of one Claude Code session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Ledger {
    epoch: u64,
    agents: BTreeMap<String, AgentState>,
    activity: VecDeque<Activity>,
    /// The cache fields of the latest `SessionStart`, for the cache verdict (#2384).
    pub(crate) resume_cache: Option<ResumeCache>,
    errors: VecDeque<ToolError>,
    pub(crate) last_query_hash: Option<String>,
    pub(crate) last_turn_at: i64,
    pending_subagents: VecDeque<PendingSubagent>,
    /// The session's project name, found once at `SessionStart` (#2249).
    pub(crate) project: Option<String>,
}

impl Ledger {
    /// Start a new epoch: every context forgets what it was sent. Activity stays.
    pub(crate) fn reset(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.agents.clear();
    }

    /// The record hashes already injected into `agent`.
    pub(crate) fn sent_for(&self, agent: &str) -> BTreeSet<String> {
        self.agents
            .get(agent)
            .map(|a| a.sent.clone())
            .unwrap_or_default()
    }

    pub(crate) fn mark_sent(&mut self, agent: &str, hashes: impl IntoIterator<Item = String>) {
        self.agent_mut(agent).sent.extend(hashes);
    }

    /// The entry for `agent`, created if missing. A new entry past
    /// [`MAX_AGENTS`] evicts one other subagent entry, never `main`.
    fn agent_mut(&mut self, agent: &str) -> &mut AgentState {
        if !self.agents.contains_key(agent) && self.agents.len() >= MAX_AGENTS {
            let victim = self.agents.keys().find(|k| *k != MAIN_AGENT).cloned();
            if let Some(victim) = victim {
                self.agents.remove(&victim);
            }
        }
        self.agents.entry(agent.to_string()).or_default()
    }

    pub(crate) fn scope_sent(&self, agent: &str) -> bool {
        self.agents.get(agent).is_some_and(|a| a.scope_sent)
    }

    pub(crate) fn set_scope_sent(&mut self, agent: &str) {
        self.agent_mut(agent).scope_sent = true;
    }

    pub(crate) fn activity(&self) -> &VecDeque<Activity> {
        &self.activity
    }

    pub(crate) fn errors(&self) -> &VecDeque<ToolError> {
        &self.errors
    }

    /// Whether any activity arrived after `at`.
    pub(crate) fn activity_since(&self, at: i64) -> bool {
        self.activity.back().is_some_and(|a| a.at > at)
    }

    pub(crate) fn push_activity(&mut self, entry: Activity) {
        push_capped(&mut self.activity, entry, ACTIVITY_CAP);
    }

    pub(crate) fn push_error(&mut self, tool: &str, error: &str, now: i64) {
        let entry = ToolError {
            tool: tool.to_string(),
            head: head_bytes(error, ERROR_HEAD_BYTES),
            at: now,
        };
        push_capped(&mut self.errors, entry, ERRORS_CAP);
    }

    /// Queue a subagent task. A repeat of the same `tool_use_id` is ignored.
    pub(crate) fn queue_subagent(
        &mut self,
        tool_use_id: &str,
        subagent_type: &str,
        prompt: &str,
        now: i64,
    ) {
        self.drop_expired(now);
        if self
            .pending_subagents
            .iter()
            .any(|p| p.tool_use_id == tool_use_id)
        {
            return;
        }
        let entry = PendingSubagent {
            tool_use_id: tool_use_id.to_string(),
            subagent_type: subagent_type.to_string(),
            task: prompt.chars().take(TASK_HEAD_CHARS).collect(),
            at: now,
        };
        push_capped(&mut self.pending_subagents, entry, PENDING_CAP);
    }

    /// Remove and return the oldest queued task for `agent_type`.
    pub(crate) fn take_subagent(&mut self, agent_type: &str, now: i64) -> Option<PendingSubagent> {
        self.drop_expired(now);
        let index = self
            .pending_subagents
            .iter()
            .position(|p| p.subagent_type == agent_type)?;
        self.pending_subagents.remove(index)
    }

    /// Whether `agent` already has an entry, which means a subagent resumed.
    pub(crate) fn knows_agent(&self, agent: &str) -> bool {
        self.agents.contains_key(agent)
    }

    /// Remove the queued task of one `Agent` call, when that call has finished
    /// or was denied and so will never reach a `SubagentStart`.
    pub(crate) fn drop_subagent(&mut self, tool_use_id: &str) {
        self.pending_subagents
            .retain(|p| p.tool_use_id != tool_use_id);
    }

    fn drop_expired(&mut self, now: i64) {
        self.pending_subagents
            .retain(|p| now.saturating_sub(p.at) <= PENDING_TTL_SECS);
    }
}

fn push_capped<T>(ring: &mut VecDeque<T>, entry: T, cap: usize) {
    ring.push_back(entry);
    let excess = ring.len().saturating_sub(cap);
    ring.drain(..excess);
}

/// The longest prefix of `text` that fits in `max` bytes and ends on a char boundary.
fn head_bytes(text: &str, max: usize) -> String {
    text.get(..text.floor_char_boundary(max))
        .unwrap_or_default()
        .to_string()
}

/// Timing data for a call to `LedgerStore::update_reporting_save` (#3012).
///
/// A field is `None` when its step did not run, such as `read_us` after a busy lock.
#[derive(Debug, Clone, Default)]
pub(crate) struct LedgerTiming {
    /// Polls of the lock (`1` = no contention).
    pub lock_attempts: Option<u32>,
    /// Time spent polling for the lock, in microseconds.
    pub lock_wait_us: Option<u64>,
    /// Time to read the ledger file, in microseconds.
    pub read_us: Option<u64>,
    /// Time to write the ledger file, in microseconds.
    pub write_us: Option<u64>,
    /// Size of the ledger after serialization, when a trace sink is on.
    pub ledger_bytes: Option<usize>,
    /// Whether the change did not reach disk: a busy lock, an unreadable file, or a failed save.
    pub state_unsaved: Option<bool>,
}

/// The ledger files under `state_dir()/recall_session/`.
#[derive(Debug, Clone)]
pub(crate) struct LedgerStore {
    dir: PathBuf,
}

impl LedgerStore {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("recall_session"),
        }
    }

    /// Read the ledger. `None` for an unsafe `session_id`, a busy lock, or a file that exists
    /// but cannot be read. An empty default would make the caller forget what the session
    /// already received and repeat it (#2711).
    pub(crate) fn load(&self, session_id: &str) -> Option<Ledger> {
        let _lock = self.lock(session_id)?;
        self.read(session_id)
    }

    /// Apply `f` under the lock and save. `None` for an unsafe `session_id`, a
    /// busy lock, or a file that exists but cannot be read: a write then would
    /// replace good state with an empty ledger. A failed save is logged; the
    /// result of `f` is still returned.
    pub(crate) fn update<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut Ledger) -> T,
    ) -> Option<T> {
        let _lock = self.lock(session_id)?;
        let mut ledger = self.read(session_id)?;
        let result = f(&mut ledger);
        let _saved = self.write(session_id, &ledger);
        Some(result)
    }

    /// [`Self::update`] with the lock and file timing, for the recall trace (#3012).
    pub(crate) fn update_timed<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut Ledger) -> T,
    ) -> (Option<T>, LedgerTiming) {
        let (updated, timing) = self.update_reporting_save(session_id, f);
        (updated.map(|(result, _saved)| result), timing)
    }

    /// Like [`Self::update`], and also reports whether the ledger reached disk.
    /// The flag is `false` when the save failed, and the result of `f` is still returned.
    /// The timing comes back even when the update does not run (#3012).
    pub(crate) fn update_reporting_save<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut Ledger) -> T,
    ) -> (Option<(T, bool)>, LedgerTiming) {
        let us = |t: Instant| u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX);
        let (lock, lock_timing) =
            super::session_state::lock_state_file_timed(&self.dir, session_id, "recall ledger");
        let mut timing = LedgerTiming {
            lock_attempts: Some(lock_timing.attempts),
            lock_wait_us: Some(lock_timing.wait_us),
            state_unsaved: Some(true),
            ..LedgerTiming::default()
        };
        let _lock = match lock {
            Ok(file) => file,
            // The lock helper already recorded the cause for the agent.
            Err(_) => return (None, timing),
        };
        let t_read = Instant::now();
        let Some(mut ledger) = self.read(session_id) else {
            return (None, timing);
        };
        timing.read_us = Some(us(t_read));
        let result = f(&mut ledger);
        let t_write = Instant::now();
        let saved = self.write(session_id, &ledger);
        timing.write_us = Some(us(t_write));
        timing.state_unsaved = Some(!saved);
        if crate::trace::enabled() {
            timing.ledger_bytes = serde_json::to_vec(&ledger).ok().map(|v| v.len());
        }
        (Some((result, saved)), timing)
    }

    fn file(&self, session_id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.{ext}"))
    }

    fn lock(&self, session_id: &str) -> Option<File> {
        super::session_state::lock_state_file(&self.dir, session_id, "recall ledger")
    }

    /// The stored ledger: a default for a missing or corrupt file, `None` for a
    /// file that exists but cannot be read.
    fn read(&self, session_id: &str) -> Option<Ledger> {
        let path = self.file(session_id, "json");
        match crate::paths::bounded::state_text_io(&path) {
            Ok(text) => Some(serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::error!(
                    "recall ledger {} is corrupt, starting a fresh epoch: {e}",
                    path.display()
                );
                crate::agent_notice::record(
                    "llmenv: the recall ledger is corrupt, so it started fresh. Records already \
                     sent may repeat. Fix: delete the ledger file or run `llmenv doctor` to check \
                     the state directory.",
                );
                Ledger::default()
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Ledger::default()),
            Err(e) => {
                tracing::error!(
                    "cannot read recall ledger {}, update skipped: {e}",
                    path.display()
                );
                None
            }
        }
    }

    /// Returns `false` when the save failed. The failure is already logged and noticed.
    fn write(&self, session_id: &str, ledger: &Ledger) -> bool {
        super::session_state::prune_stale_json_files(&self.dir, STALE_DAYS);
        super::session_state::prune_orphan_locks(&self.dir, STALE_DAYS);
        let path = self.file(session_id, "json");
        let result = serde_json::to_vec(ledger)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::paths::write_owner_only_atomic(&path, &bytes));
        match result {
            Ok(()) => true,
            Err(e) => {
                note_save_failure(&path, &e);
                false
            }
        }
    }
}

/// Logs a failed ledger save and tells the agent. `update` still returns the result of its
/// closure, so the caller cannot see this failure (#2729).
fn note_save_failure(path: &std::path::Path, err: &std::io::Error) {
    crate::agent_notice::record_logged(format!(
        "llmenv: the recall ledger {} could not be saved ({err}), so some records may repeat. \
         Fix: run `llmenv doctor` to check the state directory.",
        path.display()
    ));
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
#[cfg(test)]
#[allow(clippy::expect_used, clippy::expect_fun_call)]
mod corrupt_notice_tests {
    use super::{LedgerStore, MAIN_AGENT};

    #[test]
    fn a_corrupt_ledger_tells_the_agent_that_records_may_repeat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LedgerStore::new(dir.path());
        store.update("s1", |l| l.mark_sent(MAIN_AGENT, ["keep".to_string()]));
        let path = dir.path().join("recall_session").join("s1.json");
        std::fs::write(&path, "not json").expect("corrupt the ledger");
        assert!(
            store.load("s1").is_some(),
            "a corrupt ledger still loads as a fresh one"
        );
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(text.contains("recall ledger is corrupt"), "got: {text}");
    }

    #[test]
    fn a_failed_save_tells_the_agent() {
        // The atomic writer rebuilds the directory, so a real save failure cannot be forced here.
        super::note_save_failure(
            std::path::Path::new("/state/recall_session/s1.json"),
            &std::io::Error::other("disk full"),
        );
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(
            text.contains("recall ledger /state/recall_session/s1.json could not be saved"),
            "got: {text}"
        );
    }
}
