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

mod branch_failure;
mod slug;
pub(crate) use slug::slugify;
mod add;
use add::add_task_for_session_with;
mod ancestors;
mod block_cycle;
mod blockers;
pub use blockers::block_task;
use blockers::first_undone_in_subtree;
pub mod core_text;
mod finish_hints;
mod parent_chain;
use ancestors::ancestors_to_start;
use parent_chain::reject_cycle;
mod edit;
pub use edit::TaskEdit;
pub use edit::edit_task;
mod note;
pub use note::note_task;
pub mod note_not_saved;
pub mod ownership;
pub mod partial;
pub use note_not_saved::exit_code;
pub mod pending_note;
pub use pending_note::recover as recover_crashed_notes;
pub mod progress;
mod session_choice;
mod startability;
pub use resolve::TaskTarget;
mod store_lock;
pub use store_lock::with_store_lock;
pub mod project;
pub mod relation;
mod reminder_store;
pub use reminder_store::Stored;
use reminder_store::{store_for_reminder, store_warning_text};
pub mod reopen;
mod resolve;
pub mod resume;
pub mod session;
pub mod visibility;
pub use finish_hints::last_task_hint;
use finish_hints::session_finish_reminders;
mod recency;
mod tracking;
pub use progress::{StrictTasks, progress_after_close, progress_in, wip_title_in};
use recency::most_recently_updated;
pub use reopen::reopen_task;
pub use resolve::resolve_identifier;
pub use tracking::{Tracking, tracking, tracking_in};

pub use relation::Placement;
use relation::Relation;
use session::Session;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a tracked task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
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

impl TaskState {
    /// Lowercase label (`open`/`wip`/`waiting`/`done`), the canonical
    /// rendering shared by `cli::style::task_state_label` and any
    /// user-facing message composed here in `task/mod.rs`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Wip => "wip",
            Self::Waiting => "waiting",
            Self::Done => "done",
        }
    }
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
    parent: Option<String>,
    /// Whether the task is a sub-task of `parent` or a top-level task of the queue (#2455).
    #[serde(default, skip_serializing_if = "Relation::is_queued")]
    relation: Relation,
    /// A queued task that runs beside the head of the queue instead of behind it (#2455).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    parallel: bool,
    #[serde(default)]
    pub blocked_on: Vec<String>,
    #[serde(default)]
    notes: Vec<TaskNote>,
    /// What a cold reader needs to do the task: files, acceptance criteria, gotchas (#2339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
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
    updated_at: String,
}

/// The task-store subdirectory under llmenv's state dir.
pub fn tasks_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("tasks")
}

fn task_path(state_dir: &Path, slug: &str) -> PathBuf {
    tasks_dir(state_dir).join(format!("{slug}.json"))
}

/// Current RFC3339 timestamp (UTC).
// The queue (#2455) orders tasks by comparing `created_at` text, so the text must have a fixed
// width. `now_rfc3339_nanos` always writes 9 fractional digits.
use llmenv_util::time::now_rfc3339_nanos as now_rfc3339;

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
///
/// `task.slug` is validated the same way [`resolve_identifier`]'s input is —
/// this crate's `slug` field is `pub` (needed by callers that display or
/// compare it), so a caller could otherwise mutate it to a path-traversal
/// payload (`../../etc/passwd`) and reach outside [`tasks_dir`] via
/// [`task_path`]'s naive `join`.
///
/// # Errors
/// Returns an error when `task.slug` fails validation, or the write fails.
pub fn save_task(state_dir: &Path, task: &Task) -> anyhow::Result<()> {
    if !llmenv_paths::is_valid_short_name(&task.slug) {
        anyhow::bail!("'{}' is not a valid task slug", task.slug);
    }
    let dir = tasks_dir(state_dir);
    llmenv_paths::create_dir_owner_only(&dir)?;
    let json = serde_json::to_string_pretty(task)?;
    llmenv_paths::write_owner_only_atomic(&task_path(state_dir, &task.slug), json.as_bytes())?;
    StrictTasks::record_write();
    Ok(())
}

/// Load a single task by its exact slug.
///
/// `slug` is validated the same way [`resolve_identifier`]'s input is — see
/// [`save_task`]'s doc comment for why this crate can't rely on every caller
/// pre-validating it.
///
/// # Errors
/// Returns an error when `slug` fails validation, the file can't be read, or
/// its contents aren't a valid `Task`.
pub fn load_task(state_dir: &Path, slug: &str) -> anyhow::Result<Task> {
    use anyhow::Context as _;
    if !llmenv_paths::is_valid_short_name(slug) {
        anyhow::bail!("'{slug}' is not a valid task slug");
    }
    let path = task_path(state_dir, slug);
    let content = session::read_state_text(&path)
        .with_context(|| format!("cannot read task '{slug}' at {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("task '{slug}' at {} is not valid task JSON", path.display()))
}

/// List all tasks in the store, tolerating a missing or unreadable store by
/// treating it as empty (logging the cause via `tracing::warn!`). Callers
/// that must distinguish "genuinely empty" from "couldn't read the store"
/// (e.g. denying a `TaskList` redirect with the real error, #1112) should use
/// [`try_list_tasks`] instead.
pub fn list_tasks(state_dir: &Path) -> Vec<Task> {
    match try_list_tasks(state_dir) {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::error!(error = %e, "failed to read tasks dir; treating as empty");
            Vec::new()
        }
    }
}

/// Fallible sibling of [`list_tasks`]: propagates a genuine read error on the
/// tasks directory itself (e.g. permission denied) instead of collapsing it
/// to an empty `Vec` indistinguishable from "no tasks tracked yet" (#1112).
/// A missing directory still resolves to `Ok(vec![])` — that case really is
/// "no tasks yet". Per-entry `DirEntry` errors and corrupt task files are
/// logged via `tracing::warn!` and skipped, never silently dropped.
///
/// # Errors
/// Returns an error if the tasks directory exists but can't be read (e.g.
/// permission denied).
pub fn try_list_tasks(state_dir: &Path) -> anyhow::Result<Vec<Task>> {
    read_tasks(state_dir, UnreadablePolicy::Skip)
}

/// [`try_list_tasks`], but a task file that cannot be read is an error and not a skipped entry.
/// A decision or a reminder that reads the store uses this, so it never sees a partial view
/// (#2608).
pub fn list_tasks_strict(state_dir: &Path) -> anyhow::Result<StrictTasks> {
    StrictTasks::read(state_dir)
}

/// What [`read_tasks`] does with a task file or directory entry that cannot be read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnreadablePolicy {
    /// Log the file and leave it out of the listing.
    Skip,
    /// Fail the whole listing.
    Fail,
}

