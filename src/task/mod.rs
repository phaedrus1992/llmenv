//! In-engine task tracker (#231): a file-based task store, one JSON file per
//! task under `state_dir()/tasks/<slug>.json`.
//!
//! Concurrent `llmenv task` invocations (e.g. from multiple Claude Code
//! sessions on the same project) are serialized via a single advisory lock
//! file at `<tasks_dir>/.lock`, held for the duration of each mutating
//! operation's full read-modify-write. Coarse-grained (whole-store, not
//! per-task) — simplest correct option for this scale; no new dependency,
//! `std::fs::File::lock()` (stable since Rust 1.89) is the stdlib flock/
//! LockFileEx wrapper.
//! ponytail: per-task locking (rather than whole-store) if write throughput
//! ever becomes a real bottleneck — unlikely for a CLI task tracker.

pub mod project;
pub mod session;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lifecycle state of a tracked task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    #[default]
    Open,
    Wip,
    /// Blocked on something outside the agent's control — typically human
    /// input (a review, a decision, external system access) — that no
    /// amount of further autonomous action will resolve. Distinct from
    /// `Wip` so the Stop-hook reminder doesn't nag to "take action" on it:
    /// the correct behavior here is to actually wait, not keep retrying.
    /// Resume with `start_task` once the blocker clears (it accepts any
    /// non-`Done` state as its starting point).
    Waiting,
    Done,
}

/// A timestamped progress note attached to a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskNote {
    /// RFC3339 timestamp.
    pub at: String,
    pub text: String,
}

/// A single tracked task, persisted as one JSON file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub state: TaskState,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub blocked_on: Vec<String>,
    #[serde(default)]
    pub notes: Vec<TaskNote>,
    /// Id of the session active when this task was created (`None` for a
    /// task added outside any session, or created before sessions existed —
    /// `#[serde(default)]` keeps old task files loadable). Set once at
    /// creation and never changed afterward, so a task's session membership
    /// reflects when it was started, not whatever session happens to be
    /// active later.
    #[serde(default)]
    pub session: Option<String>,
    /// RFC3339 timestamp.
    pub created_at: String,
    /// RFC3339 timestamp.
    pub updated_at: String,
}

/// The task-store subdirectory under llmenv's state dir.
pub fn tasks_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("tasks")
}

fn task_path(state_dir: &Path, slug: &str) -> PathBuf {
    tasks_dir(state_dir).join(format!("{slug}.json"))
}

/// Run `f` while holding an exclusive lock on the whole task store, so
/// concurrent `llmenv task` invocations (e.g. from multiple Claude Code
/// sessions on the same project) serialize their read-modify-write cycles
/// instead of racing on the same file. Blocks until the lock is acquired.
fn with_store_lock<T>(
    state_dir: &Path,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = tasks_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))?;
    lock_file.lock()?;
    f()
}

/// Current RFC3339 timestamp (UTC, second precision).
fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}

