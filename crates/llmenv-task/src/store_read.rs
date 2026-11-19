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

/// The tasks of a read, with the count of the files that a lenient read left out.
pub struct ReadTasks {
    pub tasks: Vec<Task>,
    /// Files and directory entries that a lenient read skipped.
    skipped: usize,
    /// The first skipped item, with its cause, so a person can start the repair (#3883). It is
    /// `Some` exactly when `skipped` is above 0, and only `skip` sets either.
    first_problem: Option<String>,
}

impl ReadTasks {
    /// The count of the skipped items and the first problem, or `None` when nothing was skipped.
    pub fn skipped(&self) -> Option<(usize, &str)> {
        self.first_problem
            .as_deref()
            .map(|first| (self.skipped, first))
    }

    fn skip(&mut self, problem: String) {
        self.skipped += 1;
        self.first_problem.get_or_insert(problem);
    }
}

pub(super) fn read_tasks(state_dir: &Path, policy: UnreadablePolicy) -> anyhow::Result<Vec<Task>> {
    read_tasks_counted(state_dir, policy).map(|read| read.tasks)
}

/// The lenient read: a read error on the tasks directory is an error, a missing directory is no
/// tasks, and a corrupt task file is logged, noticed, and skipped (#1112). It counts the skipped files. A person at
/// a terminal sees the count, because an agent notice reaches hook output only (#3883).
///
/// # Errors
/// Returns an error if the tasks directory exists but cannot be read.
pub fn try_list_tasks_counted(state_dir: &Path) -> anyhow::Result<ReadTasks> {
    read_tasks_counted(state_dir, UnreadablePolicy::Skip)
}

/// [`read_tasks`] with the count of the skipped files. A strict read never skips, so its count is
/// 0.
fn read_tasks_counted(state_dir: &Path, policy: UnreadablePolicy) -> anyhow::Result<ReadTasks> {
    super::StrictTasks::count_read();
    let dir = tasks_dir(state_dir);
    let mut read = ReadTasks {
        tasks: Vec::new(),
        skipped: 0,
        first_problem: None,
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(read),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("reading tasks dir {}", dir.display()))
            );
        }
    };
    // A fixed order, so the first problem is the same on every run.
    let mut entries: Vec<_> = entries.collect();
    entries.sort_by_key(|e| e.as_ref().ok().map(std::fs::DirEntry::path));
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if policy == UnreadablePolicy::Fail => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reading an entry of tasks dir {}", dir.display())));
            }
            Err(e) => {
                read.skip(skip_entry(&dir, &e));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_task_file(&path) {
            Ok(task) => read.tasks.push(task),
            // Distinguish a genuine read failure (e.g. permission denied on this
            // one file) from corrupt JSON content: the former is an `io::Error`
            // (from `read_to_string`), the latter a `serde_json::Error` — the
            // wrong diagnosis sends someone chasing data corruption for what's
            // actually a permissions problem (#1112).
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
                if policy == UnreadablePolicy::Fail {
                    return Err(e.context(format!("reading task file {}", path.display())));
                }
                read.skip(skip_unreadable_file(&path, &e));
            }
            // The message names the file, so no more context is added.
            Err(e) if policy == UnreadablePolicy::Fail => return Err(e),
            Err(e) => read.skip(skip_task_file(&path, &e)),
        }
    }
    Ok(read)
}

/// Log and notice a directory entry that cannot be read. Returns the problem for the count.
fn skip_entry(dir: &Path, e: &std::io::Error) -> String {
    tracing::warn!(
        error = %e,
        dir = %dir.display(),
        "skipping unreadable directory entry"
    );
    llmenv_util::agent_notice::record(llmenv_util::strip_unsafe_chars(&format!(
        "llmenv skipped an unreadable entry in {}: {e}. Fix: check the directory's permissions.",
        dir.display()
    )));
    format!("an unreadable entry in {}: {e}", dir.display())
}

/// Log and notice a task file that cannot be read from disk. Returns the problem for the count.
fn skip_unreadable_file(path: &Path, e: &anyhow::Error) -> String {
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
    llmenv_util::agent_notice::record(llmenv_util::strip_unsafe_chars(&format!(
        "llmenv skipped the unreadable task file {}: {e}. The task is missing from \
         task lists and resume. Fix: {fix}.",
        path.display()
    )));
    format!("{}: {e}", path.display())
}