fn read_tasks(state_dir: &Path, policy: UnreadablePolicy) -> anyhow::Result<Vec<Task>> {
    StrictTasks::count_read();
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
                tracing::warn!(error = %e, dir = %dir.display(), "skipping unreadable directory entry");
                llmenv_util::agent_notice::record(format!(
                    "llmenv skipped an unreadable entry in {}: {e}. Fix: check the directory's permissions.",
                    dir.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match session::read_state_text(&path)
            .map_err(anyhow::Error::from)
            .and_then(|content| Ok(serde_json::from_str::<Task>(&content)?))
        {
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
                let refused = e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::InvalidData);
                let fix = if refused {
                    "remove the file, or replace it with a task file that llmenv wrote"
                } else {
                    "check the file's permissions"
                };
                llmenv_util::agent_notice::record(format!(
                    "llmenv skipped the unreadable task file {}: {e}. The task is missing from \
                     task lists and resume. Fix: {fix}.",
                    path.display()
                ));
            }
            Err(e) if policy == UnreadablePolicy::Fail => {
                return Err(e.context(format!("parsing task file {}", path.display())));
            }
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "skipping corrupt task file");
                llmenv_util::agent_notice::record(format!(
                    "llmenv skipped the corrupt task file {}: {e}. The task is missing from task \
                     lists and resume. Fix: repair or delete the file.",
                    path.display()
                ));
            }
        }
    }
    Ok(tasks)
}

/// A task paired with its indentation depth for human `task ls` rendering: a
/// parent is depth 0, its subtasks depth 1, and so on (#926).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRow {
    pub depth: usize,
    pub task: Task,
}

/// Keep only tasks whose state is one of `states`. An empty `states` keeps
/// everything (the "no `--state` filter" case), so callers pass either the
/// states the user asked for or an empty slice.
#[must_use]
pub fn filter_by_state(tasks: Vec<Task>, states: &[TaskState]) -> Vec<Task> {
    if states.is_empty() {
        return tasks;
    }
    tasks
        .into_iter()
        .filter(|t| states.contains(&t.state))
        .collect()
}

/// Sort key for a task's session group in human `task ls` output: prioritized
/// sessions first (in `priority` order — the caller puts current-project open
/// sessions there), then any other session id alphabetically, then the
/// no-session bucket last.
fn session_rank(key: &Option<String>, priority: &[String]) -> (u8, usize, String) {
    match key {
        Some(id) => match priority.iter().position(|p| p == id) {
            Some(i) => (0, i, String::new()),
            None => (1, 0, id.clone()),
        },
        None => (2, 0, String::new()),
    }
}

