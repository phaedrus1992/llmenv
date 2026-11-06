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

/// The finish time as `YYYY-MM-DD HH:MM UTC`. The time is UTC on purpose: it is a timestamp
/// from a log, so it does not depend on the zone of the machine that reads it.
fn format_time(time: SystemTime) -> String {
    jiff::Timestamp::try_from(time).map_or_else(
        |_| "at an unknown time".to_string(),
        |ts| ts.strftime("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// Classify the text of a result file. `finished` is the file's modification time, and `log` is
/// the path of the index log, which a failure points to.
fn classify(text: &str, finished: &str, log: &Path) -> (CheckLevel, String) {
    let result = serde_json::from_str::<Value>(text.trim())
        .ok()
        .filter(Value::is_object);
    let Some(result) = result else {
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

/// The most bytes of a result file that doctor reads. A result is one small JSON object.
const MAX_RESULT_BYTES: u64 = 64 * 1024;

/// The doctor line for the last index of `project_root`.
fn check(cache_dir: &Path, project_root: &Path) -> (CheckLevel, String) {
    use std::io::Read as _;
    let path = crate::hook_run::index_result_path(cache_dir, project_root);
    let log = cache_dir.join("index.log");
    let cannot_read = |e: &dyn std::fmt::Display| {
        (
            CheckLevel::Info,
            format!("codebase-memory: cannot read {}: {e}", path.display()),
        )
    };
    // `symlink_metadata`: a link in a shared cache folder is not followed to another file.
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                CheckLevel::Info,
                "codebase-memory: no index result for this project yet".to_string(),
            );
        }
        Err(e) => return cannot_read(&e),
    };
    if !meta.is_file() {
        return cannot_read(&"it is not a regular file");
    }
    let finished = meta
        .modified()
        .map_or_else(|_| "at an unknown time".to_string(), format_time);
    // An indexer killed during its write can cut the last character (#3457).
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_RESULT_BYTES).read_to_end(&mut bytes))
        .and_then(|_| crate::paths::bounded::decode_state_text(bytes));
    let text = match read {
        Ok(text) => text,
        Err(e) => return cannot_read(&e),
    };
    classify(&text, &finished, &log)
}

/// Print the line for the active `codebase_memory` entry.
pub(super) fn run_doctor_cbm_index(
    use_color: bool,
    config: &crate::config::Config,
    active: &crate::scope::ActiveScopes,
) {
    let entries = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    let mut active_entries = entries
        .iter()
        .filter(|cm| cm.when.iter().any(|t| active.tags.contains(t)));
    let (Some(entry), None) = (active_entries.next(), active_entries.next()) else {
        // None is fine. Two or more is ambiguous: the SessionStart index skips it too.
        if entries
            .iter()
            .filter(|cm| cm.when.iter().any(|t| active.tags.contains(t)))
            .count()
            > 1
        {
            let info = super::super::doctor_info(use_color);
            eprintln!(
                "{info} codebase-memory: more than one entry is active, so no index runs and \
                 there is no result to report"
            );
        }
        return;
    };
    let (project_root, state_dir) = match crate::mcp::resolve::codebase_memory_paths() {
        Ok(paths) => paths,
        Err(e) => {
            let info = super::super::doctor_info(use_color);
            eprintln!(
                "{info} codebase-memory: cannot find the project root for the index result: {e}"
            );
            return;
        }
    };
    let cache_dir = match crate::hook_run::codebase_memory_cache_dir(entry, &state_dir) {
        Ok(dir) => dir,
        Err(e) => {
            let info = super::super::doctor_info(use_color);
            eprintln!("{info} codebase-memory: cannot expand index_path: {e}");
            return;
        }
    };
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    super::print_check(check(&cache_dir, &project_root), &pass, &warn, &info);
    print_roots(entry, &project_root, (&pass, &warn, &info));
}

/// Print the roots that codebase-memory-mcp may index, and any wanted root it lacks (#2406).
fn print_roots(
    entry: &crate::config::CodebaseMemory,
    project_root: &Path,
    (pass, warn, info): (&str, &str, &str),
) {
    use llmenv_mcp::cbm_roots;
    let bases = cbm_roots::RootBases::for_project(project_root);
    let problems = cbm_roots::root_problems(entry, &bases);
    let wanted = cbm_roots::configured_roots(entry, &bases);
    let check = cbm_roots::check_roots(entry, &wanted, bases.home.as_deref())
        .map(|report| {
            let (is_warn, text) = cbm_roots::describe(&report);
            (
                if is_warn {
                    CheckLevel::Warn
                } else {
                    CheckLevel::Pass
                },
                text,
            )
        })
        .unwrap_or_else(|e| {
            (
                CheckLevel::Warn,
                format!(
                    "codebase-memory: cannot check the allowed roots: {e:#}. Run `llmenv doctor` \
                     after the fix, and check that codebase-memory-mcp is on PATH."
                ),
            )
        });
    super::print_check(check, pass, warn, info);
    for problem in problems {
        super::print_check((CheckLevel::Warn, problem), pass, warn, info);
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
