//! In-engine task tracker (#231): a file-based task store, one JSON file per
//! task under `state_dir()/tasks/<slug>.json`.
//!
//! Single-writer assumption: no file locking. Concurrent `llmenv task`
//! invocations against the same task can race (last write wins). Fine for
//! the single-agent-per-session model this targets.
//! ponytail: add per-task file locking (e.g. `fs4`) if multi-agent
//! concurrent writers become a real scenario.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lifecycle state of a tracked task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    #[default]
    Open,
    Wip,
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

/// Write a task to disk atomically.
pub fn save_task(state_dir: &Path, task: &Task) -> anyhow::Result<()> {
    let dir = tasks_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(task)?;
    // ponytail: single-writer assumption, no file lock — see module doc.
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

/// Create a new task in `open` state and persist it.
pub fn add_task(state_dir: &Path, title: &str, parent: Option<&str>) -> anyhow::Result<Task> {
    let dir = tasks_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let base_slug = slugify(title);
    let slug = unique_slug(&dir, &base_slug);
    let now = now_rfc3339();
    let task = Task {
        slug,
        title: title.to_string(),
        state: TaskState::Open,
        parent: parent.map(str::to_string),
        blocked_on: Vec::new(),
        notes: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
    };
    save_task(state_dir, &task)?;
    Ok(task)
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
    let slug = resolve_identifier(state_dir, input)?;
    let mut task = load_task(state_dir, &slug)?;
    if task.state == TaskState::Done {
        anyhow::bail!("task '{slug}' is already done; cannot start it again");
    }
    for blocker_slug in &task.blocked_on {
        if let Ok(blocker) = load_task(state_dir, blocker_slug)
            && blocker.state != TaskState::Done
        {
            eprintln!(
                "llmenv: warning: '{slug}' is blocked on '{blocker_slug}' ({:?}, not done) — starting anyway",
                blocker.state
            );
        }
    }
    task.state = TaskState::Wip;
    task.updated_at = now_rfc3339();
    save_task(state_dir, &task)?;
    Ok(task)
}

/// Mark a task done. Idempotent from any prior state (fast-path completion).
pub fn done_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    let slug = resolve_identifier(state_dir, input)?;
    let mut task = load_task(state_dir, &slug)?;
    task.state = TaskState::Done;
    task.updated_at = now_rfc3339();
    save_task(state_dir, &task)?;
    Ok(task)
}

/// Append a timestamped progress note to a task.
pub fn note_task(state_dir: &Path, input: &str, text: &str) -> anyhow::Result<Task> {
    let slug = resolve_identifier(state_dir, input)?;
    let mut task = load_task(state_dir, &slug)?;
    task.notes.push(TaskNote {
        at: now_rfc3339(),
        text: text.to_string(),
    });
    task.updated_at = now_rfc3339();
    save_task(state_dir, &task)?;
    Ok(task)
}

/// Record an ordering dependency: `input` is blocked on `on`.
///
/// # Errors
/// Errors if `on` doesn't resolve to an existing task — this is a fresh
/// write, not a load of possibly-stale state, so it's validated eagerly
/// (unlike the load-time tolerance for dangling `blocked_on` entries left
/// behind by a since-deleted task file).
pub fn block_task(state_dir: &Path, input: &str, on: &str) -> anyhow::Result<Task> {
    let slug = resolve_identifier(state_dir, input)?;
    let on_slug = resolve_identifier(state_dir, on)?;
    let mut task = load_task(state_dir, &slug)?;
    if !task.blocked_on.contains(&on_slug) {
        task.blocked_on.push(on_slug);
    }
    task.updated_at = now_rfc3339();
    save_task(state_dir, &task)?;
    Ok(task)
}

/// SessionStart hook: if any `wip` tasks exist, build a reminder nudging the
/// agent to resume or close them before starting new work. Empty string when
/// there are none, or on any internal error (logged to stderr, never
/// propagated — hooks must never block the agent).
pub fn session_start_reminder(state_dir: &Path) -> String {
    wip_reminder(
        state_dir,
        "In-progress tasks from a previous session",
        "Resume one of these or run `llmenv task done <slug>` before starting new work.",
    )
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
    wip_reminder(
        state_dir,
        "You still have task(s) in progress",
        "Run `llmenv task done <slug>` when finished, or `llmenv task note <slug> \"...\"` \
         to record progress before this session ends.",
    )
}

fn wip_reminder(state_dir: &Path, header: &str, footer: &str) -> String {
    let wip: Vec<Task> = list_tasks(state_dir)
        .into_iter()
        .filter(|t| t.state == TaskState::Wip)
        .collect();
    if wip.is_empty() {
        return String::new();
    }
    let list = wip
        .iter()
        .map(|t| format!("- {} ({})", t.title, t.slug))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}:\n{list}\n{footer}")
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

    #[test]
    fn add_task_with_parent() {
        let dir = TempDir::new().expect("test");
        let parent = add_task(dir.path(), "Parent task", None).expect("test");
        let child = add_task(dir.path(), "Child task", Some(&parent.slug)).expect("test");
        assert_eq!(child.parent, Some(parent.slug));
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
}
