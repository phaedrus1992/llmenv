//! Task sessions (#905, reworked for mandatory sessions + project tagging —
//! docs/superpowers/specs/2026-07-21-task-project-scoping-design.md): every
//! task belongs to a session, and a session is tagged with the project it
//! was started in. Any number of sessions can be open at once (globally, and
//! per project via `--new`) — there is no more single "active session"
//! pointer. `task add`'s auto-resolve and `session start`'s checkpoint both
//! query "sessions open for this project" rather than a global singleton.
//!
//! One JSON file per session under `<tasks_dir>/sessions/<id>.json`. A
//! session is "open" when both `finished_at` and `abandoned_at` are `None`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    Task, TaskNote, TaskState, list_tasks, now_rfc3339, slugify, task_path, tasks_dir, unique_slug,
};

/// A task session: a named (or anonymous) span of work, tagged with the
/// project it was started in, whose tasks are tracked as a group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub name: Option<String>,
    /// The resolved project tag (see [`super::project::resolve_project_tag`])
    /// at the moment this session was started. Informational — used to
    /// filter/sort in `session ls`, `task add`'s auto-resolve, and `session
    /// start`'s checkpoint. Never used to partition storage.
    pub project: String,
    /// Free text set via `--description` (e.g. "dev-sprint issue 493").
    /// Display-only — never fed into slug/id generation, unlike `name`.
    #[serde(default)]
    pub description: Option<String>,
    /// RFC3339 timestamp.
    pub started_at: String,
    /// RFC3339 timestamp, updated whenever a task tagged to this session
    /// changes (add/start/done/note) or the session is resumed. Surfaced as
    /// an idle duration in `session ls` and the `session start` checkpoint.
    pub last_activity: String,
    /// RFC3339 timestamp; `None` while the session is open.
    #[serde(default)]
    pub finished_at: Option<String>,
    /// RFC3339 timestamp; set instead of `finished_at` when an existing
    /// session was abandoned via `session start --replace` rather than
    /// explicitly finished.
    #[serde(default)]
    pub abandoned_at: Option<String>,
}

impl Session {
    /// A session is open when it has been neither finished nor abandoned.
    /// The single source of truth for the predicate — callers outside this
    /// module (the CLI's `session ls` filter, `add_task`'s resolver) reuse
    /// it rather than re-inlining the two-field check, so adding a future
    /// close-state field updates every site at once.
    #[must_use]
    pub(crate) fn is_open(&self) -> bool {
        self.finished_at.is_none() && self.abandoned_at.is_none()
    }
}

/// How `session start` should resolve an existing same-project session.
#[derive(Debug, Clone)]
pub enum StartDecision {
    /// Create cleanly if none are open for this project; error (listing
    /// them) if one or more already are.
    Auto,
    /// Adopt the named existing session instead of creating a new one.
    Resume(String),
    /// Abandon every existing open session tagged to this project, then
    /// create a fresh one.
    Replace,
    /// Create a new session regardless of what's already open — the
    /// genuine-concurrency path.
    New,
}

/// What `start_session` actually did, so the CLI layer can report it.
#[derive(Debug, Clone)]
pub enum StartOutcome {
    Created(Session),
    Resumed(Session),
    Replaced {
        session: Session,
        abandoned: Vec<Session>,
    },
}

fn sessions_dir(state_dir: &Path) -> PathBuf {
    tasks_dir(state_dir).join("sessions")
}

fn session_path(state_dir: &Path, id: &str) -> PathBuf {
    sessions_dir(state_dir).join(format!("{id}.json"))
}

fn save_session(state_dir: &Path, session: &Session) -> anyhow::Result<()> {
    std::fs::create_dir_all(sessions_dir(state_dir))?;
    let json = serde_json::to_string_pretty(session)?;
    crate::paths::write_owner_only_atomic(&session_path(state_dir, &session.id), json.as_bytes())?;
    Ok(())
}