/// Order tasks for human display (#926): grouped by session (prioritized
/// sessions first per `session_priority`, then other sessions alphabetically,
/// then no-session tasks), and within each group parent-before-children — a
/// parent immediately followed by its subtree (recursively), roots and
/// siblings in creation order. Nothing is dropped regardless of
/// `session_priority`: any session present but unlisted still gets a group.
#[must_use]
pub fn display_rows(tasks: Vec<Task>, session_priority: &[String]) -> Vec<DisplayRow> {
    let mut keys: Vec<Option<String>> = tasks
        .iter()
        .map(|t| t.session.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    // Cache the key (it allocates for the "other sessions" bucket) so it's
    // computed once per element, not on every comparison.
    keys.sort_by_cached_key(|k| session_rank(k, session_priority));

    let mut rows = Vec::new();
    for key in &keys {
        let group: Vec<&Task> = tasks.iter().filter(|t| &t.session == key).collect();
        append_forest(&group, &mut rows);
    }
    rows
}

/// Append one session group's tasks to `rows` as a parent→child forest.
/// Tasks whose parent is absent from this group are treated as roots; a
/// `visited` guard makes malformed parent cycles terminate instead of
/// recursing forever.
fn append_forest(group: &[&Task], rows: &mut Vec<DisplayRow>) {
    let present: HashSet<&str> = group.iter().map(|t| t.slug.as_str()).collect();
    let mut children: HashMap<&str, Vec<&Task>> = HashMap::new();
    let mut roots: Vec<&Task> = Vec::new();
    for &t in group {
        match &t.parent {
            Some(p) if present.contains(p.as_str()) => {
                children.entry(p.as_str()).or_default().push(t)
            }
            _ => roots.push(t),
        }
    }
    // created_at ties are rare but possible (legacy second-precision data,
    // or a genuine same-instant race); fall back to slug for a deterministic,
    // stable order (readdir order from `list_tasks` is otherwise arbitrary).
    let order = |a: &&Task, b: &&Task| queue_order(a, b);
    roots.sort_by(order);
    for kids in children.values_mut() {
        kids.sort_by(order);
    }
    let mut visited: HashSet<&str> = HashSet::new();
    for root in roots {
        visit(root, 0, &children, &mut visited, rows);
    }
    // Completeness guard: any task not reached from a root (e.g. a malformed
    // parent cycle where every node points at another present task) is
    // rendered as a depth-0 row rather than silently dropped from the listing.
    let mut orphans: Vec<&Task> = group
        .iter()
        .copied()
        .filter(|t| !visited.contains(t.slug.as_str()))
        .collect();
    orphans.sort_by(order);
    for orphan in orphans {
        visit(orphan, 0, &children, &mut visited, rows);
    }
}

/// Push the rows of `task` and of everything below it in pre-order. An explicit stack replaces
/// the recursion, so a deep chain of sub-tasks cannot overflow the call stack (#3767).
fn visit<'a>(
    task: &'a Task,
    depth: usize,
    children: &std::collections::HashMap<&'a str, Vec<&'a Task>>,
    visited: &mut HashSet<&'a str>,
    rows: &mut Vec<DisplayRow>,
) {
    let mut stack: Vec<(&'a Task, usize)> = vec![(task, depth)];
    while let Some((current, depth)) = stack.pop() {
        if !visited.insert(current.slug.as_str()) {
            continue;
        }
        rows.push(DisplayRow {
            depth,
            task: current.clone(),
        });
        if let Some(kids) = children.get(current.slug.as_str()) {
            // Reversed, so the stack pops the first child first.
            stack.extend(kids.iter().rev().map(|child| (*child, depth + 1)));
        }
    }
}

/// How `add_task`/`add_task_for_session` should set the new task's `parent`, a display link
/// for a top-level task. A sub-task takes its parent from [`Placement::Child`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentSpec<'a> {
    /// `--parent <id>` given explicitly.
    Explicit(&'a str),
    /// No parent. A new task no longer chains onto the previous one (#2455): the queue orders
    /// top-level tasks.
    Detached,
}

/// Which session a new task joins.
#[derive(Debug, Clone, Copy)]
pub enum SessionChoice<'a> {
    /// `--session <id>` given explicitly. The writer refuses a session of another conversation
    /// unless `other_session` names its owner (#3486, #3617).
    Named {
        id: &'a str,
        owner: &'a session::EngineIdentity,
        other_session: Option<&'a str>,
    },
    /// No `--session`: the project's only open session, else the one the
    /// caller owns ([`session::pick_open_session`], #2365).
    Resolve(&'a session::EngineIdentity),
    /// An open session of the project that the caller read in the same command, under the same
    /// store lock as the write, so the session files are not read again (#3474). A `Session`
    /// comes only from a read or a start, so a caller cannot name an arbitrary id here. The
    /// writer refuses it when it is closed, of another project, or of another conversation.
    Checked {
        session: &'a session::Session,
        owner: &'a session::EngineIdentity,
    },
}

/// Create a new task in `open` state and persist it, tagged to a resolved
/// session (mandatory-sessions design).
///
/// # Errors
/// Errors if `parent` is [`ParentSpec::Explicit`] and doesn't resolve to an
/// existing task. Errors on session resolution: [`SessionChoice::Named`]
/// unknown/closed → error; [`SessionChoice::Resolve`] with no open session
/// for `project`, or with 2+ open and not exactly one owned by the caller →
/// error telling the agent to run `llmenv task session start` or pass
/// `--session`; otherwise auto-resolved.
pub fn add_task(
    state_dir: &Path,
    title: &str,
    parent: ParentSpec<'_>,
    session: SessionChoice<'_>,
    project: &str,
) -> anyhow::Result<Task> {
    let new = NewTask {
        title,
        ..NewTask::default()
    };
    add_task_with(state_dir, &new, parent, session, project)
}

/// What a new task starts with.
#[derive(Debug, Clone, Copy, Default)]
pub struct NewTask<'a> {
    pub title: &'a str,
    /// What a cold reader needs to do the task. Written in the same save that creates the task.
    /// An empty string means no detail.
    pub detail: Option<&'a str>,
    /// Whether the task is queued, parallel, or a sub-task (#2455).
    pub placement: Placement<'a>,
    /// A task that must be done before this one starts: written as a `blocked_on` edge.
    pub after: Option<&'a str>,
    /// A progress note that the task starts with, written in the same save that creates the task
    /// (#3617).
    pub note: Option<&'a str>,
}

