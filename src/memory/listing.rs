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
    out.push_str(&count_note(records.len()));
    out
}

/// The count of records, and the cap of one recall. `memory diff` prints it too, because a record
/// that does not fit in the budget is missing from the reply and reads as removed.
pub(crate) fn count_note(count: usize) -> String {
    format!(
        "{count} memories. llmenv asks icm for at most {}, and fewer come back when they do not \
         fit in a {}-token budget.",
        recall_json::BUDGET_MAX_LIMIT,
        recall_json::MAX_TOKENS
    )
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

/// One record on one line. The summary is cut at its first line break, with ` …` when more
/// follows, so two records that differ after that break do not print the same line. Each field
/// loses its control and bidirectional characters, so a stored memory cannot forge a diff line
/// or send a terminal escape.
fn line(r: &SnapshotRecord) -> String {
    use crate::util::strip_unsafe_chars;
    let mut summary_lines = r.summary.lines();
    let first = summary_lines.next().unwrap_or_default();
    let more = if summary_lines.next().is_some() {
        " …"
    } else {
        ""
    };
    format!(
        "[{}] {}{more} ({}, {})",
        strip_unsafe_chars(&r.topic),
        strip_unsafe_chars(first),
        strip_unsafe_chars(&r.importance),
        strip_unsafe_chars(&r.id)
    )
}

/// The diff lines from `previous` to `current`, matched by record id: `+` for a new record, `-`
/// for a gone one, and a `-` then `+` pair for a changed one. Empty when nothing changed. icm sends
/// each id once; if an id repeats, its last record is the one compared.
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
        assert!(text.starts_with("[t] one … (high, a1)\n"), "{text}");
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

    // A stored memory cannot send a terminal escape or a second line through any field.
    #[test]
    fn a_record_line_has_no_control_character_in_any_field() {
        let hostile = SnapshotRecord {
            id: "i\r\x1b]52;c;x\x07".into(),
            topic: "t\nforged".into(),
            summary: "s\u{202e}x\rmore".into(),
            importance: "high\x1b[2J".into(),
        };
        let text = line(&hostile);
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
    }

    #[test]
    fn records_that_differ_after_the_first_line_print_different_lines() {
        let one = rec("a", "same\nfirst");
        let two = rec("a", "same");
        assert_ne!(line(&one), line(&two));
    }

    fn arb_records() -> impl Strategy<Value = Vec<SnapshotRecord>> {
        proptest::collection::btree_map("[a-e]", "\\PC{0,8}", 0..5).prop_map(|m| {
            m.into_iter()
                .map(|(id, summary)| rec(&id, &summary))
                .collect()
        })
    }

    /// The ids that the diff marks with `sign`, read from the end of each line.
    fn ids(lines: &[String], sign: char) -> std::collections::BTreeSet<String> {
        lines
            .iter()
            .filter(|l| l.starts_with(sign))
            .filter_map(|l| l.rsplit(", ").next())
            .map(|tail| tail.trim_end_matches(')').to_string())
            .collect()
    }

    proptest! {
        /// A snapshot compared with itself, in any order, shows no change. A removed id is only
        /// on `-` lines, an added id only on `+` lines, a changed id on both, and an equal id on
        /// none.
        #[test]
        fn the_diff_names_each_added_removed_and_changed_id(
            previous in arb_records(),
            current in arb_records(),
        ) {
            let mut reversed = previous.clone();
            reversed.reverse();
            prop_assert!(diff_lines(&previous, &reversed).is_empty());
            let lines = diff_lines(&previous, &current);
            prop_assert!(lines.iter().all(|l| !l.contains('\n')));
            let minus = ids(&lines, '-');
            let plus = ids(&lines, '+');
            for p in &previous {
                match current.iter().find(|c| c.id == p.id) {
                    None => prop_assert!(minus.contains(&p.id) && !plus.contains(&p.id)),
                    Some(c) if c != p => prop_assert!(minus.contains(&p.id) && plus.contains(&p.id)),
                    Some(_) => prop_assert!(!minus.contains(&p.id) && !plus.contains(&p.id)),
                }
            }
            for c in current.iter().filter(|c| previous.iter().all(|p| p.id != c.id)) {
                prop_assert!(plus.contains(&c.id) && !minus.contains(&c.id));
            }
        }

        /// A snapshot reads back exactly as written, whatever its summaries hold.
        #[test]
        fn a_snapshot_round_trips(records in proptest::collection::vec(
            ("[a-z0-9]{1,6}", "\\PC{0,4}", "(?s).{0,20}", "[a-z]{0,6}"), 0..5,
        )) {
            let records: Vec<SnapshotRecord> = records
                .into_iter()
                .map(|(id, topic, summary, importance)| SnapshotRecord { id, topic, summary, importance })
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(DIFF_SNAPSHOT);
            write_snapshot(&path, &records).unwrap();
            prop_assert_eq!(read_snapshot(&path).unwrap(), Some(records));
        }

        /// The list has one line for each record, each line starts with its topic, and the count
        /// line comes last.
        #[test]
        fn the_list_has_one_line_per_record(records in arb_records()) {
            let parsed: Vec<RecallRecord> = records
                .iter()
                .map(|r| RecallRecord {
                    id: r.id.clone(),
                    topic: r.topic.clone(),
                    summary: r.summary.clone(),
                    importance: r.importance.clone(),
                    keywords: Vec::new(),
                })
                .collect();
            let text = render_list(&parsed);
            let lines: Vec<&str> = text.lines().collect();
            prop_assert_eq!(lines.len(), records.len() + 1);
            let count_line = format!("{} memories.", records.len());
            prop_assert!(lines.last().is_some_and(|l| l.starts_with(&count_line)));
        }
    }

    // icm sends each id once. If an id repeats, the last record for it is the one compared.
    #[test]
    fn a_repeated_id_compares_its_last_record() {
        let previous = [rec("a", "first"), rec("a", "second")];
        assert!(diff_lines(&previous, &[rec("a", "second")]).is_empty());
    }
}
