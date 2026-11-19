//! Reads every task file in the store. The strict and the lenient listings share this read, so a
//! file that cannot be read is handled the same way on both paths.

use super::{Task, tasks_dir};
use anyhow::Context as _;
use std::path::Path;

/// What [`read_tasks`] does with a task file or directory entry that cannot be read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UnreadablePolicy {
    /// Log the file and leave it out of the listing.
    Skip,
    /// Fail the whole listing.
    Fail,
}

pub(super) fn read_tasks(state_dir: &Path, policy: UnreadablePolicy) -> anyhow::Result<Vec<Task>> {
    #[cfg(test)]
    super::StrictTasks::count_read();
    let dir = tasks_dir(state_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("reading tasks dir {}", dir.display()))
            );
        }
    };
    let mut tasks = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if policy == UnreadablePolicy::Fail => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reading an entry of tasks dir {}", dir.display())));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %dir.display(),
                    "skipping unreadable directory entry"
                );
                crate::agent_notice::record(format!(
                    "llmenv skipped an unreadable entry in {}: {e}. Fix: check the directory's \
                     permissions.",
                    dir.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_task_file(&path) {
            Ok(task) => tasks.push(task),
            // Distinguish a genuine read failure (e.g. permission denied on this
            // one file) from corrupt JSON content: the former is an `io::Error`
            // (from `read_to_string`), the latter a `serde_json::Error` — the
            // wrong diagnosis sends someone chasing data corruption for what's
            // actually a permissions problem (#1112).
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
                if policy == UnreadablePolicy::Fail {
                    return Err(e.context(format!("reading task file {}", path.display())));
                }
                tracing::error!(
                    error = %e,
                    path = %path.display(),
                    "skipping unreadable task file"
                );
                // A refused read (a FIFO, a file past the cap, or not UTF-8) is not a permission
                // problem, so its fix differs.
                let refused = matches!(
                    e.downcast_ref::<std::io::Error>(),
                    Some(io) if io.kind() == std::io::ErrorKind::InvalidData
                );
                let fix = if refused {
                    "remove the file, or replace it with a task file that llmenv wrote"
                } else {
                    "check the file's permissions"
                };
                crate::agent_notice::record(format!(
                    "llmenv skipped the unreadable task file {}: {e}. The task is missing from \
                     task lists and resume. Fix: {fix}.",
                    path.display()
                ));
            }
            Err(e) if policy == UnreadablePolicy::Fail => {
                return Err(e.context(format!("parsing task file {}", path.display())));
            }
            Err(e) => {
                // The full chain names the cause, such as an unreadable created_at and its fix.
                let cause = format!("{e:#}");
                tracing::error!(error = %cause, path = %path.display(), "skipping task file");
                crate::agent_notice::record(format!(
                    "llmenv skipped the task file {}: {cause}. The task is missing from task \
                     lists and resume.",
                    path.display()
                ));
            }
        }
    }
    Ok(tasks)
}

/// Read one task file, then check its `created_at`. The queue sorts on that field, so a value
/// that does not parse would move the task to the head of the queue with no cue (#3856).
fn read_task_file(path: &Path) -> anyhow::Result<Task> {
    let content = crate::paths::bounded::state_text_io(path).map_err(anyhow::Error::from)?;
    let task: Task = serde_json::from_str(&content).with_context(|| {
        format!(
            "task file {} is not valid task JSON. Fix: repair or delete the file.",
            path.display()
        )
    })?;
    crate::util::time::validate_stored(&task.created_at).with_context(|| {
        format!(
            "task '{}' at {} has an unreadable created_at. Fix: rewrite it as RFC 3339, \
             such as 2026-01-01T00:00:00Z",
            task.slug,
            path.display()
        )
    })?;
    Ok(task)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use super::{UnreadablePolicy, read_tasks};
    use crate::task::tasks_dir;
    use proptest::prelude::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_task(state_dir: &Path, slug: &str, created_at: &str) -> std::path::PathBuf {
        let dir = tasks_dir(state_dir);
        std::fs::create_dir_all(&dir).expect("test");
        let body = serde_json::json!({
            "slug": slug,
            "title": slug,
            "created_at": created_at,
            "updated_at": "",
        });
        let path = dir.join(format!("{slug}.json"));
        std::fs::write(&path, body.to_string()).expect("test");
        path
    }

    #[test]
    fn a_strict_read_fails_on_an_unreadable_created_at_and_names_the_fix() {
        let dir = TempDir::new().expect("test");
        let bad = write_task(dir.path(), "bad", "yesterday");
        let err = read_tasks(dir.path(), UnreadablePolicy::Fail)
            .expect_err("an unreadable created_at must fail the strict read");
        let text = format!("{err:#}");
        assert!(text.contains(&bad.display().to_string()), "{text}");
        assert!(text.contains("Fix: rewrite it as RFC 3339"), "{text}");
    }

    #[test]
    fn a_lenient_read_leaves_out_the_unreadable_created_at_and_keeps_the_rest() {
        let dir = TempDir::new().expect("test");
        write_task(dir.path(), "bad", "yesterday");
        write_task(dir.path(), "good", "2026-01-01T00:00:00Z");
        let tasks = read_tasks(dir.path(), UnreadablePolicy::Skip).expect("lenient read");
        let slugs: Vec<&str> = tasks.iter().map(|t| t.slug.as_str()).collect();
        assert_eq!(slugs, ["good"]);
    }

    #[test]
    fn a_strict_read_accepts_a_valid_created_at() {
        let dir = TempDir::new().expect("test");
        write_task(dir.path(), "good", "2026-01-01T00:00:00Z");
        let tasks = read_tasks(dir.path(), UnreadablePolicy::Fail).expect("valid read");
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn a_lenient_read_notices_the_skipped_created_at_with_its_cause_and_fix() {
        let dir = TempDir::new().expect("test");
        write_task(dir.path(), "store-read-3856", "yesterday");
        read_tasks(dir.path(), UnreadablePolicy::Skip).expect("lenient read");
        crate::agent_notice::assert_noticed(&[
            "store-read-3856",
            "unreadable created_at",
            "Fix: rewrite it as RFC 3339",
        ]);
    }

    fn created_at_text() -> impl Strategy<Value = String> {
        prop_oneof![
            (0i64..4_000_000_000).prop_map(|s| {
                jiff::Timestamp::from_second(s)
                    .expect("in range")
                    .to_string()
            }),
            "\\PC{0,30}",
        ]
    }

    proptest! {
        // The strict read fails exactly when the validator rejects created_at, and the lenient
        // read keeps the task exactly when the validator accepts it. A disagreement would let a
        // bad value reach the queue sort (#3856).
        #[test]
        fn the_reads_agree_with_the_created_at_validator(created_at in created_at_text()) {
            let dir = TempDir::new().expect("test");
            write_task(dir.path(), "p", &created_at);
            let valid = crate::util::time::validate_stored(&created_at).is_ok();
            let strict = read_tasks(dir.path(), UnreadablePolicy::Fail);
            let lenient = read_tasks(dir.path(), UnreadablePolicy::Skip).expect("lenient read");
            prop_assert_eq!(strict.is_ok(), valid);
            prop_assert_eq!(lenient.len(), usize::from(valid));
        }
    }
}