/// [`add_task`] for a task that starts with a detail (#2339).
///
/// # Errors
/// The same errors as [`add_task`].
pub fn add_task_with(
    state_dir: &Path,
    new: &NewTask<'_>,
    parent: ParentSpec<'_>,
    session: SessionChoice<'_>,
    project: &str,
) -> anyhow::Result<Task> {
    // The session is resolved under the store lock, so a session that another process finishes
    // after the check cannot take the new task.
    let task = with_store_lock(state_dir, || {
        let resolved = session_choice::resolve_session_for_add(state_dir, session, project)?;
        // The `--other-session` note is part of the one save that creates the task, so a failed
        // note cannot leave a task that joined another conversation's session with no record.
        let text = format!(
            "Added with --other-session, which names the owner of session '{}'.",
            resolved.id
        );
        let new = NewTask {
            note: if resolved.overridden {
                Some(text.as_str())
            } else {
                new.note
            },
            ..*new
        };
        add_task_for_session_with(state_dir, &new, parent, &resolved.id)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// A test convenience: add a task with no detail to a session whose id is already known, which
/// skips the mandatory-session lookup. Production code calls [`add_task_for_session_with`].
///
/// # Errors
/// Errors if `parent` is [`ParentSpec::Explicit`] and doesn't resolve to an
/// existing task — same eager-validation reasoning as `block_task`'s `on`.
pub fn add_task_for_session(
    state_dir: &Path,
    title: &str,
    parent: ParentSpec<'_>,
    session_id: &str,
) -> anyhow::Result<Task> {
    let new = NewTask {
        title,
        ..NewTask::default()
    };
    add_task_for_session_with(state_dir, &new, parent, session_id)
}

/// Claim a task, transitioning it to `wip`.
///
/// `blocked_on` is a hard-block (#1164): an explicit dependency the user
/// configured on purpose, unlike the soft-block `parent` relationship (see
/// `cli/mod.rs`'s `Start` handler, which warns-but-allows on an undone
/// parent instead). A `blocked_on` reference resolves as done only once the
/// target task *and every one of its descendants* are done — so blocking on
/// a parent task alone covers its whole child set (the parallel fan-out
/// case) without hand-wiring a block edge per sibling.
///
/// # Errors
/// Errors if the task is already `done`, or if any `blocked_on` reference
/// isn't (recursively) done and `force` is `false`. A dangling `blocked_on`
/// reference (deleted/corrupt blocker file) fails closed — treated as
/// unmet, same rationale as [`blockers::unmet_blocker`]'s dangling-blocker handling.
/// `force` overrides both cases — the agent may know better than the
/// ordering hint.
pub fn start_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    force: bool,
) -> anyhow::Result<Task> {
    let target = target.into();
    let task = with_store_lock(state_dir, || {
        // One strict read feeds the name lookup, the blocker check, and the queue check (#3474).
        target.check()?;
        let snapshot = target.tasks(state_dir)?;
        let all_tasks: &StrictTasks = &snapshot;
        let slug = target.slug_in(state_dir, all_tasks)?;
        let mut task = load_task(state_dir, &slug)?;
        if task.state == TaskState::Done {
            anyhow::bail!(
                "task '{slug}' is already done; pass --reopen to move it back to open and \
                 start it again"
            );
        }
        if !force && !task.blocked_on.is_empty() {
            let by_slug: HashMap<&str, &Task> = all_tasks
                .as_slice()
                .iter()
                .map(|t| (t.slug.as_str(), t))
                .collect();
            let mut unmet: Vec<String> = Vec::new();
            for blocker in &task.blocked_on {
                let Some(undone) = first_undone_in_subtree(blocker, &slug, &by_slug)? else {
                    continue;
                };
                unmet.push(if undone == blocker.as_str() {
                    blocker.clone()
                } else {
                    format!("{blocker} (its descendant '{undone}' is not done)")
                });
            }
            if !unmet.is_empty() {
                anyhow::bail!(
                    "task '{slug}' is blocked on not-done task(s): {} \
                     (pass --force to start anyway)",
                    unmet.join(", ")
                );
            }
        }
        if !force
            && let Some(session_id) = &task.session
            && let Some(block) = relation::queue_block(
                &task,
                &session::filter::tasks_in_sessions(
                    all_tasks.as_slice(),
                    &std::iter::once(session_id.clone()).collect(),
                ),
            )
        {
            anyhow::bail!("{block}");
        }
        // The ancestors go first: if one cannot start, the sub-task stays `open`.
        let promoted = ancestors_to_start(state_dir, &task, force, all_tasks)?;
        let now = now_rfc3339();
        let names: Vec<String> = promoted.iter().map(|a| a.slug.clone()).collect();
        for (saved, mut ancestor) in promoted.into_iter().enumerate() {
            ancestor.state = TaskState::Wip;
            ancestor.updated_at = now.clone();
            save_task(state_dir, &ancestor)
                .map_err(|e| ancestors::promotion_failed(&slug, &names, saved, e))?;
        }
        task.state = TaskState::Wip;
        task.updated_at = now;
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// Bump the `last_activity` of the session a task is tagged to, if any — so a
/// session with active `add`/`start`/`done`/`note` traffic reads as "recent"
/// in `session ls` and the `session start` checkpoint. Called *after* the
/// task mutation has already been committed. Best-effort by design: a
/// failure to bump the timestamp (pure display bookkeeping) must never turn
/// an already-persisted task change into a reported error — log and move on.
fn touch_task_session(state_dir: &Path, task: &Task) {
    if let Some(session_id) = &task.session
        && let Err(e) = session::touch_last_activity(state_dir, session_id)
    {
        // error level, because the default log filter drops warnings. A session that cannot load
        // is a real fault, even though the task change is already saved.
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: the task change is saved, but the activity time of session {session_id} is \
             not, so the session can look idle: {e:#}. Fix: run `llmenv task session show`."
        ));
    }
}

/// Delete a task outright (#905) — for a task that's being deliberately
/// abandoned, not just reshuffled. Returns the deleted task. Doesn't touch
/// other tasks' `parent`/`blocked_on` references to it, which already
/// tolerate a dangling target the same way a deleted blocker does (see
/// `start_task`'s warning path).
pub fn delete_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
) -> anyhow::Result<Task> {
    let target = target.into();
    with_store_lock(state_dir, || {
        let slug = target.slug(state_dir)?;
        let task = load_task(state_dir, &slug)?;
        std::fs::remove_file(task_path(state_dir, &slug))?;
        StrictTasks::record_write();
        Ok(task)
    })
}

/// Mark a task done from any prior state, skipping the start check
/// (`force = true`). Production callers use [`complete_task`]; test fixtures
/// use this shorter form to build a done task without starting it.
#[cfg(test)]
pub fn done_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    complete_task(state_dir, input, true).map(|c| c.task)
}

/// The result of [`complete_task`]: the saved task and the state it left.
#[derive(Debug, Clone)]
pub struct Completed {
    pub task: Task,
    prior: TaskState,
    /// Sub-tasks that were not done when `--force` closed the parent (#2455).
    undone_children: Vec<String>,
    /// The done and total counts of the task's session after the close, from the read that the
    /// close made. `None` for a task with no session (#3482).
    session_progress: Option<(u64, u64)>,
}

impl Completed {
    /// A note when `force` closed a parent whose sub-tasks were not done.
    #[must_use]
    pub fn undone_children_note(&self) -> Option<String> {
        (!self.undone_children.is_empty()).then(|| {
            format!(
                "Note: '{}' was closed while its sub-tasks were not done ({}); --force skipped \
                 the check.",
                self.task.slug,
                self.undone_children.join(", ")
            )
        })
    }

    /// A note when `force` closed a task that was never started. The CLI prints
    /// it so the skipped start stays visible, and names the way back.
    #[must_use]
    pub fn skipped_start_note(&self) -> Option<String> {
        (self.prior == TaskState::Open).then(|| {
            format!(
                "Note: '{slug}' was never started (open -> done); --force skipped the start. If \
                 its work is not finished, run `llmenv task start --reopen {slug}`.",
                slug = self.task.slug
            )
        })
    }
}

/// Mark a task done and report the state it was in before (#2416). A task that
/// is still `open` was never started, so no work was tracked; refuse it unless
/// `force` says the work is done without tracking. `done` stays idempotent.
///
/// # Errors
/// Errors if `input` does not resolve to a task, the task is `open` and `force`
/// is false, or the save fails.
pub fn complete_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    force: bool,
) -> anyhow::Result<Completed> {
    close_task(state_dir, target.into(), force, None)
}

/// [`complete_task`] for a named caller. A `done --force` that closes an `open`, `wip`, or
/// `waiting` task records the prior state and the caller in a note (#2585). The note saves under
/// the same store lock as the state change, so a failed note cannot leave a closed task without
/// its record.
///
/// # Errors
/// Errors as [`complete_task`] does.
pub fn complete_task_by<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    force: bool,
    caller: &ownership::Caller,
) -> anyhow::Result<Completed> {
    close_task(state_dir, target.into(), force, Some(caller))
}

