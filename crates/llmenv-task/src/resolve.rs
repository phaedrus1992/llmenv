//! Resolve a typed task identifier to the slug of an existing task.
//!
//! Slugs are unique per project, so a done task from a finished session keeps its slug.
//! A later session that adds the same title gets a `-2` suffix. A bare slug typed in that
//! later session must reach the later session's task first (#2501).

use std::path::Path;

use anyhow::Context as _;

use super::session::{self, EngineIdentity};
use super::{list_tasks_strict, task_path};

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
pub fn resolve_identifier(state_dir: &Path, input: &str) -> anyhow::Result<String> {
    resolve_identifier_for(state_dir, input, &EngineIdentity::from_env()?)
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
    check_identifier(input)?;
    // A strict read: an unreadable store or task file must error out here rather than be
    // misread as "no task found" (#1112, #3471). This is the resolution step every mutating
    // task command (and `TaskUpdate`'s hook redirect) runs through.
    resolve_with(
        state_dir,
        input,
        owner,
        list_tasks_strict(state_dir)?.as_slice(),
    )
}

/// The task that a command acts on: a name to look up, or a slug that a lookup in the same
/// command already returned, such as the one the ownership check resolved. A resolved slug is
/// not looked up again, so the check and the command act on the same task, and the store is
/// read once (#3477).
#[derive(Debug, Clone, Copy)]
pub enum TaskTarget<'a> {
    /// A name from the user or the agent: a slug or a unique slug prefix.
    Input(&'a str),
    /// A slug that a lookup in this command returned.
    Resolved(&'a str),
}

impl<'a> From<&'a str> for TaskTarget<'a> {
    fn from(input: &'a str) -> Self {
        Self::Input(input)
    }
}

impl<'a> From<&'a String> for TaskTarget<'a> {
    fn from(input: &'a String) -> Self {
        Self::Input(input)
    }
}

impl TaskTarget<'_> {
    /// Refuse an input that cannot be a slug, before any read of the store.
    ///
    /// # Errors
    /// The input is not a valid task identifier.
    pub(crate) fn check(self) -> anyhow::Result<()> {
        match self {
            Self::Input(name) | Self::Resolved(name) => check_identifier(name),
        }
    }

    /// The slug, with a strict read of the store for an input.
    ///
    /// # Errors
    /// The errors of [`resolve_identifier`].
    pub(crate) fn slug(self, state_dir: &Path) -> anyhow::Result<String> {
        match self {
            Self::Input(input) => resolve_identifier(state_dir, input),
            // A slug comes from a task file, which a person can edit, so it is checked before it
            // becomes a path.
            Self::Resolved(slug) => check_identifier(slug).map(|()| slug.to_string()),
        }
    }

    /// The slug, from a strict read of the store that the caller already made.
    ///
    /// # Errors
    /// The errors of [`resolve_in`].
    pub(crate) fn slug_in(
        self,
        state_dir: &Path,
        tasks: &super::StrictTasks,
    ) -> anyhow::Result<String> {
        match self {
            Self::Input(input) => {
                check_identifier(input)?;
                resolve_in(state_dir, input, tasks)
            }
            Self::Resolved(slug) => check_identifier(slug).map(|()| slug.to_string()),
        }
    }
}

/// Refuse a name that cannot be a task slug, before any read of the store, so a bad name gets
/// its own error even when the store is unreadable.
///
/// # Errors
/// `input` is not a valid task identifier.
pub(super) fn check_identifier(input: &str) -> anyhow::Result<()> {
    if !llmenv_paths::is_valid_short_name(input) {
        anyhow::bail!("'{input}' is not a valid task identifier");
    }
    Ok(())
}

/// [`resolve_identifier`] over a strict read of the store that the caller already made, so one
/// task command reads the store once (#3474). The caller runs [`check_identifier`] first.
///
/// # Errors
/// The session files cannot be read, or no task, or more than one, matches `input`.
pub(super) fn resolve_in(
    state_dir: &Path,
    input: &str,
    tasks: &super::StrictTasks,
) -> anyhow::Result<String> {
    resolve_with(
        state_dir,
        input,
        &EngineIdentity::from_env()?,
        tasks.as_slice(),
    )
}

fn resolve_with(
    state_dir: &Path,
    input: &str,
    owner: &EngineIdentity,
    tasks: &[super::Task],
) -> anyhow::Result<String> {
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
    let open: Vec<_> = session::list_sessions_strict(state_dir)?
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
mod tests;
