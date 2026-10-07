//! Resolve a typed task identifier to the slug of an existing task.
//!
//! Slugs are unique per project, so a done task from a finished session keeps its slug.
//! A later session that adds the same title gets a `-2` suffix. A bare slug typed in that
//! later session must reach the later session's task first (#2501).

use std::path::Path;

use anyhow::Context as _;

use super::session::{self, EngineIdentity};
use super::{task_path, try_list_tasks};

/// Resolve a user-supplied identifier (exact slug or unambiguous prefix) to the exact slug of
/// an existing task. The caller's open session is searched first, so a bare slug does not
/// land on a task that a finished session left behind. That search also matches by prefix, so a
/// bare `foo` reaches the open session's `foo-2` even when a finished session holds `foo`.
///
/// # Errors
/// Returns an error if `input` isn't a safe single path component (rejects
/// path traversal / absolute-path attempts before any path is constructed —
/// a task slug is always a single component), if no task matches, if the
/// prefix matches more than one task (the error lists every candidate slug), or if the session
/// or task store cannot be read.
pub(crate) fn resolve_identifier(state_dir: &Path, input: &str) -> anyhow::Result<String> {
    resolve_identifier_for(state_dir, input, &EngineIdentity::from_env())
}

/// [`resolve_identifier`] for an explicit caller identity.
///
/// # Errors
/// The same errors as [`resolve_identifier`], plus an unreadable session or task store.
fn resolve_identifier_for(
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
    let preferred = preferred_sessions(state_dir, owner)
        .with_context(|| format!("listing sessions to resolve task '{input}'"))?;
    if !preferred.is_empty() {
        let mine: Vec<&str> = tasks
            .iter()
            .filter(|t| t.session.as_ref().is_some_and(|id| preferred.contains(id)))
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

/// The ids of the sessions a bare slug is searched in first: the one open session that `owner`
/// means, else every open session when the choice is unclear. A task of a finished session never
/// outranks a task of an open one.
fn preferred_sessions(state_dir: &Path, owner: &EngineIdentity) -> anyhow::Result<Vec<String>> {
    let open: Vec<_> = session::try_list_sessions(state_dir)?
        .into_iter()
        .filter(session::Session::is_open)
        .collect();
    let ids = open.iter().map(|s| s.id.clone()).collect();
    Ok(match session::pick_open_session(open, owner) {
        Ok(picked) => vec![picked.id],
        Err(_) => ids,
    })
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

    #[test]
    fn with_several_open_sessions_and_no_owner_an_open_session_task_still_wins() {
        let dir = TempDir::new().unwrap();
        let old = open_session(dir.path(), StartDecision::Auto);
        let finished = add(dir.path(), "Shared", &old);
        let first = open_session(dir.path(), StartDecision::Replace);
        let second = open_session(dir.path(), StartDecision::New);
        assert_ne!(first, second);
        let live = add(dir.path(), "Shared", &second);
        assert_eq!((finished.as_str(), live.as_str()), ("shared", "shared-2"));
        assert_eq!(
            resolve_identifier_for(dir.path(), "shared", &NOBODY).unwrap(),
            "shared-2"
        );
    }

    #[test]
    fn pick_prefers_an_exact_match_and_rejects_an_ambiguous_prefix() {
        assert_eq!(pick(&["ab", "abc"], "ab").unwrap().as_deref(), Some("ab"));
        assert_eq!(
            pick(&["abc", "abd"], "abc").unwrap().as_deref(),
            Some("abc")
        );
        assert!(pick(&["abc", "abd"], "ab").is_err());
        assert_eq!(pick(&["abc"], "x").unwrap(), None);
    }

    proptest::proptest! {
        // Each case writes session and task files, so the case count stays low.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        // Whatever the titles, a bare slug never resolves to a task of the finished session
        // while the open session has tasks, and each open-session slug resolves to itself.
        #[test]
        fn open_session_tasks_win_over_finished_session_tasks(
            titles in proptest::collection::btree_set("[a-z]{1,6}", 1..6),
        ) {
            let dir = TempDir::new().unwrap();
            let old = open_session(dir.path(), StartDecision::Auto);
            for t in &titles {
                add(dir.path(), t, &old);
            }
            let new = open_session(dir.path(), StartDecision::Replace);
            let mine: Vec<String> = titles.iter().map(|t| add(dir.path(), t, &new)).collect();
            for t in &titles {
                let bare = crate::task::slugify(t);
                if let Ok(slug) = resolve_identifier_for(dir.path(), &bare, &NOBODY) {
                    proptest::prop_assert!(mine.contains(&slug), "{bare} -> {slug}");
                }
            }
            for slug in &mine {
                proptest::prop_assert_eq!(
                    &resolve_identifier_for(dir.path(), slug, &NOBODY).unwrap(),
                    slug
                );
            }
        }
        // `pick` gives the same answer for any order of the slugs.
        #[test]
        fn pick_ignores_the_order_of_the_slugs(
            slugs in proptest::collection::btree_set("[a-c]{1,4}", 0..8),
            input in "[a-c]{1,3}",
        ) {
            let forward: Vec<&str> = slugs.iter().map(String::as_str).collect();
            let mut backward = forward.clone();
            backward.reverse();
            let a = pick(&forward, &input).map_err(|e| e.to_string());
            let b = pick(&backward, &input).map_err(|e| e.to_string());
            proptest::prop_assert_eq!(a, b);
            if slugs.contains(&input) {
                proptest::prop_assert_eq!(pick(&forward, &input).unwrap(), Some(input));
            }
        }
    }
}