fn close_task(
    state_dir: &Path,
    target: TaskTarget<'_>,
    force: bool,
    caller: Option<&ownership::Caller>,
) -> anyhow::Result<Completed> {
    let completed = with_store_lock(state_dir, || {
        // One strict read feeds the name lookup and the sub-task check (#3471, #3474).
        target.check()?;
        let snapshot = target.tasks(state_dir)?;
        let all_tasks: &StrictTasks = &snapshot;
        let slug = target.slug_in(state_dir, all_tasks)?;
        let mut task = load_task(state_dir, &slug)?;
        let prior = task.state;
        if prior == TaskState::Open && !force {
            // The advice names a start only when the start would be accepted (#3773).
            let by_slug: HashMap<&str, &Task> = all_tasks
                .as_slice()
                .iter()
                .map(|t| (t.slug.as_str(), t))
                .collect();
            match startability::refusal(&task, all_tasks.as_slice(), &by_slug, false) {
                Some(why) => anyhow::bail!(
                    "'{slug}' was never started (open), and the start is refused: {why}. Pass \
                     --force if it is done."
                ),
                None => anyhow::bail!(
                    "'{slug}' was never started (open). Run `llmenv task start {slug}` and \
                     finish the work, or pass --force if it is done."
                ),
            }
        }
        let undone = relation::undone_descendants(&slug, all_tasks);
        if !force && !undone.is_empty() {
            anyhow::bail!("{}", relation::undone_children_message(&slug, &undone));
        }
        let undone_children: Vec<String> = undone.iter().map(|t| t.slug.clone()).collect();
        // A repeat done changes nothing, so `updated_at` keeps the time of the first close (#2585).
        if prior != TaskState::Done {
            let now = now_rfc3339();
            if let Some(caller) = caller
                && force
            {
                task.notes.push(TaskNote {
                    at: now.clone(),
                    text: forced_close_text(prior, caller),
                });
            }
            task.state = TaskState::Done;
            task.updated_at = now;
            save_task(state_dir, &task)?;
        }
        let session_progress = progress_after_close(all_tasks, &task);
        Ok(Completed {
            task,
            prior,
            undone_children,
            session_progress,
        })
    })?;
    touch_task_session(state_dir, &completed.task);
    Ok(completed)
}

/// The note that a forced close leaves: the prior state and the caller.
fn forced_close_text(prior: TaskState, caller: &ownership::Caller) -> String {
    let prior = format!("{prior:?}").to_lowercase();
    format!(
        "Closed by done --force from {prior} (by {}).",
        caller.describe()
    )
}