/// Derive a kebab-case slug from a task title: lowercase, first ~6 words,
/// non-alphanumeric runs collapsed to a single `-`, leading/trailing `-`
/// trimmed. Pure function — collision uniquification happens separately in
/// [`unique_slug`], which needs the store directory.
pub fn slugify(title: &str) -> String {
    let words: Vec<&str> = title.split_whitespace().take(6).collect();
    let joined = words.join(" ");
    let mut slug = String::with_capacity(joined.len());
    let mut last_was_sep = true; // suppress a leading '-'
    for c in joined.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('-');
            last_was_sep = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Uniquify `base_slug` against existing task files in `dir` by appending
/// `-2`, `-3`, ... on collision.
fn unique_slug(dir: &Path, base_slug: &str) -> String {
    if !dir.join(format!("{base_slug}.json")).exists() {
        return base_slug.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base_slug}-{n}");
        if !dir.join(format!("{candidate}.json")).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Write a task to disk atomically. Callers that mutate an existing task
/// (rather than just persisting one already exclusively held under
/// [`with_store_lock`]) are expected to call this from within that lock.
pub fn save_task(state_dir: &Path, task: &Task) -> anyhow::Result<()> {
    let dir = tasks_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(task)?;
    crate::paths::write_owner_only_atomic(&task_path(state_dir, &task.slug), json.as_bytes())?;
    Ok(())
}

/// Load a single task by its exact slug.
pub fn load_task(state_dir: &Path, slug: &str) -> anyhow::Result<Task> {
    let content = std::fs::read_to_string(task_path(state_dir, slug))?;
    Ok(serde_json::from_str(&content)?)
}

/// List all tasks in the store. Corrupt or unreadable files are skipped with
/// a stderr warning rather than failing the whole listing — a single bad
/// file must never block `llmenv task ls` or a hook.
pub fn list_tasks(state_dir: &Path) -> Vec<Task> {
    let dir = tasks_dir(state_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("llmenv: failed to read tasks dir {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|content| Ok(serde_json::from_str::<Task>(&content)?))
        {
            Ok(task) => tasks.push(task),
            Err(e) => eprintln!("llmenv: skipping corrupt task file {}: {e}", path.display()),
        }
    }
    tasks
}

/// Count of tasks currently `open` or `wip`, store-wide (not scoped to any
/// session) — the statusline's "no active session" fallback (#905).
#[must_use]
pub fn open_task_count(state_dir: &Path) -> u64 {
    list_tasks(state_dir)
        .into_iter()
        .filter(|t| t.state != TaskState::Done)
        .count() as u64
}

/// Title of the most recently updated `wip`/`waiting` task — the
/// statusline's "what's in progress right now" fill-in (#905). Pass
/// `session_id` to scope the search to one session's tasks (matching how
/// `open`/`session_progress` are scoped for the same widget); `None`
/// searches store-wide. `None` when nothing matching is currently
/// `wip`/`waiting`.
#[must_use]
pub fn current_wip_title(state_dir: &Path, session_id: Option<&str>) -> Option<String> {
    list_tasks(state_dir)
        .into_iter()
        .filter(|t| matches!(t.state, TaskState::Wip | TaskState::Waiting))
        .filter(|t| session_id.is_none_or(|sid| t.session.as_deref() == Some(sid)))
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .map(|t| t.title)
}

/// Create a new task in `open` state and persist it.
///
/// # Errors
/// Errors if `parent` is provided but doesn't resolve to an existing task —
/// same eager-validation reasoning as `block_task`'s `on` (a fresh write,
/// not a load of possibly-stale state, so a typo'd parent is caught
/// immediately rather than becoming a silent dangling reference).
pub fn add_task(state_dir: &Path, title: &str, parent: Option<&str>) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let dir = tasks_dir(state_dir);
        let parent_slug = match parent {
            Some(p) => Some(resolve_identifier(state_dir, p)?),
            None => None,
        };
        let now = now_rfc3339();
        let mut base_slug = slugify(title);
        if base_slug.is_empty() {
            // A title with no ASCII-alphanumeric characters at all (e.g. a
            // CJK-only title, or pure punctuation) would otherwise collapse
            // to an empty slug — a hidden `.json` file that's awkward to
            // reference. Fall back to a timestamp-derived slug instead.
            base_slug = format!("task-{}", now.replace([':', '-'], ""));
        }
        let slug = unique_slug(&dir, &base_slug);
        let task = Task {
            slug,
            title: title.to_string(),
            state: TaskState::Open,
            parent: parent_slug,
            blocked_on: Vec::new(),
            notes: Vec::new(),
            session: session::active_session(state_dir).map(|s| s.id),
            created_at: now.clone(),
            updated_at: now,
        };
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// Resolve a user-supplied identifier (exact slug or unambiguous prefix) to
/// the exact slug of an existing task.
///
/// # Errors
/// Returns an error if `input` isn't a safe single path component (rejects
/// path traversal / absolute-path attempts before any path is constructed —
/// a task slug is always a single component), if no task matches, or if the
/// prefix matches more than one task (the error lists every candidate slug).
pub fn resolve_identifier(state_dir: &Path, input: &str) -> anyhow::Result<String> {
    if !crate::paths::is_valid_short_name(input) {
        anyhow::bail!("'{input}' is not a valid task identifier");
    }
    if task_path(state_dir, input).exists() {
        return Ok(input.to_string());
    }
    let matches: Vec<String> = list_tasks(state_dir)
        .into_iter()
        .filter(|t| t.slug.starts_with(input))
        .map(|t| t.slug)
        .collect();
    match matches.len() {
        0 => anyhow::bail!("no task found matching '{input}'"),
        1 => Ok(matches[0].clone()),
        _ => {
            let mut sorted = matches;
            sorted.sort();
            anyhow::bail!("'{input}' matches multiple tasks: {}", sorted.join(", "))
        }
    }
}

/// Claim a task, transitioning it to `wip`.
///
/// # Errors
/// Errors if the task is already `done`. Warns (but still allows) starting a
/// task whose `blocked_on` list contains a non-`done` task — the agent may
/// know better than the ordering hint.
pub fn start_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        if task.state == TaskState::Done {
            anyhow::bail!("task '{slug}' is already done; cannot start it again");
        }
        for blocker_slug in &task.blocked_on {
            match load_task(state_dir, blocker_slug) {
                Ok(blocker) if blocker.state != TaskState::Done => {
                    eprintln!(
                        "llmenv: warning: '{slug}' is blocked on '{blocker_slug}' ({:?}, not done) — starting anyway",
                        blocker.state
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    // Dangling blocked_on reference (deleted/corrupt blocker
                    // file) — warn and treat the edge as absent, matching the
                    // load-time tolerance documented for parent/blocked_on
                    // slugs.
                    eprintln!(
                        "llmenv: warning: '{slug}' is blocked on '{blocker_slug}', which could not be loaded ({e}) — starting anyway"
                    );
                }
            }
        }
        task.state = TaskState::Wip;
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// Delete a task outright (#905) — for a task that's being deliberately
/// abandoned, not just reshuffled. Returns the deleted task. Doesn't touch
/// other tasks' `parent`/`blocked_on` references to it, which already
/// tolerate a dangling target the same way a deleted blocker does (see
/// `start_task`'s warning path).
pub fn delete_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let task = load_task(state_dir, &slug)?;
        std::fs::remove_file(task_path(state_dir, &slug))?;
        Ok(task)
    })
}

/// Mark a task done. Idempotent from any prior state (fast-path completion).
pub fn done_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        task.state = TaskState::Done;
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// Append a timestamped progress note to a task.
pub fn note_task(state_dir: &Path, input: &str, text: &str) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        task.notes.push(TaskNote {
            at: now_rfc3339(),
            text: text.to_string(),
        });
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// Mark a task `waiting` — blocked on something outside the agent's control
/// (typically human input) rather than actively being worked. `reason` is
/// recorded as a note so `llmenv task show` carries the context. Resume with
/// [`start_task`], which accepts `waiting` as a valid prior state.
///
/// # Errors
/// Errors if the task is already done.
pub fn wait_task(state_dir: &Path, input: &str, reason: &str) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        if task.state == TaskState::Done {
            anyhow::bail!("task '{slug}' is already done; cannot mark it waiting");
        }
        task.state = TaskState::Waiting;
        task.notes.push(TaskNote {
            at: now_rfc3339(),
            text: format!("Waiting: {reason}"),
        });
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// Record an ordering dependency: `input` is blocked on `on`.
///
/// # Errors
/// Errors if `on` doesn't resolve to an existing task — this is a fresh
/// write, not a load of possibly-stale state, so it's validated eagerly
/// (unlike the load-time tolerance for dangling `blocked_on` entries left
/// behind by a since-deleted task file). Also errors if `input` and `on`
/// resolve to the same task — a task cannot block itself.
pub fn block_task(state_dir: &Path, input: &str, on: &str) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let on_slug = resolve_identifier(state_dir, on)?;
        if slug == on_slug {
            anyhow::bail!("task '{slug}' cannot be blocked on itself");
        }
        let mut task = load_task(state_dir, &slug)?;
        if !task.blocked_on.contains(&on_slug) {
            task.blocked_on.push(on_slug);
        }
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// SessionStart hook: if any `wip` tasks exist, build a reminder nudging the
/// agent to resume or close them before starting new work. Empty string when
/// there are none, or on any internal error (logged to stderr, never
/// propagated — hooks must never block the agent).
pub fn session_start_reminder(state_dir: &Path) -> String {
    combine_reminders([
        wip_reminder(
            state_dir,
            "In-progress tasks from a previous session",
            "Resume one of these or run `llmenv task done <slug>` before starting new work.",
        ),
        session_finish_reminder(state_dir),
    ])
}

/// Stop hook (end-of-turn skip detection): if `wip` tasks remain at the end
/// of a turn, remind the agent to update or finish them.
///
/// First cut: flags any remaining `wip` task, same underlying check as
/// [`session_start_reminder`]. The design doc's fuller heuristic — only fire
/// when *this session* touched the task store (via file mtimes within the
/// session window) — is deliberately deferred: the current session has no
/// cheap way to distinguish "I started this task" from "a task was already
/// wip when I woke up" without threading session_id through task state, and
/// firing on every wip task each Stop is a reasonable, simpler starting
/// behavior (advisory-only, never blocks).
/// ponytail: add session-scoped mtime filtering if the blanket reminder
/// proves too chatty in practice.
pub fn stop_hook_reminder(state_dir: &Path) -> String {
    combine_reminders([
        wip_reminder(
            state_dir,
            "You still have task(s) in progress",
            "Run `llmenv task done <slug>` when finished. If still working, keep going — \
             don't stop mid-task. If blocked, exhaust safe autonomous remediation first \
             (retry, an alternate approach, a diagnostic command); only then ask the user, \
             once, with a specific actionable question, and `llmenv task note <slug> \"...\"` \
             the blocker instead of repeating the same status every turn.",
        ),
        session_finish_reminder(state_dir),
    ])
}

/// If a task session (`llmenv task session start`) is active and every task
/// tagged to it is done, build a reminder nudging the agent to close the
/// session out — or add more work to it instead, if the session isn't
/// actually finished. Empty string when no session is active, it has no
/// tasks yet, or it still has open/in-progress tasks.
fn session_finish_reminder(state_dir: &Path) -> String {
    let Some(active) = session::active_session(state_dir) else {
        return String::new();
    };
    let (done, total) = session::session_progress(state_dir, &active.id);
    if total == 0 || done < total {
        return String::new();
    }
    let label = active.name.as_deref().unwrap_or(active.id.as_str());
    format!(
        "All {total} task(s) in session '{label}' are done — run \
         `llmenv task session finish` to close it out, or `llmenv task add <title>` \
         to add more work to it."
    )
}

/// Join non-empty reminder strings with a blank line between them; empty
/// parts are dropped rather than leaving stray blank lines.
fn combine_reminders(parts: [String; 2]) -> String {
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Builds the `wip`-task reminder (`header`/`footer` customized per caller,
/// pushing toward action) and, separately, a `waiting`-task note. The two
/// are deliberately different in tone: a `wip` task is actionable right now,
/// a `waiting` one is correctly paused on something outside the agent's
/// control — nagging to "take action" on it would be actively wrong, so it
/// gets a plain FYI instead, never the action-pushing footer.
fn wip_reminder(state_dir: &Path, header: &str, footer: &str) -> String {
    let tasks = list_tasks(state_dir);
    let wip: Vec<&Task> = tasks.iter().filter(|t| t.state == TaskState::Wip).collect();
    let waiting: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.state == TaskState::Waiting)
        .collect();

    let render = |tasks: &[&Task]| -> String {
        tasks
            .iter()
            .map(|t| format!("- {} ({})", t.title, t.slug))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut parts = Vec::new();
    if !wip.is_empty() {
        parts.push(format!("{header}:\n{}\n{footer}", render(&wip)));
    }
    if !waiting.is_empty() {
        parts.push(format!(
            "Task(s) waiting on external input (no action needed until it \
             clears — see each task's notes for what's being waited on):\n{}",
            render(&waiting)
        ));
    }
    parts.join("\n\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn task_state_default_is_open() {
        assert_eq!(TaskState::default(), TaskState::Open);
    }

    #[test]
    fn task_state_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&TaskState::Wip).unwrap(), "\"wip\"");
    }

    #[test]
    fn slugify_basic_title() {
        assert_eq!(slugify("Fix login timeout"), "fix-login-timeout");
    }

    #[test]
    fn slugify_truncates_to_six_words() {
        assert_eq!(
            slugify("one two three four five six seven eight"),
            "one-two-three-four-five-six"
        );
    }

    #[test]
    fn slugify_strips_punctuation() {
        assert_eq!(slugify("Fix: login/timeout bug!"), "fix-login-timeout-bug");
    }

    #[test]
    fn slugify_collapses_whitespace_and_trims_hyphens() {
        assert_eq!(slugify("  --weird   title--  "), "weird-title");
    }

    #[test]
    fn add_task_creates_file_with_open_state() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(task.slug, "fix-login-timeout");
        assert_eq!(task.state, TaskState::Open);
        assert!(task.parent.is_none());

        let loaded = load_task(dir.path(), "fix-login-timeout").expect("test");
        assert_eq!(loaded, task);
    }

    #[test]
    fn add_task_uniquifies_slug_on_collision() {
        let dir = TempDir::new().expect("test");
        let t1 = add_task(dir.path(), "Fix login timeout", None).expect("test");
        let t2 = add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(t1.slug, "fix-login-timeout");
        assert_eq!(t2.slug, "fix-login-timeout-2");
    }

    /// Concurrency regression: multiple threads racing `add_task` with the
    /// *same* title (the exact scenario multiple Claude Code sessions on one
    /// project would hit) must never lose a task — the store lock serializes
    /// each read-modify-write so `unique_slug` always sees every prior
    /// writer's file before picking a suffix.
    #[test]
    fn concurrent_add_task_same_title_never_loses_a_task() {
        let dir = TempDir::new().expect("test");
        let dir_path = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir_path = dir_path.clone();
                std::thread::spawn(move || {
                    add_task(&dir_path, "Race condition task", None).expect("test")
                })
            })
            .collect();
        let mut slugs: Vec<String> = threads
            .into_iter()
            .map(|h| h.join().expect("thread panicked").slug)
            .collect();
        slugs.sort();
        slugs.dedup();
        assert_eq!(
            slugs.len(),
            8,
            "every concurrent add_task must produce a distinct task"
        );
        assert_eq!(
            list_tasks(dir.path()).len(),
            8,
            "no task file was lost to a lost update"
        );
    }

    /// Concurrency regression: two threads racing `start_task` on the *same*
    /// task must not lose the transition or corrupt the file — the lock
    /// serializes the whole load-mutate-save cycle per operation.
    #[test]
    fn concurrent_start_task_same_task_is_serialized_not_corrupted() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Shared task", None).expect("test");
        let dir_path = dir.path().to_path_buf();
        let slug = task.slug.clone();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir_path = dir_path.clone();
                let slug = slug.clone();
                std::thread::spawn(move || start_task(&dir_path, &slug))
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|h| h.join().expect("test"))
            .collect();
        assert!(
            results.iter().all(std::result::Result::is_ok),
            "start_task on an open task must always succeed, even racing itself"
        );
        let reloaded = load_task(dir.path(), &slug).expect("test");
        assert_eq!(reloaded.state, TaskState::Wip);
    }

    #[test]
    fn add_task_with_parent() {
        let dir = TempDir::new().expect("test");
        let parent = add_task(dir.path(), "Parent task", None).expect("test");
        let child = add_task(dir.path(), "Child task", Some(&parent.slug)).expect("test");
        assert_eq!(child.parent, Some(parent.slug));
    }

    #[test]
    fn add_task_with_unknown_parent_errors() {
        let dir = TempDir::new().expect("test");
        assert!(add_task(dir.path(), "Orphan", Some("no-such-parent")).is_err());
    }

    #[test]
    fn add_task_parent_accepts_unambiguous_prefix() {
        let dir = TempDir::new().expect("test");
        let parent = add_task(dir.path(), "Umbrella project", None).expect("test");
        let child = add_task(dir.path(), "Sub piece", Some("umbrella")).expect("test");
        assert_eq!(child.parent, Some(parent.slug));
    }

    #[test]
    fn nested_chain_of_three_levels_resolves_correctly() {
        let dir = TempDir::new().expect("test");
        let grandparent = add_task(dir.path(), "Epic", None).expect("test");
        let parent = add_task(dir.path(), "Story", Some(&grandparent.slug)).expect("test");
        let child = add_task(dir.path(), "Subtask", Some(&parent.slug)).expect("test");

        assert_eq!(grandparent.parent, None);
        assert_eq!(parent.parent, Some(grandparent.slug.clone()));
        assert_eq!(child.parent, Some(parent.slug.clone()));

        // Walk the chain back up via load_task, as a consumer would.
        let loaded_parent =
            load_task(dir.path(), child.parent.as_ref().expect("test")).expect("test");
        assert_eq!(loaded_parent.slug, parent.slug);
        let loaded_grandparent =
            load_task(dir.path(), loaded_parent.parent.as_ref().expect("test")).expect("test");
        assert_eq!(loaded_grandparent.slug, grandparent.slug);
        assert_eq!(loaded_grandparent.parent, None);
    }

    #[test]
    fn multiple_children_under_one_parent_all_listed() {
        let dir = TempDir::new().expect("test");
        let parent = add_task(dir.path(), "Parent with many kids", None).expect("test");
        let child_a = add_task(dir.path(), "Child A", Some(&parent.slug)).expect("test");
        let child_b = add_task(dir.path(), "Child B", Some(&parent.slug)).expect("test");
        let child_c = add_task(dir.path(), "Child C", Some(&parent.slug)).expect("test");

        let children: Vec<Task> = list_tasks(dir.path())
            .into_iter()
            .filter(|t| t.parent.as_deref() == Some(parent.slug.as_str()))
            .collect();
        assert_eq!(children.len(), 3);
        let mut slugs: Vec<&str> = children.iter().map(|t| t.slug.as_str()).collect();
        slugs.sort();
        let mut expected = [
            child_a.slug.as_str(),
            child_b.slug.as_str(),
            child_c.slug.as_str(),
        ];
        expected.sort_unstable();
        assert_eq!(slugs, expected);
    }

    #[test]
    fn starting_and_completing_a_child_does_not_affect_parent_state() {
        let dir = TempDir::new().expect("test");
        let parent = add_task(dir.path(), "Parent", None).expect("test");
        let child = add_task(dir.path(), "Child", Some(&parent.slug)).expect("test");
        start_task(dir.path(), &child.slug).expect("test");
        done_task(dir.path(), &child.slug).expect("test");

        let reloaded_parent = load_task(dir.path(), &parent.slug).expect("test");
        assert_eq!(reloaded_parent.state, TaskState::Open);
    }

    #[test]
    fn list_tasks_skips_corrupt_files_with_warning() {
        let dir = TempDir::new().expect("test");
        add_task(dir.path(), "Good task", None).expect("test");
        std::fs::create_dir_all(tasks_dir(dir.path())).expect("test");
        std::fs::write(tasks_dir(dir.path()).join("bad.json"), b"not json").expect("test");

        let tasks = list_tasks(dir.path());
        assert_eq!(tasks.len(), 1, "corrupt file must be skipped, not crash ls");
        assert_eq!(tasks[0].title, "Good task");
    }

    #[test]
    fn list_tasks_empty_dir_returns_empty() {
        let dir = TempDir::new().expect("test");
        assert!(list_tasks(dir.path()).is_empty());
    }

    #[test]
    fn resolve_identifier_exact_slug() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(
            resolve_identifier(dir.path(), &task.slug).expect("test"),
            task.slug
        );
    }

    #[test]
    fn resolve_identifier_unambiguous_prefix() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(
            resolve_identifier(dir.path(), "fix-log").expect("test"),
            task.slug
        );
    }

    #[test]
    fn resolve_identifier_ambiguous_prefix_errors_listing_candidates() {
        let dir = TempDir::new().expect("test");
        add_task(dir.path(), "Fix login timeout", None).expect("test");
        add_task(dir.path(), "Fix logout crash", None).expect("test");
        let err = resolve_identifier(dir.path(), "fix-log")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fix-login-timeout"));
        assert!(err.contains("fix-logout-crash"));
    }

    #[test]
    fn resolve_identifier_rejects_path_traversal() {
        let dir = TempDir::new().expect("test");
        let outside = dir.path().parent().expect("test").join("escaped.json");
        assert!(
            resolve_identifier(dir.path(), "../escaped").is_err(),
            "must reject a '..' identifier before touching the filesystem"
        );
        assert!(
            !outside.exists(),
            "must never create a file outside tasks_dir"
        );
    }

    #[test]
    fn resolve_identifier_rejects_absolute_path() {
        let dir = TempDir::new().expect("test");
        assert!(resolve_identifier(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn resolve_identifier_no_match_errors() {
        let dir = TempDir::new().expect("test");
        add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert!(resolve_identifier(dir.path(), "nope").is_err());
    }

    #[test]
    fn start_task_transitions_open_to_wip() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        let started = start_task(dir.path(), &task.slug).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn start_task_on_done_errors() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        done_task(dir.path(), &task.slug).expect("test");
        assert!(start_task(dir.path(), &task.slug).is_err());
    }

    #[test]
    fn done_task_from_open_allowed() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        let done = done_task(dir.path(), &task.slug).expect("test");
        assert_eq!(done.state, TaskState::Done);
    }

    #[test]
    fn start_task_warns_but_allows_when_blocked_on_open_task() {
        let dir = TempDir::new().expect("test");
        let blocker = add_task(dir.path(), "Blocker task", None).expect("test");
        let task = add_task(dir.path(), "Blocked task", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        // Not an error — warning only, transition still allowed.
        let started = start_task(dir.path(), &task.slug).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn note_task_appends_note() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        let updated = note_task(dir.path(), &task.slug, "made progress").expect("test");
        assert_eq!(updated.notes.len(), 1);
        assert_eq!(updated.notes[0].text, "made progress");
    }

    #[test]
    fn wait_task_transitions_to_waiting_and_notes_reason() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        start_task(dir.path(), &task.slug).expect("test");
        let waiting = wait_task(dir.path(), &task.slug, "need spec review").expect("test");
        assert_eq!(waiting.state, TaskState::Waiting);
        assert_eq!(waiting.notes.len(), 1);
        assert!(waiting.notes[0].text.contains("need spec review"));
    }

    #[test]
    fn wait_task_on_done_errors() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        done_task(dir.path(), &task.slug).expect("test");
        assert!(wait_task(dir.path(), &task.slug, "too late").is_err());
    }

    #[test]
    fn start_task_resumes_from_waiting() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Do thing", None).expect("test");
        wait_task(dir.path(), &task.slug, "blocked").expect("test");
        let resumed = start_task(dir.path(), &task.slug).expect("test");
        assert_eq!(resumed.state, TaskState::Wip);
    }

    #[test]
    fn block_task_records_dependency() {
        let dir = TempDir::new().expect("test");
        let blocker = add_task(dir.path(), "Blocker", None).expect("test");
        let task = add_task(dir.path(), "Blocked", None).expect("test");
        let updated = block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        assert_eq!(updated.blocked_on, vec![blocker.slug]);
    }

    #[test]
    fn block_task_on_unknown_target_errors() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Blocked", None).expect("test");
        assert!(block_task(dir.path(), &task.slug, "no-such-task").is_err());
    }

    #[test]
    fn block_task_on_self_errors() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Solo task", None).expect("test");
        assert!(block_task(dir.path(), &task.slug, &task.slug).is_err());
    }

    #[test]
    fn add_task_with_no_alphanumeric_title_falls_back_to_timestamp_slug() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "!!!", None).expect("test");
        assert!(!task.slug.is_empty());
        assert!(task.slug.starts_with("task-"));
        let loaded = load_task(dir.path(), &task.slug).expect("test");
        assert_eq!(loaded, task);
    }

    #[test]
    fn start_task_blocked_on_deleted_task_warns_but_allows() {
        let dir = TempDir::new().expect("test");
        let blocker = add_task(dir.path(), "Blocker", None).expect("test");
        let task = add_task(dir.path(), "Blocked", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        std::fs::remove_file(
            dir.path()
                .join("tasks")
                .join(format!("{}.json", blocker.slug)),
        )
        .expect("test");
        let started = start_task(dir.path(), &task.slug).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn session_start_reminder_empty_when_no_wip_tasks() {
        let dir = TempDir::new().expect("test");
        add_task(dir.path(), "Open task", None).expect("test");
        assert!(session_start_reminder(dir.path()).is_empty());
    }

    #[test]
    fn session_start_reminder_lists_wip_tasks() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "In progress task", None).expect("test");
        start_task(dir.path(), &task.slug).expect("test");
        let reminder = session_start_reminder(dir.path());
        assert!(reminder.contains("In progress task"));
        assert!(reminder.contains(&task.slug));
    }

    #[test]
    fn session_start_reminder_empty_on_missing_state_dir() {
        let dir = TempDir::new().expect("test");
        let missing = dir.path().join("does-not-exist");
        assert!(session_start_reminder(&missing).is_empty());
    }

    #[test]
    fn stop_hook_reminder_empty_when_no_wip_tasks() {
        let dir = TempDir::new().expect("test");
        assert!(stop_hook_reminder(dir.path()).is_empty());
    }

    #[test]
    fn stop_hook_reminder_flags_wip_tasks() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Left in progress", None).expect("test");
        start_task(dir.path(), &task.slug).expect("test");
        let reminder = stop_hook_reminder(dir.path());
        assert!(reminder.contains(&task.slug));
    }

    #[test]
    fn stop_hook_reminder_waiting_task_gets_no_action_framing() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Blocked on review", None).expect("test");
        wait_task(dir.path(), &task.slug, "spec review").expect("test");
        let reminder = stop_hook_reminder(dir.path());
        assert!(reminder.contains(&task.slug));
        assert!(reminder.contains("no action needed"));
        // Must not carry the action-pushing footer meant for genuinely wip tasks.
        assert!(!reminder.contains("exhaust safe autonomous remediation"));
    }

    #[test]
    fn stop_hook_reminder_separates_wip_and_waiting_tasks() {
        let dir = TempDir::new().expect("test");
        let wip = add_task(dir.path(), "Actively working", None).expect("test");
        start_task(dir.path(), &wip.slug).expect("test");
        let waiting = add_task(dir.path(), "Blocked on review", None).expect("test");
        wait_task(dir.path(), &waiting.slug, "spec review").expect("test");

        let reminder = stop_hook_reminder(dir.path());
        assert!(reminder.contains(&wip.slug));
        assert!(reminder.contains(&waiting.slug));
        assert!(reminder.contains("exhaust safe autonomous remediation"));
        assert!(reminder.contains("no action needed"));
    }

    // Task/TaskNote/TaskState derive Serialize/Deserialize and persist as
    // JSON. A serde roundtrip must be lossless — a drifted derive (renamed
    // field, wrong rename attr) would silently corrupt a user's task store.
    // Also covers slugify/resolve_identifier/nesting invariants (#231
    // pre-pr-review property-test-gap-finder pass).
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_task_state() -> impl Strategy<Value = TaskState> {
            prop_oneof![
                Just(TaskState::Open),
                Just(TaskState::Wip),
                Just(TaskState::Waiting),
                Just(TaskState::Done),
            ]
        }

        fn arb_task_note() -> impl Strategy<Value = TaskNote> {
            (".{0,40}", ".{0,80}").prop_map(|(at, text)| TaskNote { at, text })
        }

        fn arb_task() -> impl Strategy<Value = Task> {
            (
                ".{1,30}",
                ".{1,60}",
                arb_task_state(),
                proptest::option::of(".{1,30}"),
                proptest::collection::vec(".{1,30}", 0..4),
                proptest::collection::vec(arb_task_note(), 0..4),
                proptest::option::of(".{1,30}"),
                ".{1,30}",
                ".{1,30}",
            )
                .prop_map(
                    |(
                        slug,
                        title,
                        state,
                        parent,
                        blocked_on,
                        notes,
                        session,
                        created_at,
                        updated_at,
                    )| {
                        Task {
                            slug,
                            title,
                            state,
                            parent,
                            blocked_on,
                            notes,
                            session,
                            created_at,
                            updated_at,
                        }
                    },
                )
        }

        proptest! {
            #[test]
            fn task_note_json_roundtrips(note in arb_task_note()) {
                let json = serde_json::to_string(&note).unwrap();
                let back: TaskNote = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back, note);
            }

            #[test]
            fn task_state_json_roundtrips(state in arb_task_state()) {
                let json = serde_json::to_string(&state).unwrap();
                let back: TaskState = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back, state);
            }

            #[test]
            fn task_json_roundtrips(task in arb_task()) {
                let json = serde_json::to_string(&task).unwrap();
                let back: Task = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back, task);
            }

            #[test]
            fn slugify_output_is_lowercase_alnum_and_hyphen_only(title in ".{0,80}") {
                let slug = slugify(&title);
                prop_assert!(slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
            }

            #[test]
            fn slugify_never_starts_or_ends_with_hyphen(title in ".{0,80}") {
                let slug = slugify(&title);
                prop_assert!(!slug.starts_with('-'));
                prop_assert!(!slug.ends_with('-'));
            }

            #[test]
            fn slugify_is_idempotent(title in ".{0,80}") {
                let once = slugify(&title);
                let twice = slugify(&once);
                prop_assert_eq!(once, twice);
            }

            #[test]
            fn slugify_only_derived_from_first_six_whitespace_words(
                words in proptest::collection::vec("[a-zA-Z0-9]{1,8}", 0..12)
            ) {
                let title = words.join(" ");
                let full = slugify(&title);
                let truncated_title = words.iter().take(6).cloned().collect::<Vec<_>>().join(" ");
                let truncated = slugify(&truncated_title);
                // Appending more whitespace-delimited words beyond the 6th
                // must never change the slug — slugify only reads the first 6.
                prop_assert_eq!(full, truncated);
            }

            #[test]
            fn resolve_identifier_finds_every_added_tasks_own_slug(
                titles in proptest::collection::vec("[a-zA-Z]{3,12}", 1..6)
            ) {
                let dir = tempfile::TempDir::new().unwrap();
                let mut slugs = Vec::new();
                for title in &titles {
                    let task = add_task(dir.path(), title, None).unwrap();
                    slugs.push(task.slug);
                }
                for slug in &slugs {
                    prop_assert_eq!(resolve_identifier(dir.path(), slug).unwrap(), slug.clone());
                }
            }

            #[test]
            fn nested_chain_of_arbitrary_depth_links_correctly(
                titles in proptest::collection::vec("[a-zA-Z]{3,12}", 1..8)
            ) {
                let dir = tempfile::TempDir::new().unwrap();
                let mut prev_slug: Option<String> = None;
                let mut chain = Vec::new();
                for title in &titles {
                    let task = add_task(dir.path(), title, prev_slug.as_deref()).unwrap();
                    prop_assert_eq!(&task.parent, &prev_slug);
                    prev_slug = Some(task.slug.clone());
                    chain.push(task);
                }
                // Walk the chain back from the deepest task to the root,
                // confirming every parent link resolves and matches.
                for (i, task) in chain.iter().enumerate().rev() {
                    if i == 0 {
                        prop_assert_eq!(&task.parent, &None);
                    } else {
                        prop_assert_eq!(task.parent.as_deref(), Some(chain[i - 1].slug.as_str()));
                        let loaded = load_task(dir.path(), task.parent.as_ref().unwrap()).unwrap();
                        prop_assert_eq!(loaded.slug, chain[i - 1].slug.clone());
                    }
                }
            }
        }
    }
}