fn load_session(state_dir: &Path, id: &str) -> anyhow::Result<Session> {
    let content = std::fs::read_to_string(session_path(state_dir, id))?;
    Ok(serde_json::from_str(&content)?)
}

/// Every session in the store, corrupt/unreadable files skipped with a
/// stderr warning — same tolerance policy as [`super::list_tasks`], a single
/// bad file must never block `session ls` or a hook.
#[must_use]
pub fn list_sessions(state_dir: &Path) -> Vec<Session> {
    let dir = sessions_dir(state_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("llmenv: failed to read sessions dir {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|content| Ok(serde_json::from_str::<Session>(&content)?))
        {
            Ok(session) => sessions.push(session),
            Err(e) => eprintln!(
                "llmenv: skipping corrupt session file {}: {e}",
                path.display()
            ),
        }
    }
    sessions
}

/// Every currently open session tagged with `project`.
#[must_use]
pub fn open_sessions_for_project(state_dir: &Path, project: &str) -> Vec<Session> {
    list_sessions(state_dir)
        .into_iter()
        .filter(|s| s.is_open() && s.project == project)
        .collect()
}

/// Update a session's `last_activity` to now. No-op (returns `Ok`) if the
/// session doesn't exist or isn't open — a dangling `task.session` reference
/// (deleted session file) must never fail the task mutation that triggered
/// this touch. A present-but-corrupt session file is tolerated the same way,
/// but warned (matching [`list_sessions`]) rather than swallowed silently.
///
/// Runs the read-modify-write under the store lock: it's always called from
/// outside a held lock (after a task mutation's own lock has been released),
/// so a concurrent `session start --replace`/`finish_session` can't have this
/// resurrect a just-abandoned/finished session with a stale write.
///
/// # Errors
/// Propagates an I/O error only from the save of an existing, open session.
pub fn touch_last_activity(state_dir: &Path, session_id: &str) -> anyhow::Result<()> {
    super::with_store_lock(state_dir, || {
        let mut session = match load_session(state_dir, session_id) {
            Ok(session) => session,
            Err(e) if is_not_found(&e) => return Ok(()),
            Err(e) => {
                eprintln!(
                    "llmenv: could not load session '{session_id}' to update \
                     last_activity (skipping the touch): {e}"
                );
                return Ok(());
            }
        };
        if !session.is_open() {
            return Ok(());
        }
        session.last_activity = now_rfc3339();
        save_session(state_dir, &session)
    })
}

/// True when `err` wraps a `NotFound` I/O error — the "session file simply
/// doesn't exist" case, distinct from a corrupt/permission/other read error.
fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

/// Start, resume, replace, or create-alongside a session per `decision` —
/// the `session start` resume/replace/new checkpoint.
///
/// # Errors
/// `Auto`: errors listing every existing open same-project session (with
/// id/name/description/idle duration) when one or more already exist.
/// `Resume`: errors if the named session doesn't exist or isn't open.
pub fn start_session(
    state_dir: &Path,
    name: Option<&str>,
    description: Option<&str>,
    project: &str,
    decision: StartDecision,
) -> anyhow::Result<StartOutcome> {
    super::with_store_lock(state_dir, || match decision {
        StartDecision::Auto => {
            let existing = open_sessions_for_project(state_dir, project);
            if !existing.is_empty() {
                anyhow::bail!(checkpoint_error(&existing));
            }
            Ok(StartOutcome::Created(create_session(
                state_dir,
                name,
                description,
                project,
            )?))
        }
        StartDecision::Resume(id) => {
            let mut session = load_session(state_dir, &id)
                .map_err(|e| anyhow::anyhow!("no session '{id}' found: {e}"))?;
            if !session.is_open() {
                anyhow::bail!("session '{id}' is closed and cannot be resumed");
            }
            session.last_activity = now_rfc3339();
            save_session(state_dir, &session)?;
            Ok(StartOutcome::Resumed(session))
        }
        StartDecision::Replace => {
            let existing = open_sessions_for_project(state_dir, project);
            let mut abandoned = Vec::with_capacity(existing.len());
            for session in existing {
                abandoned.push(abandon_session(state_dir, session)?);
            }
            let session = create_session(state_dir, name, description, project)?;
            Ok(StartOutcome::Replaced { session, abandoned })
        }
        StartDecision::New => Ok(StartOutcome::Created(create_session(
            state_dir,
            name,
            description,
            project,
        )?)),
    })
}

fn create_session(
    state_dir: &Path,
    name: Option<&str>,
    description: Option<&str>,
    project: &str,
) -> anyhow::Result<Session> {
    let dir = sessions_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let base_slug = name
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());
    let id = unique_slug(&dir, &base_slug);
    let now = now_rfc3339();
    let session = Session {
        id,
        name: name.map(str::to_string),
        project: project.to_string(),
        description: description.map(str::to_string),
        started_at: now.clone(),
        last_activity: now,
        finished_at: None,
        abandoned_at: None,
    };
    save_session(state_dir, &session)?;
    Ok(session)
}

