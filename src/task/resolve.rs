//! Resolve a typed task identifier to the slug of an existing task.
//!
//! Slugs are unique per project, so a done task from a finished session keeps its slug.
//! A later session that adds the same title gets a `-2` suffix. A bare slug typed in that
//! later session must reach the later session's task first (#2501).

use std::path::Path;

use super::session::{self, EngineIdentity};
use super::{task_path, try_list_tasks};

/// Resolve a user-supplied identifier (exact slug or unambiguous prefix) to the exact slug of
/// an existing task. The caller's open session is searched first, so a bare slug does not
/// land on a task that a finished session left behind.
///
/// # Errors
/// Returns an error if `input` isn't a safe single path component (rejects
/// path traversal / absolute-path attempts before any path is constructed —
/// a task slug is always a single component), if no task matches, or if the
/// prefix matches more than one task (the error lists every candidate slug).
pub(crate) fn resolve_identifier(state_dir: &Path, input: &str) -> anyhow::Result<String> {
    resolve_identifier_for(state_dir, input, &EngineIdentity::from_env())
}

/// [`resolve_identifier`] for an explicit caller identity.
///
/// # Errors
/// The same errors as [`resolve_identifier`], plus an unreadable session or task store.
pub(crate) fn resolve_identifier_for(
    state_dir: &Path,
    input: &str,
    owner: &EngineIdentity,
) -> anyhow::Result<String> {
    if !crate::paths::is_valid_short_name(input) {
        anyhow::bail!("'{input}' is not a valid task identifier");
    }
    // Fallible `try_list_tasks` rather than the tolerant `list_tasks`: an unreadable store
    // must error out here rather than be misread as "no task found" (#1112) — this is the
    // resolution step every mutating task command (and `TaskUpdate`'s hook redirect) runs
    // through.
    let tasks = try_list_tasks(state_dir)?;
    if let Some(session_id) = caller_session(state_dir, owner)? {
        let mine: Vec<&str> = tasks
            .iter()
            .filter(|t| t.session.as_deref() == Some(session_id.as_str()))
            .map(|t| t.slug.as_str())
            .collect();
        if let Some(slug) = pick(&mine, input)? {
            return Ok(slug);
        }
    }
    if task_path(state_dir, input).exists() {
        return Ok(input.to_string());
    }
    let all: Vec<&str> = tasks.iter().map(|t| t.slug.as_str()).collect();
    pick(&all, input)?.ok_or_else(|| anyhow::anyhow!("no task found matching '{input}'"))
}

/// The open session that `owner` means, or `None` when no single one is meant. An unclear
/// choice is not an error here: the project-wide search below still answers.
fn caller_session(state_dir: &Path, owner: &EngineIdentity) -> anyhow::Result<Option<String>> {
    let open = session::try_list_sessions(state_dir)?
        .into_iter()
        .filter(session::Session::is_open)
        .collect();
    Ok(session::pick_open_session(open, owner).ok().map(|s| s.id))
}

/// An exact match wins, else the one slug that starts with `input`.
fn pick(slugs: &[&str], input: &str) -> anyhow::Result<Option<String>> {
    if slugs.contains(&input) {
        return Ok(Some(input.to_string()));
    }
    let mut matches: Vec<&str> = slugs
        .iter()
        .copied()
        .filter(|s| s.starts_with(input))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0].to_string())),
        _ => {
            matches.sort_unstable();
            anyhow::bail!("'{input}' matches multiple tasks: {}", matches.join(", "))
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use crate::task::session::{StartDecision, StartOutcome, start_session};
    use crate::task::{ParentSpec, add_task_for_session};
    use tempfile::TempDir;

    const PROJECT: &str = "test-project-0000000000";
    const NOBODY: EngineIdentity = EngineIdentity {
        session_id: None,
        pid: None,
    };

    fn open_session(dir: &Path, decision: StartDecision) -> String {
        match start_session(dir, None, None, PROJECT, decision).unwrap() {
            StartOutcome::Created(s)
            | StartOutcome::Resumed(s)
            | StartOutcome::Replaced { session: s, .. } => s.id,
        }
    }

    fn add(dir: &Path, title: &str, session_id: &str) -> String {
        add_task_for_session(dir, title, ParentSpec::Detached, session_id)
            .unwrap()
            .slug
    }

    /// An old finished session and a new open one, each with tasks titled "Plan task N".
    /// Returns the new session's id and slugs for N = 1..=3. The new slugs carry a `-2`.
    fn old_and_new(dir: &Path) -> (String, Vec<String>) {
        let old = open_session(dir, StartDecision::Auto);
        for n in 1..=3 {
            add(dir, &format!("Plan task {n}"), &old);
        }
        let new = open_session(dir, StartDecision::Replace);
        let slugs = (1..=3)
            .map(|n| add(dir, &format!("Plan task {n}"), &new))
            .collect();
        (new, slugs)
    }

    #[test]
    fn bare_slug_reaches_the_open_session_task_not_the_finished_one() {
        let dir = TempDir::new().unwrap();
        let (_, new) = old_and_new(dir.path());
        assert_eq!(new, ["plan-task-1-2", "plan-task-2-2", "plan-task-3-2"]);
        assert_eq!(
            resolve_identifier_for(dir.path(), "plan-task-3", &NOBODY).unwrap(),
            "plan-task-3-2"
        );
    }

    #[test]
    fn exact_slug_of_the_open_session_wins() {
        let dir = TempDir::new().unwrap();
        let (_, new) = old_and_new(dir.path());
        assert_eq!(
            resolve_identifier_for(dir.path(), &new[0], &NOBODY).unwrap(),
            new[0]
        );
    }

    #[test]
    fn a_prefix_that_hits_several_open_session_tasks_is_ambiguous() {
        let dir = TempDir::new().unwrap();
        old_and_new(dir.path());
        let err = resolve_identifier_for(dir.path(), "plan-task", &NOBODY)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("plan-task-1-2") && err.contains("plan-task-3-2"),
            "{err}"
        );
        assert!(
            !err.contains("plan-task-1,"),
            "old session slug leaked: {err}"
        );
    }

    #[test]
    fn a_slug_only_a_finished_session_has_still_resolves() {
        let dir = TempDir::new().unwrap();
        let old = open_session(dir.path(), StartDecision::Auto);
        let slug = add(dir.path(), "Old only", &old);
        open_session(dir.path(), StartDecision::Replace);
        assert_eq!(
            resolve_identifier_for(dir.path(), &slug, &NOBODY).unwrap(),
            slug
        );
    }

    #[test]
    fn with_no_open_session_resolution_is_project_wide() {
        let dir = TempDir::new().unwrap();
        let slug = add(dir.path(), "Fix login timeout", "gone");
        assert_eq!(
            resolve_identifier_for(dir.path(), "fix-log", &NOBODY).unwrap(),
            slug
        );
    }

    #[test]
    fn unknown_identifier_is_an_error() {
        let dir = TempDir::new().unwrap();
        old_and_new(dir.path());
        let err = resolve_identifier_for(dir.path(), "nope", &NOBODY).unwrap_err();
        assert!(err.to_string().contains("no task found"), "{err}");
    }
}
