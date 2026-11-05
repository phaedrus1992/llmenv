//! `memory list` and `memory diff`: the project's records from the JSON form of icm recall
//! (#3421).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::memory::recall_json::{self, RecallRecord};

/// The file that holds the records of the last `memory diff` run, in the state folder.
pub(crate) const DIFF_SNAPSHOT: &str = "memory_diff_snapshot.json";

/// The recall arguments for the listing: `scope` (the project filter) with the JSON form and the
/// largest count icm returns. icm clamps `limit` to 20 unless the call gives a token budget
/// (#3419).
pub(crate) fn listing_args(mut scope: serde_json::Value) -> serde_json::Value {
    scope["format"] = recall_json::FORMAT.into();
    scope["limit"] = recall_json::BUDGET_MAX_LIMIT.into();
    scope["max_tokens"] = recall_json::MAX_TOKENS.into();
    scope
}

/// The text of `memory list`: one line for each record, then the count and the cap.
pub(crate) fn render_list(records: &[RecallRecord]) -> String {
    let mut out: String = records
        .iter()
        .map(|r| format!("{}\n", line(&SnapshotRecord::from(r))))
        .collect();
    out.push_str(&format!(
        "{} memories. llmenv asks icm for at most {}, and fewer come back when they do not fit \
         in a {}-token budget.",
        records.len(),
        recall_json::BUDGET_MAX_LIMIT,
        recall_json::MAX_TOKENS
    ));
    out
}

/// One record as `memory diff` stores and compares it. Other recall fields, such as the score,
/// change from call to call, so they are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotRecord {
    pub id: String,
    pub topic: String,
    pub summary: String,
    pub importance: String,
}

impl From<&RecallRecord> for SnapshotRecord {
    fn from(r: &RecallRecord) -> Self {
        Self {
            id: r.id.clone(),
            topic: r.topic.clone(),
            summary: r.summary.clone(),
            importance: r.importance.clone(),
        }
    }
}

/// One record on one line. The summary is cut at its first line break, so a summary cannot forge
/// a second diff line.
fn line(r: &SnapshotRecord) -> String {
    let summary = r.summary.lines().next().unwrap_or_default();
    format!("[{}] {summary} ({}, {})", r.topic, r.importance, r.id)
}

/// The diff lines from `previous` to `current`, matched by record id: `+` for a new record, `-`
/// for a gone one, and a `-` then `+` pair for a changed one. Empty when nothing changed.
pub(crate) fn diff_lines(previous: &[SnapshotRecord], current: &[SnapshotRecord]) -> Vec<String> {
    let before: BTreeMap<&str, &SnapshotRecord> =
        previous.iter().map(|r| (r.id.as_str(), r)).collect();
    let after: BTreeMap<&str, &SnapshotRecord> =
        current.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut lines = Vec::new();
    for (id, old) in &before {
        match after.get(id) {
            None => lines.push(format!("-{}", line(old))),
            Some(new) if new != old => {
                lines.push(format!("-{}", line(old)));
                lines.push(format!("+{}", line(new)));
            }
            Some(_) => {}
        }
    }
    for (id, new) in &after {
        if !before.contains_key(id) {
            lines.push(format!("+{}", line(new)));
        }
    }
    lines
}

/// The records of the last `memory diff` run, or `None` on the first run.
///
/// # Errors
/// The snapshot cannot be read, or it is not the JSON list this module writes. A missing file is
/// the first run, and only that case gives `None`, so a permission error cannot overwrite the
/// baseline (#911).
pub(crate) fn read_snapshot(path: &Path) -> anyhow::Result<Option<Vec<SnapshotRecord>>> {
    let text = match crate::paths::bounded::state_text_io(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("reading {}", path.display())));
        }
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        anyhow::anyhow!(
            "cannot read the memory diff snapshot {}: {e}. Fix: remove the file, then run \
             `llmenv memory diff` again to save a new baseline.",
            path.display()
        )
    })
}

