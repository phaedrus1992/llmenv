use super::*;
use crate::hook_run::record_hash::HASH_HEX_CHARS;

pub(super) fn store() -> (tempfile::TempDir, LedgerStore) {
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
fn load_gives_no_ledger_for_an_unreadable_file() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store) = store();
    store.update("s1", |l| l.mark_sent(MAIN_AGENT, ["keep".to_string()]));
    let path = dir.path().join("recall_session").join("s1.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let loaded = store.load("s1");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    // An empty default would make the caller forget "keep" and send it again.
    assert_eq!(loaded, None, "an unreadable ledger must not read as empty");
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
fn a_full_agent_map_evicts_only_for_a_new_agent() {
    let mut l = Ledger::default();
    for i in 0..MAX_AGENTS {
        l.mark_sent(&format!("agent-{i:03}"), ["h".to_string()]);
    }
    assert_eq!(l.agents.len(), MAX_AGENTS, "no eviction below the cap");
    l.mark_sent("agent-000", ["h2".to_string()]);
    assert_eq!(l.agents.len(), MAX_AGENTS);
    assert!(
        (0..MAX_AGENTS).all(|i| l.knows_agent(&format!("agent-{i:03}"))),
        "an existing agent's write evicts nobody"
    );
}

#[test]
fn activity_since_needs_strictly_newer_activity() {
    let mut l = Ledger::default();
    assert!(!l.activity_since(0), "no activity");
    l.push_activity(Activity {
        tool: "Read".to_string(),
        target: None,
        at: 100,
    });
    assert!(l.activity_since(99));
    assert!(!l.activity_since(100), "same second is not newer");
    assert!(!l.activity_since(101));
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
fn a_lock_released_during_the_wait_is_taken() {
    let (_dir, store) = store();
    let held = store.lock("s1").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(held);
    });
    assert!(store.update("s1", |l| l.last_turn_at = 1).is_some());
    release.join().unwrap();
}

#[test]
fn a_lock_held_past_the_wait_skips_the_update() {
    let (_dir, store) = store();
    let held = store.lock("s1").unwrap();
    assert!(store.update("s1", |l| l.last_turn_at = 1).is_none());
    drop(held);
    assert_eq!(store.load("s1").unwrap().last_turn_at, 0);
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
fn orphan_locks_are_kept_on_write_when_only_their_mtime_is_old() {
    let (dir, store) = store();
    let old = dir.path().join("recall_session").join("gone.lock");
    let fresh = dir.path().join("recall_session").join("starting.lock");
    let recent = dir.path().join("recall_session").join("recent.lock");
    store.update("s1", |_| ());
    std::fs::write(&old, "").unwrap();
    std::fs::write(&fresh, "").unwrap();
    std::fs::write(&recent, "").unwrap();
    let days_ago = |days: u64| {
        let at = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        filetime::FileTime::from_system_time(at)
    };
    filetime::set_file_mtime(&old, days_ago(10)).unwrap();
    filetime::set_file_mtime(&recent, days_ago(3)).unwrap();
    store.update("s1", |_| ());
    // A restore sets the change time to now, so the production rule keeps the lock (#3275).
    assert!(
        old.exists(),
        "an orphan lock restored with an old mtime was removed"
    );
    assert!(recent.exists(), "an orphan lock younger than 7 days stays");
    assert!(
        fresh.exists(),
        "a session in its first update keeps its lock"
    );
    assert!(dir.path().join("recall_session").join("s1.lock").exists());
}

#[cfg(unix)]
#[test]
fn a_busy_recall_ledger_lock_is_an_agent_notice() {
    let (dir, store) = store();
    let _held =
        crate::hook_run::session_state::lock_state_file(&store.dir, "s1", "test holder").unwrap();
    assert!(store.lock("s1").is_none());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("the recall ledger lock"), "{notices}");
    drop(dir);
}

#[expect(clippy::unwrap_used, reason = "test code")]
mod truncation;