/// Log and notice a task file that read but did not parse or validate. Returns the problem for
/// the count.
fn skip_task_file(path: &Path, e: &anyhow::Error) -> String {
    // The full chain names the cause, such as an unreadable created_at and its fix.
    let cause = llmenv_util::strip_unsafe_chars(&format!("{e:#}"));
    tracing::error!(error = %cause, path = %path.display(), "skipping task file");
    llmenv_util::agent_notice::record(format!(
        "llmenv skipped the task file {}: {cause}. The task is missing from task \
         lists and resume.",
        path.display()
    ));
    format!("{}: {cause}", path.display())
}

/// Check the `created_at` and `updated_at` of a task that was read from `path`, and name every
/// field that does not parse. Each reader of a task file calls this, so the lenient listing and
/// the commands that change one task agree on which files are bad (#3882).
///
/// # Errors
/// Errors if a field is not RFC 3339. The message names the task, the file, and the fix.
pub fn check_task_times(task: &Task, path: &Path) -> anyhow::Result<()> {
    let record = format!("task '{}' at {}", task.slug, path.display());
    let problems: Vec<String> = [
        ("created_at", &task.created_at),
        ("updated_at", &task.updated_at),
    ]
    .into_iter()
    .filter_map(|(field, value)| {
        llmenv_util::time::validate_stored_field(field, value, &record)
            .err()
            .map(|e| format!("{e:#}"))
    })
    .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", problems.join("; also "))
    }
}