/// Mark a task `waiting` — blocked on something outside the agent's control
/// (typically human input) rather than actively being worked. `reason` is
/// recorded as a note so `llmenv task show` carries the context. Resume with
/// [`start_task`], which accepts `waiting` as a valid prior state.
///
/// # Errors
/// Errors if the task is already done.
pub fn wait_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    reason: &str,
) -> anyhow::Result<Task> {
    let target = target.into();
    let task = with_store_lock(state_dir, || {
        let slug = target.slug(state_dir)?;
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
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// Resolve a `--delete-note` argument to an index into `notes`: either a
/// literal 0-based index, or the exact RFC3339 `at` timestamp of a note (the
/// `at` values `task show` prints alongside each note).
fn resolve_note_index(notes: &[TaskNote], id: &str) -> anyhow::Result<usize> {
    if let Ok(idx) = id.parse::<usize>() {
        return if idx < notes.len() {
            Ok(idx)
        } else {
            Err(anyhow::anyhow!(
                "note index {idx} out of range ({} note(s))",
                notes.len()
            ))
        };
    }
    notes
        .iter()
        .position(|n| n.at == id)
        .ok_or_else(|| anyhow::anyhow!("no note found with index or timestamp '{id}'"))
}

/// True when `err` was caused by a file that does not exist.
fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

/// SessionStart hook: if any `wip` tasks exist, build a reminder listing them
/// before new work starts. Empty string when there are none, or on any
/// internal error (logged to stderr, never propagated — hooks must never
/// block the agent).
///
/// Scoped to the current project (#949) via [`tasks_for_current_project`] — a
/// `wip`/`waiting` task from a different project sharing this task store must
/// never surface here. See [`wip_reminder`] for why, within the project, the
/// footer never presumes a listed task belongs to this conversation (#1028).
pub fn session_start_reminder(state_dir: &Path) -> String {
    // A store that cannot be read leaves the lists empty. The empty-session reminder below names
    // the error, so the agent still sees it.
    // A session file that cannot be read is named here, the same way the Stop reminder names it,
    // so its tasks are not dropped without a word (#2665).
    let mut read_warning = String::new();
    // One read of the tasks and the sessions feeds every reminder of this hook run (#3468).
    let stored = Stored::read(state_dir);
    let tasks = match stored.tasks() {
        Ok(tasks) => match tasks_for_current_project(&stored, tasks.as_slice()) {
            Ok(tasks) => tasks,
            Err(e) => {
                read_warning = store_warning_text(&e);
                Vec::new()
            }
        },
        // The reminders below repeat this text when the project resolves, and
        // `combine_reminders` drops the copy. Without a project they do not run, so the text
        // must come from here.
        Err(e) => {
            read_warning = store_warning_text(&e.context("the task store cannot be read"));
            Vec::new()
        }
    };
    combine_reminders([
        read_warning,
        for_current_project(|project| {
            session::resume_reminder_lines(stored.open_sessions_visible_to(project, None))
        }),
        wip_reminder(
            &tasks,
            "In-progress task(s) in this project",
            "Each is tagged with the session that started it. Resume one only if you \
             recognize it as your own earlier work in this conversation, then run \
             `llmenv task done <slug>` once finished. If you don't recognize a task, a \
             different, possibly still-active session owns it — leave it alone.",
        ),
        waiting_reminder(&tasks),
        session_finish_reminders(None, &stored),
        for_current_project(|project| match store_for_reminder(project, &stored) {
            Ok(tasks) => empty_session_lines(&stored, tasks.as_slice(), project, None).join("\n\n"),
            Err(warning) => warning,
        }),
    ])
}

/// Stop hook (end-of-turn skip detection): if `wip` tasks remain at the end
/// of a turn, list them so the agent can update or finish its own.
///
/// Only `wip` tasks are listed one by one. A session with `open` tasks and
/// nothing in progress gets one line that names its next task
/// ([`idle_sessions`], #2338), because an agent that adds steps and never
/// starts them needs the reminder most. `waiting` tasks are deliberately silent on Stop — they're correctly paused
/// on something outside the agent's control, and re-injecting their FYI on
/// every turn nags about a state that is meant to be quiet (they still show at
/// session start via [`session_start_reminder`]).
///
/// `caller` is the conversation id of the agent that stops. With an id, the reminder names only
/// the sessions that conversation owns, because a reminder about another agent's session is not
/// something this agent can act on (#2511). Without an id every session of the project is named.
/// Repeat suppression is not done here: see `hook_run::stop_dedupe`.
///
/// Scoped to the current project (#949) via [`tasks_for_current_project`] — a
/// `wip` task from a different project sharing this task store must never
/// surface here. See [`wip_reminder`] for why, within the project, the
/// footer never presumes a listed task belongs to this conversation (#1028).
pub fn stop_hook_reminder(stored: &Stored, caller: Option<&str>) -> String {
    // The caller reads the tasks and the sessions once for the whole Stop run, task tracking
    // included (#3466, #3468, #3472).
    let (tasks, store_warning) = match tasks_visible_to(caller, stored) {
        Ok(tasks) => (tasks, String::new()),
        Err(e) => (Vec::new(), store_warning_text(&e)),
    };
    combine_reminders([
        wip_reminder(
            &tasks,
            "Task(s) marked in-progress in this project",
            "Each is tagged with the session that started it. If you recognize one as a \
             task you started earlier in this conversation, run `llmenv task done <slug>` \
             when finished, or keep working — don't stop mid-task. If you don't recognize \
             starting a listed task, it belongs to a different, possibly still-active \
             session — leave it alone; never resume it or drive it to completion on the \
             assumption that it's yours. If blocked on your own task, exhaust safe \
             autonomous remediation first (retry, an alternate approach, a diagnostic); \
             only then ask the user once with a specific actionable question, and \
             `llmenv task note <slug> \"...\"` the blocker instead of repeating status.",
        ),
        session_finish_reminders(caller, stored),
        idle_session_reminders(caller, stored),
        for_current_project(|project| session::missing_context_reminders(project, caller, stored)),
        store_warning,
    ])
}

/// An open session that holds `open` tasks while none of its tasks is `wip`
/// or `waiting`: work is planned, but nothing is in progress (#2338).
#[derive(Debug, Clone)]
struct IdleSession {
    session: session::Session,
    /// The task to start next: the first `open` task in execution order that `task start`
    /// accepts. `None` when the start refuses every open task, and `loop_error` then says why.
    next: Option<Task>,
    open_count: usize,
    /// Set when no task is actionable: the slug of the first open task that a blocker keeps from
    /// starting, and why (a loop in the blocker walk, or a blocker that is not done). The reminder
    /// names the reason instead of advising a start that cannot run (#3618, #3764).
    loop_error: Option<(String, String)>,
}

/// Every open session for `project` that [`IdleSession`] describes. Kept
/// separate from the Stop wording so other end-of-turn checks can share it
/// (#2339). A `waiting` task makes a session quiet on purpose, so such a
/// session is not idle.
#[must_use]
fn idle_sessions(open: Vec<Session>, all_tasks: &[Task]) -> Vec<IdleSession> {
    let by_slug: HashMap<&str, &Task> = all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    open.into_iter()
        .filter_map(|session| {
            let tasks: Vec<Task> = all_tasks
                .iter()
                .filter(|t| t.session.as_deref() == Some(session.id.as_str()))
                .cloned()
                .collect();
            if tasks
                .iter()
                .any(|t| matches!(t.state, TaskState::Wip | TaskState::Waiting))
            {
                return None;
            }
            let order = execution_order(&tasks);
            let open: Vec<&Task> = order
                .iter()
                .filter(|t| t.state == TaskState::Open)
                .collect();
            if open.is_empty() {
                return None;
            }
            let next = startability::first_startable(&open, all_tasks, &by_slug);
            let loop_error = match next {
                Some(_) => None,
                None => blockers::first_obstacle(&open, all_tasks, &by_slug),
            };
            Some(IdleSession {
                next: next.map(Task::to_owned),
                open_count: open.len(),
                loop_error,
                session,
            })
        })
        .collect()
}

/// The Stop reminder for each [`IdleSession`] in the current project. Like
/// [`session_finish_reminders`], it cannot tell whose session it names
/// (#1028), so the nudge is conditioned on the agent recognizing it.
fn idle_session_reminders(caller: Option<&str>, stored: &Stored) -> String {
    for_current_project(|project| idle_reminder_lines(project, caller, stored))
}

fn idle_reminder_lines(project: &str, caller: Option<&str>, stored: &Stored) -> String {
    let tasks = match store_for_reminder(project, stored) {
        Ok(tasks) => tasks,
        Err(warning) => return warning,
    };
    let open = match stored.open_sessions_visible_to(project, caller) {
        Ok(open) => open,
        Err(e) => return store_warning_text(&e),
    };
    let open_ids: Vec<String> = open.iter().map(|s| s.id.clone()).collect();
    let stalled = relation::stalled_parent_lines(tasks.as_slice(), &open_ids);
    let idle = idle_sessions(open, tasks.as_slice())
        .iter()
        .map(|idle| {
            let id = &idle.session.id;
            let label = idle.session.name.as_deref().unwrap_or(id.as_str());
            if let Some((looped, why)) = &idle.loop_error {
                return format!(
                    "Session '{label}' ({id}) has {count} open task(s) and none in progress, but \
                     '{looped}' cannot start: {why}. If you don't recognize the session, it \
                     belongs to a different session — leave it alone.",
                    count = idle.open_count,
                );
            }
            let Some(next) = &idle.next else {
                return format!(
                    "Session '{label}' ({id}) has {count} open task(s) and none in progress, but \
                     the start refuses every one of them. Run `llmenv task ls` to see why.",
                    count = idle.open_count,
                );
            };
            format!(
                "Session '{label}' ({id}) has {count} open task(s) and none in progress. If \
                 you recognize this as your own session, run `llmenv task start {next}` \
                 ({title}) before you work on that step. If that step needs the user or an \
                 outside event first, run `llmenv task wait {next} \"<reason>\"` instead. \
                 The session cannot be closed while any task is open. If you don't \
                 recognize it, it belongs to a different session — leave it alone.",
                count = idle.open_count,
                next = next.slug,
                title = next.title,
            )
        })
        .collect::<Vec<_>>();
    empty_session_lines(stored, tasks.as_slice(), project, caller)
        .into_iter()
        .chain(stalled)
        .chain(idle)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The queue order of two tasks: creation time, then slug for a tie. The tree listing and the
/// next-task lookup both use it, so they cannot disagree about which task is next.
pub fn queue_order(a: &Task, b: &Task) -> std::cmp::Ordering {
    llmenv_util::time::cmp_instants(&a.created_at, &b.created_at, "task created_at")
        .then_with(|| a.slug.cmp(&b.slug))
}

/// A line for each open session of `project` that holds no task at all (#2456). An empty session
/// is an error state: the work goes on with nothing recorded.
fn empty_session_lines(
    stored: &Stored,
    tasks: &[Task],
    project: &str,
    caller: Option<&str>,
) -> Vec<String> {
    let open = match stored.open_sessions_visible_to(project, caller) {
        Ok(open) => open,
        Err(e) => return vec![store_warning_text(&e)],
    };
    open.into_iter()
        .filter(|s| {
            !tasks
                .iter()
                .any(|t| t.session.as_deref() == Some(s.id.as_str()))
        })
        .map(|s| {
            let label = s.name.as_deref().unwrap_or(s.id.as_str());
            format!(
                "Session '{label}' ({}) is open and has no tasks. If you recognize it as your own \
                 session, add a task for the work you are doing with `llmenv task add \"<step>\"`, \
                 or finish it with `llmenv task session finish {}`. If you don't recognize it, it \
                 belongs to a different session — leave it alone.",
                s.id, s.id
            )
        })
        .collect()
}

/// Filter `tasks` down to those attributable to the current project
/// (resolved from the process's actual cwd — hooks run with cwd set to the
/// project directory — via [`project::current_tag`]): kept only if tagged to
/// a session — open or already closed, since a `wip`/`waiting` task's session
/// may no longer be "open" — whose `project` matches. Mirrors
/// [`session_finish_reminders`]'s own project resolution. Empty when the
/// project can't be resolved (degrades silently — hooks must never error).
fn tasks_for_current_project(stored: &Stored, tasks: &[Task]) -> anyhow::Result<Vec<Task>> {
    let project = match project::current_tag() {
        Ok(project) => project,
        Err(e) => {
            tracing::error!("project::current_tag failed, so the task reminder is skipped: {e:#}");
            llmenv_util::agent_notice::record(TAG_UNRESOLVED_NOTICE);
            return Ok(Vec::new());
        }
    };
    Ok(session::filter::tasks_in_sessions(
        tasks,
        &stored.session_ids_visible_to(&project, None)?,
    ))
}

/// The tasks of the current project that `caller` may be told about: all of them without a
/// conversation id, else those of sessions that [`session::Session`] does not hide from it
/// (#2511).
fn tasks_visible_to(caller: Option<&str>, stored: &Stored) -> anyhow::Result<Vec<Task>> {
    // An unresolved project must reach the Stop reminder as an error, not as an empty list
    // that looks like "no wip tasks" (#2658 review).
    let project = project::current_tag()
        .map_err(|e| anyhow::Error::new(e).context("cannot resolve the project tag"))?;
    // Strict session reads: a corrupt session file must reach the Stop reminder as an error,
    // not drop its tasks silently (#2628).
    let project_ids = stored.session_ids_visible_to(&project, None)?;
    let all = stored.tasks().context("the task store cannot be read")?;
    let mut tasks = session::filter::tasks_in_sessions(all.as_slice(), &project_ids);
    if caller.is_some() {
        let visible = stored.session_ids_visible_to(&project, caller)?;
        tasks.retain(|t| t.session.as_ref().is_some_and(|id| visible.contains(id)));
    }
    Ok(tasks)
}

/// Keep only tasks whose session is tagged to `project` — any session ever
/// tagged to it, open or closed — the shared filter behind `task ls
/// --current-project` (#1117). The SessionStart/Stop hook reminders apply the same rule to
/// their shared read ([`tasks_for_current_project`]). Legacy tasks with no `session` (predate
/// mandatory sessions) are dropped: they can't be attributed to any
/// project, and the conservative default is to never surface a task we
/// can't attribute.
///
/// `sessions` comes from the caller's strict read: a tolerant read would drop the tasks of a
/// corrupt session file without a word (#2665). The caller can then use the same read again
/// (#3479).
pub fn filter_tasks_for_project(
    sessions: &[session::Session],
    project: &str,
    tasks: Vec<Task>,
) -> Vec<Task> {
    let session_ids = session::filter::ids_visible(sessions, project, None);
    session::filter::tasks_in_sessions(&tasks, &session_ids)
}

/// Resolve the "current" task for `task show --current`/`--next` (#1117):
/// the `wip` task (most recently updated, if somehow more than one),
/// falling back to the most recently updated non-`done` task when nothing
/// is `wip`. `tasks` is expected to already be scoped to one session — this
/// makes no session judgment of its own.
#[must_use]
pub fn resolve_current_task(tasks: &[Task]) -> Option<Task> {
    most_recently_updated(tasks.iter().filter(|t| t.state == TaskState::Wip))
        .or_else(|| most_recently_updated(tasks.iter().filter(|t| t.state != TaskState::Done)))
        .cloned()
}

/// Resolve the next actionable task after `current`, in the same
/// parent-before-children execution order `task ls` displays (#926): walk
/// forward from `current`'s position among `session_tasks`, skipping `done`
/// and `waiting` tasks (a `waiting` task is paused on something outside the
/// agent's control, not a legitimate "next" step — see [`TaskState::Waiting`])
/// and any task whose `blocked_on` refs aren't all `done` (resolved against
/// `all_tasks`, since a blocker can live in a different session than the
/// task it blocks), and any task that `task start` refuses for the queue or an ancestor, as
/// [`startability::refusal`] decides (#3773). A task with a damaged parent link is skipped with no
/// message here. `None` if `current` isn't found in `session_tasks`, or nothing after it
/// qualifies.
#[must_use]
pub fn resolve_next_task(
    all_tasks: &[Task],
    session_tasks: &[Task],
    current: &Task,
) -> Option<Task> {
    let order = execution_order(session_tasks);
    let pos = order.iter().position(|t| t.slug == current.slug)?;
    let by_slug: HashMap<&str, &Task> = all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    order.into_iter().skip(pos + 1).find(|t| {
        !matches!(t.state, TaskState::Done | TaskState::Waiting)
            && startability::refusal(t, all_tasks, &by_slug, true).is_none()
    })
}

/// Every task after `current` that `task show --next` cannot offer because its blocker walk
/// loops or a parent link is damaged (#3777), each with the text of the loop and its fix, in
/// execution order. `resolve_next_task` skips such a task with no sign, so the caller asks this to
/// name each one (#3618).
#[must_use]
pub fn next_task_loops(
    all_tasks: &[Task],
    session_tasks: &[Task],
    current: &Task,
) -> Vec<(String, String)> {
    let order = execution_order(session_tasks);
    let Some(pos) = order.iter().position(|t| t.slug == current.slug) else {
        return Vec::new();
    };
    let by_slug: HashMap<&str, &Task> = all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    order
        .iter()
        .skip(pos + 1)
        .filter(|t| !matches!(t.state, TaskState::Done | TaskState::Waiting))
        .filter_map(|t| {
            blockers::blocker_walk_error(t, &by_slug)
                .or_else(|| ancestors::ancestor_damage(t, all_tasks))
                .map(|e| (t.slug.clone(), e))
        })
        .collect()
}

/// Parent-before-children execution order for a single session's tasks —
/// [`display_rows`]'s own forest ordering with a single (empty-priority)
/// group, reused as the walk order for `task show --next`'s parent/child
/// chaining so the two can never drift apart.
fn execution_order(tasks: &[Task]) -> Vec<Task> {
    display_rows(tasks.to_vec(), &[])
        .into_iter()
        .map(|r| r.task)
        .collect()
}

/// Run `reminder` for the current project. A project tag that cannot be resolved is logged, and
/// the reminder is skipped: a hook must never block the agent.
/// Shown to the agent when the task reminder cannot find its project tag.
const TAG_UNRESOLVED_NOTICE: &str = "llmenv: the task reminder was skipped: the project tag cannot \
     be resolved. Fix: run this from a git repository or a named project folder.";

fn for_current_project(reminder: impl FnOnce(&str) -> String) -> String {
    match project::current_tag() {
        Ok(project) => reminder(&project),
        Err(e) => {
            tracing::error!("project::current_tag failed, so the task reminder is skipped: {e:#}");
            llmenv_util::agent_notice::record(TAG_UNRESOLVED_NOTICE);
            String::new()
        }
    }
}

/// Join non-empty reminder strings with a blank line between them; empty
/// parts are dropped rather than leaving stray blank lines.
fn combine_reminders(parts: impl IntoIterator<Item = String>) -> String {
    // One unreadable session file reaches several reminders. Its warning is shown once.
    let mut seen = HashSet::new();
    parts
        .into_iter()
        .filter(|p| !p.is_empty() && seen.insert(p.clone()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a bullet list of tasks (`- title (slug)`), one per line.
pub fn render_task_list(tasks: &[&Task]) -> String {
    tasks
        .iter()
        .map(|t| task_line(t))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The shared `- title (slug)` prefix every task-list line starts with.
fn task_line(t: &Task) -> String {
    format!("- {} ({})", t.title, t.slug)
}

/// Builds the `wip`-task reminder (`header`/`footer` customized per caller).
/// Empty when no `wip` tasks exist. Takes the already-loaded task list so the
/// caller reads the store once and shares it with [`waiting_reminder`].
///
/// Callers reach this only via [`tasks_for_current_project`], which already
/// drops any task with no `session` — so every line here does have one to
/// show. There is no signal available (hooks and the `llmenv task` CLI are
/// independent processes with no shared conversation identity) to tell
/// whether that owning session is *this* conversation's own (#1028), so each
/// line names it and leaves the ownership judgment to the caller's
/// `header`/`footer` wording rather than baking in a bare title/slug list.
fn wip_reminder(tasks: &[Task], header: &str, footer: &str) -> String {
    let wip: Vec<&Task> = tasks.iter().filter(|t| t.state == TaskState::Wip).collect();
    if wip.is_empty() {
        return String::new();
    }
    let list = wip
        .iter()
        .map(|t| {
            // Defensive fallback only — never actually hit given the caller
            // contract above, but `Task.session` stays `Option` for legacy
            // tasks created before sessions were mandatory.
            let session = t.session.as_deref().unwrap_or("unknown session");
            format!(
                "{} [session: {session}]{}",
                task_line(t),
                relation::progress_suffix(&t.slug, tasks)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}:\n{list}\n{footer}")
}

/// Builds the `waiting`-task FYI. Deliberately different in tone from
/// [`wip_reminder`]: a `wip` task is actionable right now, a `waiting` one is
/// correctly paused on something outside the agent's control — nagging to
/// "take action" on it would be actively wrong, so it gets a plain, no-action
/// note. Surfaced only at session start (resume/wake); never on Stop, where
/// re-injecting it every turn would nag about a state meant to be quiet. Empty
/// when no `waiting` tasks exist. Takes the already-loaded task list (shared
/// with [`wip_reminder`]) so session start reads the store once.
fn waiting_reminder(tasks: &[Task]) -> String {
    let waiting: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.state == TaskState::Waiting)
        .collect();
    if waiting.is_empty() {
        return String::new();
    }
    let list = render_task_list(&waiting);
    format!(
        "Task(s) waiting on external input (no action needed until it \
         clears — see each task's notes for what's being waited on):\n{list}"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod ownership_props_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod read_once_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod reject_cycle_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod store_guard_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod walk_tests;
