//! `llmenv doctor`: the result of the last codebase-memory index of this project (#2154).
//!
//! Design: docs/design/issue-2154-cbm-index-health.md

use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;

use super::CheckLevel;

/// The result `status` values that report a failed index.
const FAILED_STATUSES: [&str; 4] = [
    "error",
    "aborted_previous_preserved",
    "ambiguous",
    "persist_failed",
];

/// The finish time as `YYYY-MM-DD HH:MM UTC`. The `jiff` build has no time-zone database, so the
/// time is UTC.
fn format_time(time: SystemTime) -> String {
    jiff::Timestamp::try_from(time).map_or_else(
        |_| "at an unknown time".to_string(),
        |ts| ts.strftime("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// Classify the text of a result file. `finished` is the file's modification time, and `log` is
/// the path of the index log, which a failure points to.
fn classify(text: &str, finished: &str, log: &Path) -> (CheckLevel, String) {
    let Ok(result) = serde_json::from_str::<Value>(text.trim()) else {
        return (
            CheckLevel::Info,
            format!(
                "codebase-memory: last index finished {finished}; result not readable (the run \
                 may still be going or was killed)"
            ),
        );
    };
    let field = |key: &str| result.get(key).and_then(Value::as_str);
    let number = |key: &str| result.get(key).and_then(Value::as_u64);
    if field("reason") == Some("over_memory_budget") {
        let mut text =
            format!("codebase-memory: last index ({finished}) stopped at the memory budget");
        let facts: Vec<String> = [
            number("budget_mb").map(|n| format!("budget {n} MB")),
            number("peak_rss_mb").map(|n| format!("peak {n} MB")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !facts.is_empty() {
            text.push_str(&format!(": {}", facts.join(", ")));
        }
        text.push_str(". The previous index is still served.");
        if let Some(n) = number("suggested_budget_mb") {
            text.push_str(&format!(" Set codebase_memory.mem_budget_mb to {n}."));
        }
        return (CheckLevel::Warn, text);
    }
    let status = field("status");
    if status.is_some_and(|s| FAILED_STATUSES.contains(&s)) {
        let mut text = format!(
            "codebase-memory: last index ({finished}) ended with status {}",
            status.unwrap_or_default()
        );
        if let Some(reason) = field("reason") {
            text.push_str(&format!(", reason {reason}"));
        }
        text.push('.');
        if let Some(hint) = field("hint") {
            text.push_str(&format!(" {hint}."));
        }
        text.push_str(&format!(" Log: {}", log.display()));
        // The server controls the text, and doctor prints it to a terminal.
        return (CheckLevel::Warn, crate::util::strip_unsafe_chars(&text));
    }
    (
        CheckLevel::Pass,
        format!("codebase-memory: last index finished {finished}"),
    )
}

/// The doctor line for the last index of `project_root`.
fn check(cache_dir: &Path, project_root: &Path) -> (CheckLevel, String) {
    let path = crate::hook_run::index_result_path(cache_dir, project_root);
    let log = cache_dir.join("index.log");
    let finished = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(time) => format_time(time),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                CheckLevel::Info,
                "codebase-memory: no index result for this project yet".to_string(),
            );
        }
        Err(e) => {
            return (
                CheckLevel::Info,
                format!("codebase-memory: cannot read {}: {e}", path.display()),
            );
        }
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    classify(&text, &finished, &log)
}

/// Print the line for the active `codebase_memory` entry.
pub(super) fn run_doctor_cbm_index(
    use_color: bool,
    config: &crate::config::Config,
    active: &crate::scope::ActiveScopes,
) {
    let Some(entry) = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default()
        .iter()
        .find(|cm| cm.when.iter().any(|t| active.tags.contains(t)))
    else {
        return;
    };
    let (project_root, state_dir) = match crate::mcp::resolve::codebase_memory_paths() {
        Ok(paths) => paths,
        Err(e) => {
            tracing::debug!("doctor: no project root for the codebase-memory result: {e}");
            return;
        }
    };
    let cache_dir = crate::hook_run::codebase_memory_cache_dir(entry, &state_dir);
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    super::print_check(check(&cache_dir, &project_root), &pass, &warn, &info);
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    const LOG: &str = "/state/codebase-memory/index.log";

    fn run(text: &str) -> (CheckLevel, String) {
        classify(text, "2026-10-03 12:00 UTC", Path::new(LOG))
    }

    #[test]
    fn an_over_budget_result_names_the_numbers_and_the_setting() {
        let (level, text) = run(
            r#"{"status":"error","reason":"over_memory_budget","previous_index":"preserved",
                "budget_mb":128,"peak_rss_mb":900,"suggested_budget_mb":1024}"#,
        );
        assert_eq!(level, CheckLevel::Warn);
        assert_eq!(
            text,
            "codebase-memory: last index (2026-10-03 12:00 UTC) stopped at the memory budget: \
             budget 128 MB, peak 900 MB. The previous index is still served. Set \
             codebase_memory.mem_budget_mb to 1024."
        );
    }

    #[test]
    fn an_over_budget_result_leaves_out_a_missing_number() {
        let (_, text) = run(r#"{"reason":"over_memory_budget","budget_mb":128}"#);
        assert!(
            text.contains("budget 128 MB.") && !text.contains("peak"),
            "{text}"
        );
        assert!(!text.contains("Set codebase_memory"), "{text}");
        let (_, bare) = run(r#"{"reason":"over_memory_budget"}"#);
        assert_eq!(
            bare,
            "codebase-memory: last index (2026-10-03 12:00 UTC) stopped at the memory budget. \
             The previous index is still served."
        );
    }

    #[test]
    fn every_failed_status_warns_with_reason_hint_and_log() {
        for status in FAILED_STATUSES {
            let (level, text) = run(&format!(
                r#"{{"status":"{status}","reason":"disk","hint":"free some space"}}"#
            ));
            assert_eq!(level, CheckLevel::Warn, "{status}");
            assert_eq!(
                text,
                format!(
                    "codebase-memory: last index (2026-10-03 12:00 UTC) ended with status \
                     {status}, reason disk. free some space. Log: {LOG}"
                )
            );
        }
        let (_, plain) = run(r#"{"status":"error"}"#);
        assert!(
            plain.ends_with(&format!("status error. Log: {LOG}")),
            "{plain}"
        );
    }

    #[test]
    fn a_success_result_passes() {
        for text in [r#"{"status":"ok","nodes":10}"#, r#"{"nodes":10}"#, "{}"] {
            assert_eq!(
                run(text),
                (
                    CheckLevel::Pass,
                    "codebase-memory: last index finished 2026-10-03 12:00 UTC".to_string()
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn an_unreadable_result_is_information() {
        for text in ["", "   \n", "not json", "{\"status\":"] {
            let (level, message) = run(text);
            assert_eq!(level, CheckLevel::Info, "{text:?}");
            assert!(message.contains("result not readable"), "{message}");
        }
    }

    #[test]
    fn server_text_loses_control_characters() {
        let (_, text) = run("{\"status\":\"error\",\"hint\":\"a\\u001b]52;c;x\\u0007b\"}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
    }

    #[test]
    fn the_check_reads_the_file_of_this_project_only() {
        let dir = tempfile::tempdir().unwrap();
        let (mine, other) = (Path::new("/work/mine"), Path::new("/work/other"));
        assert_eq!(
            check(dir.path(), mine),
            (
                CheckLevel::Info,
                "codebase-memory: no index result for this project yet".to_string()
            )
        );
        let path = crate::hook_run::index_result_path(dir.path(), other);
        std::fs::write(path, r#"{"status":"error"}"#).unwrap();
        assert_eq!(check(dir.path(), mine).0, CheckLevel::Info);
        let own = crate::hook_run::index_result_path(dir.path(), mine);
        std::fs::write(own, r#"{"status":"ok"}"#).unwrap();
        let (level, text) = check(dir.path(), mine);
        assert_eq!(level, CheckLevel::Pass);
        assert!(
            text.starts_with("codebase-memory: last index finished 20"),
            "{text}"
        );
        assert!(text.ends_with(" UTC"), "{text}");
    }

    #[test]
    fn the_time_is_formatted_in_utc() {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        assert_eq!(format_time(time), "2026-09-21 14:13 UTC");
    }
}
