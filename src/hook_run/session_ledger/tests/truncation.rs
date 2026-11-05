//! A cut-off ledger file, the shape a crash or a power loss leaves (#3130).

use super::super::*;
use super::store;

/// Each strict prefix of a saved ledger, the empty file included, starts a fresh epoch and tells
/// the agent that records may repeat.
#[test]
fn every_cut_off_ledger_starts_a_fresh_epoch() {
    let (dir, store) = store();
    let mut ledger = Ledger::default();
    ledger.reset();
    ledger.last_turn_at = 42;
    ledger.project = Some("p".to_string());
    assert!(store.write("s1", &ledger));
    let path = dir.path().join("recall_session").join("s1.json");
    let saved = std::fs::read_to_string(&path).unwrap();
    let saved = saved.trim_end();
    assert_eq!(store.load("s1"), Some(ledger));
    for len in 0..saved.len() {
        std::fs::write(&path, &saved[..len]).unwrap();
        assert_eq!(
            store.load("s1"),
            Some(Ledger::default()),
            "prefix of {len} bytes"
        );
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(text.contains("recall ledger is corrupt"), "{len}: {text}");
    }
}