/// Save `records` as the snapshot that the next `memory diff` compares against.
///
/// # Errors
/// The snapshot cannot be written.
pub(crate) fn write_snapshot(path: &Path, records: &[SnapshotRecord]) -> anyhow::Result<()> {
    let text = serde_json::to_string(records)?;
    crate::paths::write_owner_only_atomic(path, text.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rec(id: &str, summary: &str) -> SnapshotRecord {
        SnapshotRecord {
            id: id.into(),
            topic: "t".into(),
            summary: summary.into(),
            importance: "high".into(),
        }
    }

    #[test]
    fn listing_args_keep_the_scope_and_ask_for_json_with_a_budget() {
        let args = listing_args(serde_json::json!({ "query": "", "project": "p" }));
        assert_eq!(args["project"], "p");
        assert_eq!(args["format"], "json");
        assert_eq!(args["limit"], 200);
        assert_eq!(args["max_tokens"], 32_000);
    }

    #[test]
    fn the_list_names_each_record_and_the_cap() {
        let records = recall_json::parse(
            r#"[{"id":"a1","topic":"t","summary":"one\ntwo","importance":"high"}]"#,
        )
        .unwrap();
        let text = render_list(&records);
        assert!(text.starts_with("[t] one (high, a1)\n"), "{text}");
        assert!(!text.contains("two"), "{text}");
        assert!(
            text.contains("1 memories. llmenv asks icm for at most 200"),
            "{text}"
        );
    }

    #[test]
    fn a_diff_matches_records_by_id() {
        let previous = [rec("a", "same"), rec("b", "old"), rec("c", "gone")];
        let current = [rec("b", "new"), rec("a", "same"), rec("d", "added")];
        assert_eq!(
            diff_lines(&previous, &current),
            vec![
                "-[t] old (high, b)",
                "+[t] new (high, b)",
                "-[t] gone (high, c)",
                "+[t] added (high, d)",
            ]
        );
    }

    #[test]
    fn a_missing_snapshot_is_the_first_run_and_a_bad_one_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DIFF_SNAPSHOT);
        assert_eq!(read_snapshot(&path).unwrap(), None);
        write_snapshot(&path, &[rec("a", "x")]).unwrap();
        assert_eq!(read_snapshot(&path).unwrap(), Some(vec![rec("a", "x")]));
        std::fs::write(&path, "--- old text snapshot").unwrap();
        let err = read_snapshot(&path).unwrap_err().to_string();
        assert!(err.contains("Fix: remove the file"), "{err}");
    }

    // #911: a non-NotFound I/O error (EACCES) must propagate, not be masked as "no snapshot",
    // which would overwrite the baseline.
    #[cfg(unix)]
    #[test]
    fn a_permission_error_propagates_instead_of_reading_as_absent() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join(DIFF_SNAPSHOT);
        write_snapshot(&path, &[rec("a", "x")]).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_snapshot(&path);
        let readable_anyway = std::fs::read_dir(&dir).is_ok();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A process that bypasses permissions (root) cannot get EACCES here (#3086).
        assert!(
            !readable_anyway,
            "EACCES cannot be exercised: this process reads the locked folder. Run the test as a \
             non-root user."
        );
        assert!(
            result.is_err(),
            "permission error must propagate, got {result:?}"
        );
    }

    fn arb_records() -> impl Strategy<Value = Vec<SnapshotRecord>> {
        proptest::collection::btree_map("[a-e]", "[a-z ]{0,6}", 0..5).prop_map(|m| {
            m.into_iter()
                .map(|(id, summary)| rec(&id, &summary))
                .collect()
        })
    }

    proptest! {
        /// A snapshot compared with itself, in any order, shows no change. Each id that is only
        /// on one side gives exactly one line.
        #[test]
        fn the_diff_depends_on_ids_and_content_not_order(
            previous in arb_records(),
            current in arb_records(),
        ) {
            let mut shuffled = previous.clone();
            shuffled.reverse();
            prop_assert!(diff_lines(&previous, &shuffled).is_empty());
            let lines = diff_lines(&previous, &current);
            let only_one_side = previous.iter().filter(|p| !current.iter().any(|c| c.id == p.id)).count()
                + current.iter().filter(|c| !previous.iter().any(|p| p.id == c.id)).count();
            let changed = previous
                .iter()
                .filter(|p| current.iter().any(|c| c.id == p.id && c != *p))
                .count();
            prop_assert_eq!(lines.len(), only_one_side + 2 * changed);
        }
    }
}