/// Read one task file, then check its `created_at` and `updated_at`. The queue sorts on the first,
/// and the current-task pick uses the second, so a value that does not parse would win or lose
/// with no cue (#3856, #3882).
fn read_task_file(path: &Path) -> anyhow::Result<Task> {
    let content = crate::session::read_state_text(path).map_err(anyhow::Error::from)?;
    let task: Task = serde_json::from_str(&content).with_context(|| {
        format!(
            "task file {} is not valid task JSON. Fix: repair or delete the file.",
            path.display()
        )
    })?;
    check_task_times(&task, path)?;
    Ok(task)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use super::{UnreadablePolicy, read_tasks, try_list_tasks_counted};
    use crate::tasks_dir;
    use proptest::prelude::*;
    use std::path::Path;
    use tempfile::TempDir;

    const VALID_AT: &str = "2026-01-01T00:00:00Z";

    fn write_task(state_dir: &Path, slug: &str, created_at: &str) -> std::path::PathBuf {
        write_task_at(state_dir, slug, created_at, VALID_AT)
    }

    fn write_task_at(
        state_dir: &Path,
        slug: &str,
        created_at: &str,
        updated_at: &str,
    ) -> std::path::PathBuf {
        let dir = tasks_dir(state_dir);
        std::fs::create_dir_all(&dir).expect("test");
        let body = serde_json::json!({
            "slug": slug,
            "title": slug,
            "created_at": created_at,
            "updated_at": updated_at,
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
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        for part in [
            "store-read-3856",
            "unreadable created_at",
            "Fix: rewrite it as RFC 3339",
        ] {
            assert!(notices.contains(part), "{part} missing from {notices}");
        }
    }

    #[test]
    fn a_strict_read_fails_on_an_unreadable_updated_at_and_names_the_field_and_the_fix() {
        let dir = TempDir::new().expect("test");
        let bad = write_task_at(dir.path(), "bad", VALID_AT, "");
        let err = read_tasks(dir.path(), UnreadablePolicy::Fail)
            .expect_err("an unreadable updated_at must fail the strict read");
        let text = format!("{err:#}");
        assert!(text.contains(&bad.display().to_string()), "{text}");
        assert!(text.contains("unreadable updated_at"), "{text}");
        assert!(text.contains("Fix: rewrite it as RFC 3339"), "{text}");
    }

    #[test]
    fn the_counted_read_counts_each_skipped_file_and_keeps_the_first_cause() {
        let dir = TempDir::new().expect("test");
        write_task(dir.path(), "good", VALID_AT);
        let first = write_task(dir.path(), "first-bad", "yesterday");
        std::fs::write(tasks_dir(dir.path()).join("junk.json"), "{ not json").expect("test");

        let read = try_list_tasks_counted(dir.path()).expect("lenient read");
        assert_eq!(read.tasks.len(), 1);
        assert_eq!(read.skipped, 2);
        let problem = read.first_problem.expect("a first problem");
        assert!(problem.contains(&first.display().to_string()), "{problem}");
    }

    #[test]
    fn the_counted_read_of_a_clean_store_skips_nothing() {
        let dir = TempDir::new().expect("test");
        write_task(dir.path(), "good", VALID_AT);
        let read = try_list_tasks_counted(dir.path()).expect("read");
        assert_eq!((read.tasks.len(), read.skipped), (1, 0));
        assert!(read.first_problem.is_none());
    }

    #[test]
    fn a_lenient_read_skips_an_unreadable_updated_at_and_notices_it() {
        let dir = TempDir::new().expect("test");
        write_task_at(dir.path(), "store-read-3882", VALID_AT, "later");
        write_task(dir.path(), "good", VALID_AT);
        let tasks = read_tasks(dir.path(), UnreadablePolicy::Skip).expect("lenient read");
        let slugs: Vec<&str> = tasks.iter().map(|t| t.slug.as_str()).collect();
        assert_eq!(slugs, ["good"]);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        for want in [
            "store-read-3882",
            "unreadable updated_at",
            "Fix: rewrite it as RFC 3339",
        ] {
            assert!(notices.contains(want), "{want:?} not in {notices}");
        }
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
        // The strict read fails exactly when the validator rejects `created_at` or `updated_at`,
        // and the lenient read keeps the task exactly when the validator accepts both. A
        // disagreement would let a bad value reach the queue sort or the current-task pick
        // (#3856, #3882).
        #[test]
        fn the_reads_agree_with_the_timestamp_validator(
            created_at in created_at_text(),
            updated_at in created_at_text(),
        ) {
            let dir = TempDir::new().expect("test");
            write_task_at(dir.path(), "p", &created_at, &updated_at);
            let valid = llmenv_util::time::validate_stored(&created_at).is_ok()
                && llmenv_util::time::validate_stored(&updated_at).is_ok();
            let strict = read_tasks(dir.path(), UnreadablePolicy::Fail);
            let lenient = read_tasks(dir.path(), UnreadablePolicy::Skip).expect("lenient read");
            prop_assert_eq!(strict.is_ok(), valid);
            prop_assert_eq!(lenient.len(), usize::from(valid));
        }

        // Every `.json` file is kept or skipped, so the two counts add up, and the first problem
        // exists exactly when a file was skipped (#3883).
        #[test]
        fn the_counted_read_keeps_or_skips_every_file(kinds in proptest::collection::vec(0u8..4, 0..8)) {
            let dir = TempDir::new().expect("test");
            for (i, kind) in kinds.iter().enumerate() {
                let slug = format!("t{i}");
                match kind {
                    0 => { write_task(dir.path(), &slug, VALID_AT); }
                    1 => { write_task(dir.path(), &slug, "bad"); }
                    2 => { write_task_at(dir.path(), &slug, VALID_AT, "bad"); }
                    _ => {
                        std::fs::create_dir_all(tasks_dir(dir.path())).expect("test");
                        std::fs::write(tasks_dir(dir.path()).join(format!("{slug}.json")), "{ x")
                            .expect("test");
                    }
                }
            }
            let read = try_list_tasks_counted(dir.path()).expect("lenient read");
            let kept = kinds.iter().filter(|k| **k == 0).count();
            prop_assert_eq!(read.tasks.len(), kept);
            prop_assert_eq!(read.tasks.len() + read.skipped, kinds.len());
            prop_assert_eq!(read.first_problem.is_none(), read.skipped == 0);
            let strict = read_tasks(dir.path(), UnreadablePolicy::Fail);
            prop_assert_eq!(strict.is_ok(), read.skipped == 0);
        }
    }
}
