//! Per-session recall ledger for adaptive ICM recall (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
/// A hook must not stall the agent on a busy ledger, so a write gives up after this wait.
const LOCK_WAIT: Duration = Duration::from_millis(200);
const LOCK_POLL: Duration = Duration::from_millis(10);
const STALE_DAYS: u64 = 7;
/// Hex characters kept from the SHA-256 digest; 64 bits is enough to key one session.
const HASH_HEX_CHARS: usize = 16;

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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Ledger {
    epoch: u64,
    agents: BTreeMap<String, AgentState>,
    activity: VecDeque<Activity>,
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
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// The longest prefix of `text` that fits in `max` bytes and ends on a char boundary.
fn head_bytes(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default().to_string()
}

/// A stable key for one recall record: whitespace layout does not change it.
pub(crate) fn record_hash(record: &str) -> String {
    let normalized = record.split_whitespace().collect::<Vec<_>>().join(" ");
    Sha256::digest(normalized.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(HASH_HEX_CHARS)
        .collect()
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
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

    /// Read the ledger. `None` only for an unsafe `session_id` or a busy lock.
    /// An unreadable file reads as empty here, because nothing is written back.
    pub(crate) fn load(&self, session_id: &str) -> Option<Ledger> {
        let _lock = self.lock(session_id)?;
        Some(self.read(session_id).unwrap_or_default())
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
        self.write(session_id, &ledger);
        Some(result)
    }

    fn file(&self, session_id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.{ext}"))
    }

    fn lock(&self, session_id: &str) -> Option<File> {
        // The id comes from hook stdin; an unsafe value must never reach a path join.
        if !crate::paths::is_valid_short_name(session_id) {
            tracing::error!("session_id failed path-safety validation for recall ledger");
            return None;
        }
        if let Err(e) = crate::paths::create_dir_owner_only(&self.dir) {
            tracing::error!("cannot create {}: {e}", self.dir.display());
            return None;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.file(session_id, "lock"))
            .inspect_err(|e| tracing::error!("cannot open recall ledger lock: {e}"))
            .ok()?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => {
                    // The orphan prune goes by age, so a held lock must look new.
                    if let Err(e) = file.set_modified(std::time::SystemTime::now()) {
                        tracing::warn!("cannot refresh recall ledger lock age: {e}");
                    }
                    return Some(file);
                }
                Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(LOCK_POLL);
                }
                Err(e) => {
                    tracing::warn!("recall ledger busy or unlockable, access skipped: {e}");
                    return None;
                }
            }
        }
    }

    /// The stored ledger: a default for a missing or corrupt file, `None` for a
    /// file that exists but cannot be read.
    fn read(&self, session_id: &str) -> Option<Ledger> {
        let path = self.file(session_id, "json");
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::error!(
                    "recall ledger {} is corrupt, starting a fresh epoch: {e}",
                    path.display()
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

    fn write(&self, session_id: &str, ledger: &Ledger) {
        super::session_state::prune_stale_json_files(&self.dir, STALE_DAYS);
        self.prune_orphan_locks();
        let path = self.file(session_id, "json");
        let result = serde_json::to_vec(ledger)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::paths::write_owner_only_atomic(&path, &bytes));
        if let Err(e) = result {
            tracing::error!("cannot save recall ledger {}: {e}", path.display());
        }
    }

    /// Remove stale `.lock` files whose `.json` is gone. A session in its first
    /// update holds a lock with no `.json` yet; a removal of that lock lets a
    /// second writer lock a new file and lose an update, so only old locks go.
    fn prune_orphan_locks(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let max_age = Duration::from_secs(STALE_DAYS * 86_400);
        for path in entries.flatten().map(|e| e.path()) {
            let stale = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > max_age);
            if stale
                && path.extension().and_then(|e| e.to_str()) == Some("lock")
                && !path.with_extension("json").exists()
            {
                let _ignored = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, LedgerStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        (dir, store)
    }

    use proptest::prelude::*;

    proptest! {
        /// Any whitespace layout of the same words gives the same 16-hex-char hash.
        #[test]
        fn record_hash_depends_only_on_the_words(
            words in prop::collection::vec("[a-z0-9\\[\\]]{1,8}", 1..12),
            seps in prop::collection::vec("[ \t\n]{1,3}", 12),
        ) {
            let spaced: String = words
                .iter()
                .zip(seps.iter())
                .map(|(w, s)| format!("{w}{s}"))
                .collect();
            let single = words.join(" ");
            prop_assert_eq!(record_hash(&spaced), record_hash(&single));
            prop_assert_eq!(record_hash(&single).len(), HASH_HEX_CHARS);
            prop_assert!(record_hash(&single).chars().all(|c| c.is_ascii_hexdigit()));
        }

        /// The head is a prefix, fits the limit, and is the whole text when it fits.
        #[test]
        fn head_bytes_is_a_bounded_prefix(text in "\\PC{0,200}", max in 0usize..400) {
            let head = head_bytes(&text, max);
            prop_assert!(text.starts_with(&head));
            prop_assert!(head.len() <= max);
            if text.len() <= max {
                prop_assert_eq!(head, text);
            }
        }

        /// Whatever the operations, the ledger survives a JSON round trip unchanged.
        #[test]
        fn ledger_round_trips_through_json(
            hashes in prop::collection::vec("[0-9a-f]{16}", 0..10),
            tools in prop::collection::vec("[A-Za-z]{1,10}", 0..30),
            prompt in "\\PC{0,700}",
            error in "\\PC{0,500}",
        ) {
            let mut l = Ledger::default();
            l.reset();
            l.mark_sent(MAIN_AGENT, hashes.clone());
            l.mark_sent("agent-1", hashes);
            for (i, tool) in tools.iter().enumerate() {
                let at = i64::try_from(i).unwrap_or(0);
                l.push_activity(Activity { tool: tool.clone(), target: Some(prompt.clone()), at });
                l.push_error(tool, &error, at);
                l.queue_subagent(&format!("u{i}"), tool, &prompt, at);
            }
            l.last_query_hash = Some("abc".into());
            let json = serde_json::to_string(&l).unwrap();
            let back: Ledger = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, l);
        }
    }

    #[test]
    fn record_hash_ignores_whitespace_layout() {
        assert_eq!(record_hash("[t] a  b\n c"), record_hash("[t] a b c"));
        assert_ne!(record_hash("[t] a"), record_hash("[u] a"));
    }

    #[test]
    fn missing_ledger_loads_as_default() {
        let (_dir, store) = store();
        assert_eq!(store.load("s1"), Some(Ledger::default()));
    }

    #[test]
    fn corrupt_ledger_loads_as_default() {
        let (dir, store) = store();
        let path = dir.path().join("recall_session").join("s1.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"epoch\": 3, \"agents\": ").unwrap();
        assert_eq!(store.load("s1"), Some(Ledger::default()));
    }

    #[test]
    fn an_unreadable_ledger_is_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store) = store();
        store.update("s1", |l| l.mark_sent(MAIN_AGENT, ["keep".to_string()]));
        let path = dir.path().join("recall_session").join("s1.json");
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = store.update("s1", |l| l.mark_sent(MAIN_AGENT, ["new".to_string()]));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(result, None, "an unreadable ledger must skip the write");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn taking_the_lock_refreshes_its_age() {
        let (dir, store) = store();
        store.update("s1", |_| ());
        let lock = dir.path().join("recall_session").join("s1.lock");
        let ten_days = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
        filetime::set_file_mtime(&lock, filetime::FileTime::from_system_time(ten_days)).unwrap();
        store.load("s1").unwrap();
        let age = std::fs::metadata(&lock)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(age < std::time::Duration::from_secs(60), "{age:?}");
    }

    #[test]
    fn invalid_session_id_is_refused() {
        let (dir, store) = store();
        for id in ["../x", "/abs", "..", "", "a/b"] {
            assert_eq!(store.load(id), None, "{id}");
            assert_eq!(store.update(id, |_| ()), None, "{id}");
        }
        assert!(!dir.path().join("x.json").exists());
    }

    #[test]
    fn update_persists_and_reset_clears_agents_only() {
        let (_dir, store) = store();
        store.update("s1", |l| {
            l.mark_sent(MAIN_AGENT, ["h1".to_string()]);
            l.set_scope_sent(MAIN_AGENT);
            l.push_activity(Activity {
                tool: "Read".into(),
                target: None,
                at: 1,
            });
        });
        let before = store.load("s1").unwrap();
        assert!(before.sent_for(MAIN_AGENT).contains("h1"));
        assert!(before.scope_sent(MAIN_AGENT));
        store.update("s1", Ledger::reset);
        let after = store.load("s1").unwrap();
        assert_eq!(after.epoch, before.epoch + 1);
        assert!(after.sent_for(MAIN_AGENT).is_empty());
        assert!(!after.scope_sent(MAIN_AGENT));
        assert_eq!(after.activity().len(), 1, "activity survives a reset");
    }

    #[test]
    fn the_agent_map_is_capped_and_keeps_main() {
        let mut l = Ledger::default();
        l.mark_sent(MAIN_AGENT, ["m".to_string()]);
        for i in 0..(MAX_AGENTS * 2) {
            l.mark_sent(&format!("agent-{i:03}"), ["h".to_string()]);
        }
        assert!(l.agents.len() <= MAX_AGENTS, "{}", l.agents.len());
        assert!(l.sent_for(MAIN_AGENT).contains("m"));
        assert!(
            l.sent_for(&format!("agent-{:03}", MAX_AGENTS * 2 - 1))
                .contains("h")
        );
    }

    #[test]
    fn sent_sets_are_kept_per_agent() {
        let mut l = Ledger::default();
        l.mark_sent("agent-1", ["h".to_string()]);
        assert!(l.sent_for("agent-1").contains("h"));
        assert!(l.sent_for(MAIN_AGENT).is_empty());
    }

    #[test]
    fn rings_keep_only_the_newest_entries() {
        let mut l = Ledger::default();
        for i in 0..25 {
            l.push_activity(Activity {
                tool: format!("t{i}"),
                target: None,
                at: i,
            });
            l.push_error("Bash", &format!("Exit code 1\nerr {i}"), i);
        }
        assert_eq!(l.activity().len(), ACTIVITY_CAP);
        assert_eq!(l.activity().front().unwrap().tool, "t5");
        assert_eq!(l.errors().len(), ERRORS_CAP);
        assert_eq!(l.errors().back().unwrap().head, "Exit code 1\nerr 24");
    }

    #[test]
    fn error_head_is_capped_on_a_char_boundary() {
        let mut l = Ledger::default();
        l.push_error("Bash", &"é".repeat(400), 1);
        let head = &l.errors().back().unwrap().head;
        assert!(head.len() <= ERROR_HEAD_BYTES);
        assert!(!head.is_empty());
        assert!(head.chars().all(|c| c == 'é'));
    }

    #[test]
    fn subagent_queue_dedups_by_tool_use_id_and_takes_oldest_match() {
        let mut l = Ledger::default();
        l.queue_subagent("u1", "Explore", "find the parser", 100);
        l.queue_subagent("u1", "Explore", "find the parser", 101);
        l.queue_subagent("u2", "Plan", "plan it", 102);
        l.queue_subagent("u3", "Explore", "second task", 103);
        assert_eq!(
            l.take_subagent("Explore", 110).unwrap().task,
            "find the parser"
        );
        assert_eq!(l.take_subagent("Explore", 110).unwrap().task, "second task");
        assert!(l.take_subagent("Explore", 110).is_none());
        assert!(
            l.take_subagent("Plan", 102 + PENDING_TTL_SECS + 5)
                .is_none(),
            "expired"
        );
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let (_dir, store) = store();
        let store = std::sync::Arc::new(store);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    store.update("s1", |l| l.mark_sent(MAIN_AGENT, [format!("h{i}")]))
                })
            })
            .collect();
        let written = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .count();
        let ledger = store.load("s1").unwrap();
        assert_eq!(ledger.sent_for(MAIN_AGENT).len(), written);
        assert!(written >= 1);
    }

    #[test]
    fn only_stale_orphan_lock_files_are_removed_on_write() {
        let (dir, store) = store();
        let old = dir.path().join("recall_session").join("gone.lock");
        let fresh = dir.path().join("recall_session").join("starting.lock");
        store.update("s1", |_| ());
        std::fs::write(&old, "").unwrap();
        std::fs::write(&fresh, "").unwrap();
        let ten_days = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
        filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(ten_days)).unwrap();
        store.update("s1", |_| ());
        assert!(!old.exists(), "a stale orphan lock is removed");
        assert!(
            fresh.exists(),
            "a session in its first update keeps its lock"
        );
        assert!(dir.path().join("recall_session").join("s1.lock").exists());
    }
}