/// Human-readable idle duration since an RFC3339 `last_activity` timestamp
/// (e.g. `"2h 5m 3s"`), for `session ls` and the `session start` checkpoint.
/// `"unknown"` when the timestamp can't be parsed or is in the future.
#[must_use]
pub(crate) fn idle_display(last_activity: &str) -> String {
    let now = std::time::SystemTime::now();
    humantime::parse_rfc3339(last_activity)
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .map(|d| {
            humantime::format_duration(std::time::Duration::from_secs(d.as_secs())).to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build the `session start` checkpoint error message: lists every existing
/// open same-project session with enough detail (id, name, description,
/// idle duration) that the agent or a human can decide `--resume`,
/// `--replace`, or `--new` without needing to inspect anything further.
fn checkpoint_error(existing: &[Session]) -> String {
    let lines: Vec<String> = existing
        .iter()
        .map(|s| {
            let idle = idle_display(&s.last_activity);
            format!(
                "  - {} ({}){} — idle {idle}",
                s.id,
                s.name.as_deref().unwrap_or("unnamed"),
                s.description
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default(),
            )
        })
        .collect();
    format!(
        "session(s) already open for this project:\n{}\n\
         pass one of --resume <id>, --replace, or --new",
        lines.join("\n")
    )
}

/// Every task currently tagged with `session_id`.
fn tasks_in_session(state_dir: &Path, session_id: &str) -> Vec<Task> {
    list_tasks(state_dir)
        .into_iter()
        .filter(|t| t.session.as_deref() == Some(session_id))
        .collect()
}

/// Abandon `session`: stamps `abandoned_at`, and for every one of its tasks
/// that isn't already `done`, clears the `session` tag and appends an
/// orphaning note. Already-`done` tasks keep their tag — a legitimate
/// historical record. Caller must already hold the store lock. Returns the
/// stamped session, so the caller doesn't re-read what was just written.
fn abandon_session(state_dir: &Path, mut session: Session) -> anyhow::Result<Session> {
    let now = now_rfc3339();
    session.abandoned_at = Some(now.clone());
    save_session(state_dir, &session)?;

    let label = session.name.clone().unwrap_or_else(|| session.id.clone());
    for mut task in tasks_in_session(state_dir, &session.id)
        .into_iter()
        .filter(|t| t.state != TaskState::Done)
    {
        task.notes.push(TaskNote {
            at: now.clone(),
            text: format!(
                "Orphaned: session '{label}' was abandoned (`session start --replace`) \
                 before this task was finished."
            ),
        });
        task.session = None;
        task.updated_at = now.clone();
        super::save_task(state_dir, &task)?;
    }
    Ok(session)
}

/// Finish an open session by id: stamps `finished_at`.
///
/// # Errors
/// Errors if `id` doesn't resolve to an existing, currently-open session.
pub fn finish_session(state_dir: &Path, id: &str) -> anyhow::Result<Session> {
    super::with_store_lock(state_dir, || {
        let mut session = load_session(state_dir, id)
            .map_err(|e| anyhow::anyhow!("no session '{id}' found: {e}"))?;
        if !session.is_open() {
            anyhow::bail!("session '{id}' is already closed");
        }
        session.finished_at = Some(now_rfc3339());
        save_session(state_dir, &session)?;
        Ok(session)
    })
}

/// `(done, total)` counts for tasks tagged with `session_id`.
#[must_use]
pub fn session_progress(state_dir: &Path, session_id: &str) -> (u64, u64) {
    let tasks = tasks_in_session(state_dir, session_id);
    let done = tasks.iter().filter(|t| t.state == TaskState::Done).count() as u64;
    (done, tasks.len() as u64)
}

/// Delete every task tagged with `session_id` outright. Returns the deleted
/// tasks. Doesn't touch the session record itself.
pub fn delete_tasks_in_session(state_dir: &Path, session_id: &str) -> anyhow::Result<Vec<Task>> {
    super::with_store_lock(state_dir, || {
        let tasks = tasks_in_session(state_dir, session_id);
        for t in &tasks {
            std::fs::remove_file(task_path(state_dir, &t.slug))?;
        }
        Ok(tasks)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::task::{add_task_for_session, done_task, load_task, save_task};
    use proptest::prelude::*;
    use tempfile::TempDir;

    const PROJECT_A: &str = "project-a-0000000000";
    const PROJECT_B: &str = "project-b-0000000000";

    #[test]
    fn list_sessions_empty_store_is_empty() {
        let dir = TempDir::new().expect("test");
        assert!(list_sessions(dir.path()).is_empty());
    }

    #[test]
    fn start_session_auto_creates_when_none_open_for_project() {
        let dir = TempDir::new().expect("test");
        let outcome = start_session(
            dir.path(),
            Some("sprint 1"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test");
        let StartOutcome::Created(session) = outcome else {
            panic!("expected Created");
        };
        assert_eq!(session.project, PROJECT_A);
        assert_eq!(session.name.as_deref(), Some("sprint 1"));
        assert!(session.finished_at.is_none());
    }

    #[test]
    fn start_session_auto_errors_listing_existing_when_one_is_open() {
        let dir = TempDir::new().expect("test");
        start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test");
        let err = start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("first"),
            "error should list existing session: {err}"
        );
        assert!(err.contains("--resume"));
        assert!(err.contains("--replace"));
        assert!(err.contains("--new"));
    }

    #[test]
    fn start_session_auto_does_not_see_sessions_from_a_different_project() {
        let dir = TempDir::new().expect("test");
        start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test");
        let outcome = start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT_B,
            StartDecision::Auto,
        )
        .expect("test");
        assert!(matches!(outcome, StartOutcome::Created(_)));
    }

    #[test]
    fn start_session_resume_adopts_existing_session_without_new_id() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(first) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let outcome = start_session(
            dir.path(),
            None,
            None,
            PROJECT_A,
            StartDecision::Resume(first.id.clone()),
        )
        .expect("test");
        let StartOutcome::Resumed(resumed) = outcome else {
            panic!("expected Resumed");
        };
        assert_eq!(resumed.id, first.id);
    }

    #[test]
    fn start_session_resume_unknown_id_errors() {
        let dir = TempDir::new().expect("test");
        let err = start_session(
            dir.path(),
            None,
            None,
            PROJECT_A,
            StartDecision::Resume("no-such-session".to_string()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no-such-session"));
    }

    #[test]
    fn start_session_replace_abandons_existing_and_creates_fresh() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(first) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let outcome = start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT_A,
            StartDecision::Replace,
        )
        .expect("test");
        let StartOutcome::Replaced { session, abandoned } = outcome else {
            panic!("expected Replaced");
        };
        assert_ne!(session.id, first.id);
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].id, first.id);
        assert_eq!(open_sessions_for_project(dir.path(), PROJECT_A).len(), 1);
    }

    #[test]
    fn start_session_replace_untags_incomplete_tasks_but_preserves_done_tasks_tag() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(first) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let open_task =
            add_task_for_session(dir.path(), "Still open", None, &first.id).expect("test");
        let done_task_ =
            add_task_for_session(dir.path(), "Finished", None, &first.id).expect("test");
        done_task(dir.path(), &done_task_.slug).expect("test");

        start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT_A,
            StartDecision::Replace,
        )
        .expect("test");

        let reloaded_open = load_task(dir.path(), &open_task.slug).expect("test");
        assert!(reloaded_open.session.is_none());
        assert!(reloaded_open.notes[0].text.contains("Orphaned"));

        let reloaded_done = load_task(dir.path(), &done_task_.slug).expect("test");
        assert_eq!(reloaded_done.session, Some(first.id));
    }

    #[test]
    fn start_session_new_creates_alongside_existing_open_session() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(first) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let outcome = start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT_A,
            StartDecision::New,
        )
        .expect("test");
        let StartOutcome::Created(second) = outcome else {
            panic!("expected Created");
        };
        assert_ne!(first.id, second.id);
        let open = open_sessions_for_project(dir.path(), PROJECT_A);
        assert_eq!(open.len(), 2, "both sessions must remain open");
    }

    #[test]
    fn description_round_trips() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("first"),
            Some("dev-sprint issue 493"),
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        assert_eq!(session.description.as_deref(), Some("dev-sprint issue 493"));
        let reloaded = list_sessions(dir.path())
            .into_iter()
            .find(|s| s.id == session.id)
            .expect("test");
        assert_eq!(
            reloaded.description.as_deref(),
            Some("dev-sprint issue 493")
        );
    }

    #[test]
    fn last_activity_updates_on_touch() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let original = session.last_activity.clone();
        std::thread::sleep(std::time::Duration::from_secs(1));
        touch_last_activity(dir.path(), &session.id).expect("test");
        let reloaded = list_sessions(dir.path())
            .into_iter()
            .find(|s| s.id == session.id)
            .expect("test");
        assert_ne!(reloaded.last_activity, original);
    }

    #[test]
    fn touch_last_activity_on_missing_session_is_a_noop_ok() {
        let dir = TempDir::new().expect("test");
        // No such session file — a dangling task.session reference must not
        // fail the touch.
        touch_last_activity(dir.path(), "no-such-session").expect("test");
    }

    #[test]
    fn touch_last_activity_on_corrupt_session_file_is_a_noop_ok() {
        let dir = TempDir::new().expect("test");
        std::fs::create_dir_all(sessions_dir(dir.path())).expect("test");
        std::fs::write(session_path(dir.path(), "corrupt"), b"not valid json").expect("test");
        // Tolerated (warned, not propagated) — a corrupt session file must
        // not turn an already-committed task mutation into an error.
        touch_last_activity(dir.path(), "corrupt").expect("test");
    }

    #[test]
    fn touch_last_activity_on_finished_session_does_not_reopen_it() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("s"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
            panic!("expected Created");
        };
        finish_session(dir.path(), &session.id).expect("test");
        touch_last_activity(dir.path(), &session.id).expect("test");
        // Still closed — the touch is a no-op on a non-open session.
        assert!(open_sessions_for_project(dir.path(), PROJECT_A).is_empty());
    }

    #[test]
    fn last_activity_updates_on_resume() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let original = session.last_activity.clone();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let StartOutcome::Resumed(resumed) = start_session(
            dir.path(),
            None,
            None,
            PROJECT_A,
            StartDecision::Resume(session.id.clone()),
        )
        .expect("test") else {
            panic!("expected Resumed");
        };
        assert_ne!(resumed.last_activity, original);
    }

    #[test]
    fn finish_session_by_id_stamps_finished_at() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let finished = finish_session(dir.path(), &session.id).expect("test");
        assert!(finished.finished_at.is_some());
        assert!(open_sessions_for_project(dir.path(), PROJECT_A).is_empty());
    }

    #[test]
    fn finish_session_unknown_id_errors() {
        let dir = TempDir::new().expect("test");
        assert!(finish_session(dir.path(), "no-such-session").is_err());
    }

    #[test]
    fn finish_session_already_closed_errors() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        finish_session(dir.path(), &session.id).expect("test");
        assert!(finish_session(dir.path(), &session.id).is_err());
    }

    #[test]
    fn open_sessions_for_project_excludes_finished_and_other_projects() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(a) =
            start_session(dir.path(), Some("a"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
            panic!("expected Created");
        };
        start_session(dir.path(), Some("b"), None, PROJECT_B, StartDecision::Auto).expect("test");
        finish_session(dir.path(), &a.id).expect("test");
        start_session(dir.path(), Some("c"), None, PROJECT_A, StartDecision::Auto).expect("test");

        let open = open_sessions_for_project(dir.path(), PROJECT_A);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].name.as_deref(), Some("c"));
    }

    #[test]
    fn session_progress_counts_only_tasks_in_that_session() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("sprint 1"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let t1 = add_task_for_session(dir.path(), "Task one", None, &session.id).expect("test");
        add_task_for_session(dir.path(), "Task two", None, &session.id).expect("test");
        done_task(dir.path(), &t1.slug).expect("test");
        assert_eq!(session_progress(dir.path(), &session.id), (1, 2));
    }

    #[test]
    fn delete_tasks_in_session_removes_only_that_sessions_tasks() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) = start_session(
            dir.path(),
            Some("sprint 1"),
            None,
            PROJECT_A,
            StartDecision::Auto,
        )
        .expect("test") else {
            panic!("expected Created");
        };
        let in_session =
            add_task_for_session(dir.path(), "In the session", None, &session.id).expect("test");
        let deleted = delete_tasks_in_session(dir.path(), &session.id).expect("test");
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].slug, in_session.slug);
        assert!(load_task(dir.path(), &in_session.slug).is_err());
    }

    proptest::proptest! {
        /// `(done, total)` invariants: unaffected by the schema fields added
        /// in this task (`project`/`description`/`last_activity`) — same
        /// invariant `session.rs` already carried before this rewrite.
        #[test]
        fn session_progress_invariants_hold_for_arbitrary_task_mix(
            states in proptest::collection::vec(
                (proptest::bool::ANY, proptest::bool::ANY, proptest::bool::ANY),
                0..12,
            ),
        ) {
            let dir = TempDir::new().expect("test");
            let session_id = "session-under-test";
            let mut expected_total = 0u64;
            let mut expected_done = 0u64;
            for (i, (tagged, other_session, done)) in states.iter().enumerate() {
                let session = if *tagged {
                    expected_total += 1;
                    if *done {
                        expected_done += 1;
                    }
                    Some(session_id.to_string())
                } else if *other_session {
                    Some("some-other-session".to_string())
                } else {
                    None
                };
                let task = Task {
                    slug: format!("task-{i}"),
                    title: format!("Task {i}"),
                    state: if *done { TaskState::Done } else { TaskState::Open },
                    parent: None,
                    blocked_on: Vec::new(),
                    notes: Vec::new(),
                    session,
                    created_at: now_rfc3339(),
                    updated_at: now_rfc3339(),
                };
                save_task(dir.path(), &task).expect("test");
            }
            let (done, total) = session_progress(dir.path(), session_id);
            prop_assert!(done <= total);
            prop_assert_eq!(total, expected_total);
            prop_assert_eq!(done, expected_done);
        }
    }
}
