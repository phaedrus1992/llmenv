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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    #[serde(default)]
    pub blocked_on: Vec<String>,
    #[serde(default)]
    notes: Vec<TaskNote>,
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

/// Run `f` while holding an exclusive lock on the whole task store, so
/// concurrent `llmenv task` invocations (e.g. from multiple Claude Code
/// sessions on the same project) serialize their read-modify-write cycles
/// instead of racing on the same file. Blocks until the lock is acquired.
fn with_store_lock<T>(
    state_dir: &Path,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = tasks_dir(state_dir);
    llmenv_paths::create_dir_owner_only(&dir)?;
    let mut open_options = std::fs::OpenOptions::new();
    open_options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }
    let lock_file = open_options.open(dir.join(".lock"))?;
    lock_file.lock()?;
    f()
}

/// Current RFC3339 timestamp (UTC).
/// Nanosecond precision, not seconds: [`ParentSpec::Auto`] (#929) picks the
/// implicit-chain parent by comparing `created_at` strings, and several
/// sequential `task add` invocations (agent tool calls, shell loops) commonly
/// land within the same wall-clock second — second precision made that tie
/// resolve to readdir order, which is arbitrary, not creation order.
fn now_rfc3339() -> String {
    humantime::format_rfc3339_nanos(std::time::SystemTime::now()).to_string()
}

/// Derive a kebab-case slug from a task title: lowercase, first ~6 words,
/// non-alphanumeric runs collapsed to a single `-`, leading/trailing `-`
/// trimmed. Pure function — collision uniquification happens separately in
/// [`unique_slug`], which needs the store directory.
pub(crate) fn slugify(title: &str) -> String {
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
    llmenv_paths::create_dir_owner_only(&dir)?;
    let json = serde_json::to_string_pretty(task)?;
    llmenv_paths::write_owner_only_atomic(&task_path(state_dir, &task.slug), json.as_bytes())?;
    Ok(())
}

/// Load a single task by its exact slug.
pub fn load_task(state_dir: &Path, slug: &str) -> anyhow::Result<Task> {
    let content = std::fs::read_to_string(task_path(state_dir, slug))?;
    Ok(serde_json::from_str(&content)?)
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
            tracing::warn!(error = %e, "failed to read tasks dir; treating as empty");
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
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "skipping unreadable directory entry");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
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
                tracing::warn!(error = %e, path = %path.display(), "skipping unreadable task file");
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "skipping corrupt task file");
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
    let order = |a: &&Task, b: &&Task| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.slug.cmp(&b.slug))
    };
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

fn visit<'a>(
    task: &'a Task,
    depth: usize,
    children: &std::collections::HashMap<&'a str, Vec<&'a Task>>,
    visited: &mut HashSet<&'a str>,
    rows: &mut Vec<DisplayRow>,
) {
    if !visited.insert(task.slug.as_str()) {
        return;
    }
    rows.push(DisplayRow {
        depth,
        task: task.clone(),
    });
    if let Some(kids) = children.get(task.slug.as_str()) {
        for child in kids {
            visit(child, depth + 1, children, visited, rows);
        }
    }
}

/// Title of the most recently updated `wip`/`waiting` task among tasks
/// tagged to any of `session_ids` — the statusline's "what's in progress
/// right now" fill-in (#905). `None` when nothing matching is currently
/// `wip`/`waiting`, or when `session_ids` is empty.
#[must_use]
pub fn current_wip_title(state_dir: &Path, session_ids: &[String]) -> Option<String> {
    if session_ids.is_empty() {
        return None;
    }
    let tasks = list_tasks(state_dir);
    most_recently_updated(tasks.iter().filter(|t| {
        matches!(t.state, TaskState::Wip | TaskState::Waiting)
            && t.session
                .as_deref()
                .is_some_and(|sid| session_ids.iter().any(|s| s == sid))
    }))
    .map(|t| t.title.clone())
}

/// The most recently updated of `tasks`, by `updated_at` string comparison
/// (RFC3339 sorts lexicographically). Ties resolve to `max_by`'s documented
/// last-element-wins rule — callers that need a stable tiebreak on ties sort
/// first (see [`append_forest`]'s `created_at`-then-`slug` order).
fn most_recently_updated<'a>(tasks: impl Iterator<Item = &'a Task>) -> Option<&'a Task> {
    tasks.max_by(|a, b| a.updated_at.cmp(&b.updated_at))
}

/// The most recently *created* of `tasks`, by `created_at` string comparison
/// (RFC3339 sorts lexicographically). Used by [`ParentSpec::Auto`] to find
/// the implicit-chain parent — `created_at`, not `updated_at`, since a task
/// finishing (bumping `updated_at`) shouldn't retroactively change which
/// task a *new* add chains onto. Ties resolve to `max_by`'s documented
/// last-element-wins rule, same caveat as [`most_recently_updated`].
fn most_recently_created<'a>(tasks: impl Iterator<Item = &'a Task>) -> Option<&'a Task> {
    tasks.max_by(|a, b| a.created_at.cmp(&b.created_at))
}

/// How `add_task`/`add_task_for_session` should set the new task's `parent`.
/// A plain `Option<&str>` can't distinguish "no `--parent` given, use the
/// implicit-chain default" from "explicitly no parent" — this can (#929).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentSpec<'a> {
    /// No `--parent` given: default to the most recently created task in
    /// the same session (or no parent if this is the session's first task).
    Auto,
    /// `--parent <id>` given explicitly.
    Explicit(&'a str),
    /// `--no-parent` given explicitly: a deliberate top-level task, no
    /// implicit chaining even if the session already has other tasks.
    Detached,
}

/// Create a new task in `open` state and persist it, tagged to a resolved
/// session (mandatory-sessions design).
///
/// # Errors
/// Errors if `parent` is [`ParentSpec::Explicit`] and doesn't resolve to an
/// existing task. Errors on session resolution: `session_id` explicit but
/// unknown/closed → error; omitted with zero or 2+ open sessions for
/// `project` → error telling the agent to run `llmenv task session start`
/// or pass `--session`; omitted with exactly one open session for `project`
/// → auto-resolved.
pub fn add_task(
    state_dir: &Path,
    title: &str,
    parent: ParentSpec<'_>,
    session_id: Option<&str>,
    project: &str,
) -> anyhow::Result<Task> {
    let resolved_session = resolve_session_for_add(state_dir, session_id, project)?;
    let task = add_task_for_session(state_dir, title, parent, &resolved_session)?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// `add_task`, but with the session id already resolved — skips the
/// mandatory-session lookup dance. Used by `add_task` itself, and directly by
/// `session.rs`'s own tests (and any caller that already has a validated
/// session id in hand, e.g. the CLI's `--session <id>` path).
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
    with_store_lock(state_dir, || {
        let dir = tasks_dir(state_dir);
        let parent_slug = match parent {
            ParentSpec::Explicit(p) => Some(resolve_identifier(state_dir, p)?),
            ParentSpec::Detached => None,
            ParentSpec::Auto => {
                most_recently_created(session::tasks_in_session(state_dir, session_id).iter())
                    .map(|t| t.slug.clone())
            }
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
            session: Some(session_id.to_string()),
            created_at: now.clone(),
            updated_at: now,
        };
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// Resolve which session a `task add` belongs to, per the mandatory-sessions
/// rules. An explicit `session_id` is honored as long as it names an
/// existing, open session (any project — an explicit id is a deliberate
/// choice). Omitted, it auto-resolves to the current project's single open
/// session, or errors on zero/2+.
fn resolve_session_for_add(
    state_dir: &Path,
    session_id: Option<&str>,
    project: &str,
) -> anyhow::Result<String> {
    if let Some(id) = session_id {
        // Fallible `try_list_sessions` rather than the tolerant `list_sessions`:
        // an unreadable store must error out here rather than be misread as "no
        // such session" (#1112) — this is the same resolution step `TaskCreate`
        // (via `add_task`) runs through, so the hook redirect's own unreadable-
        // store guard in `task_tools::create` would otherwise be undone here.
        let exists_open = session::try_list_sessions(state_dir)?
            .into_iter()
            .any(|s| s.id == id && s.is_open());
        if !exists_open {
            anyhow::bail!("session '{id}' does not exist or is not open");
        }
        return Ok(id.to_string());
    }
    let open = session::try_open_sessions_for_project(state_dir, project)?;
    match open.len() {
        0 => anyhow::bail!(
            "no open session for this project — run `llmenv task session start` first, \
             or pass --session <id>"
        ),
        1 => Ok(open[0].id.clone()),
        n => anyhow::bail!(
            "{n} open sessions for this project — pass --session <id>, or see \
             `llmenv task session ls`"
        ),
    }
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
    if !llmenv_paths::is_valid_short_name(input) {
        anyhow::bail!("'{input}' is not a valid task identifier");
    }
    if task_path(state_dir, input).exists() {
        return Ok(input.to_string());
    }
    // Fallible `try_list_tasks` rather than the tolerant `list_tasks`: an
    // unreadable store must error out here rather than be misread as "no task
    // found" (#1112) — this is the resolution step every mutating task command
    // (and `TaskUpdate`'s hook redirect) runs through.
    let matches: Vec<String> = try_list_tasks(state_dir)?
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
/// unmet, same rationale as [`is_actionable`]'s dangling-blocker handling.
/// `force` overrides both cases — the agent may know better than the
/// ordering hint.
pub fn start_task(state_dir: &Path, input: &str, force: bool) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        if task.state == TaskState::Done {
            anyhow::bail!("task '{slug}' is already done; cannot start it again");
        }
        if !force && !task.blocked_on.is_empty() {
            let all_tasks = list_tasks(state_dir);
            let by_slug: HashMap<&str, &Task> =
                all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
            let unmet: Vec<&str> = task
                .blocked_on
                .iter()
                .map(String::as_str)
                .filter(|blocker_slug| !is_done_including_descendants(blocker_slug, &by_slug))
                .collect();
            if !unmet.is_empty() {
                anyhow::bail!(
                    "task '{slug}' is blocked on not-done task(s): {} \
                     (pass --force to start anyway)",
                    unmet.join(", ")
                );
            }
        }
        task.state = TaskState::Wip;
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// Soft-block advisory for [`start_task`] (#1164): `None` when `task` has no
/// `parent`, the parent can't be loaded (dangling reference — nothing
/// meaningful to warn about once the parent itself is gone), or the parent
/// is already `done`. Otherwise a message naming the parent and its current
/// state, for a caller to surface however it prefers (println, appended to
/// a hook's response text, …) — starting the task is never blocked by this,
/// unlike an unmet `blocked_on` reference.
#[must_use]
pub fn parent_soft_block_warning(state_dir: &Path, task: &Task) -> Option<String> {
    let parent_slug = task.parent.as_ref()?;
    let parent = load_task(state_dir, parent_slug).ok()?;
    if parent.state == TaskState::Done {
        return None;
    }
    Some(format!(
        "Note: parent task '{parent_slug}' isn't done yet ({}) — starting '{}' anyway. \
         Use `llmenv task block {} --on {parent_slug}` instead if this really can't start \
         until the parent finishes.",
        parent.state.as_str(),
        task.slug,
        task.slug
    ))
}

/// Bump the `last_activity` of the session a task is tagged to, if any — so a
/// session with active `add`/`start`/`done`/`note` traffic reads as "recent"
/// in `session ls` and the `session start` checkpoint. Called outside
/// `with_store_lock` (`touch_last_activity` takes the lock itself), *after*
/// the task mutation has already been committed. Best-effort by design: a
/// failure to bump the timestamp (pure display bookkeeping) must never turn
/// an already-persisted task change into a reported error — log and move on.
fn touch_task_session(state_dir: &Path, task: &Task) {
    if let Some(session_id) = &task.session
        && let Err(e) = session::touch_last_activity(state_dir, session_id)
    {
        tracing::warn!(
            error = %e, session_id = %session_id,
            "failed to update last_activity after a task change (task change itself is already saved)"
        );
    }
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
    let task = with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        task.state = TaskState::Done;
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// Append a timestamped progress note to a task.
pub fn note_task(state_dir: &Path, input: &str, text: &str) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;
        task.notes.push(TaskNote {
            at: now_rfc3339(),
            text: text.to_string(),
        });
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// Mark a task `waiting` — blocked on something outside the agent's control
/// (typically human input) rather than actively being worked. `reason` is
/// recorded as a note so `llmenv task show` carries the context. Resume with
/// [`start_task`], which accepts `waiting` as a valid prior state.
///
/// # Errors
/// Errors if the task is already done.
pub fn wait_task(state_dir: &Path, input: &str, reason: &str) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
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
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
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

/// What to change in an [`edit_task`] call. Every field defaults to "leave
/// unchanged" / "nothing to add or remove" — a call with every field at its
/// default is a legal no-op (#930), still bumping `updated_at`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskEdit<'a> {
    /// New title, if changing it.
    pub title: Option<&'a str>,
    /// Set the parent to this id. Mutually exclusive with `no_parent` — the
    /// CLI enforces this with `conflicts_with`; if both are set here,
    /// `parent` wins.
    pub parent: Option<&'a str>,
    /// Clear the parent.
    pub no_parent: bool,
    /// Ids to add to `blocked_on`. Idempotent — an id already present is a
    /// no-op, same as `block_task`.
    pub block_on: &'a [String],
    /// Ids to remove from `blocked_on`. Idempotent — an id not present is a
    /// no-op.
    pub unblock: &'a [String],
    /// Append a new note.
    pub add_note: Option<&'a str>,
    /// Remove a note, identified by its 0-based index or its exact RFC3339
    /// `at` timestamp.
    pub delete_note: Option<&'a str>,
}

/// Mutate an existing task's title, parent, `blocked_on` set, and notes in a
/// single load-mutate-save cycle (#930).
///
/// # Errors
/// - `edit.parent` doesn't resolve to an existing task, names `input` itself,
///   or would make `input` its own ancestor (cycle) — see [`reject_cycle`].
/// - Any `edit.block_on` id doesn't resolve to an existing task, or names
///   `input` itself — same eager-validation reasoning as [`block_task`].
/// - Any `edit.unblock` id doesn't resolve to an existing task.
/// - `edit.delete_note` doesn't match an existing note's index or timestamp.
pub fn edit_task(state_dir: &Path, input: &str, edit: &TaskEdit<'_>) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
        let slug = resolve_identifier(state_dir, input)?;
        let mut task = load_task(state_dir, &slug)?;

        if let Some(title) = edit.title {
            task.title = title.to_string();
        }

        if let Some(parent_input) = edit.parent {
            let parent_slug = resolve_identifier(state_dir, parent_input)?;
            if parent_slug == slug {
                anyhow::bail!("task '{slug}' cannot be its own parent");
            }
            reject_cycle(state_dir, &slug, &parent_slug)?;
            task.parent = Some(parent_slug);
        } else if edit.no_parent {
            task.parent = None;
        }

        for on in edit.block_on {
            let on_slug = resolve_identifier(state_dir, on)?;
            if on_slug == slug {
                anyhow::bail!("task '{slug}' cannot be blocked on itself");
            }
            if !task.blocked_on.contains(&on_slug) {
                task.blocked_on.push(on_slug);
            }
        }

        for on in edit.unblock {
            let on_slug = resolve_identifier(state_dir, on)?;
            task.blocked_on.retain(|b| b != &on_slug);
        }

        if let Some(text) = edit.add_note {
            task.notes.push(TaskNote {
                at: now_rfc3339(),
                text: text.to_string(),
            });
        }

        if let Some(id) = edit.delete_note {
            let idx = resolve_note_index(&task.notes, id)?;
            task.notes.remove(idx);
        }

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

/// Errs if setting `slug`'s parent to `new_parent` would make `slug` its own
/// ancestor — walks up from `new_parent` and checks `slug` never reappears. A
/// `visited` guard (mirroring [`append_forest`]'s) makes a pre-existing
/// malformed cycle elsewhere in the store terminate instead of looping
/// forever; that's not this call's problem to fix. A corrupt/unreadable file
/// encountered while walking up also stops the walk there (treated the same
/// as "no parent") — same fail-open tolerance `append_forest` and
/// `start_task`'s dangling-`blocked_on` handling already use for a broken
/// link elsewhere in the store; it can't itself be part of a cycle back to
/// `slug` since the walk can't see past it either way.
fn reject_cycle(state_dir: &Path, slug: &str, new_parent: &str) -> anyhow::Result<()> {
    let mut current = new_parent.to_string();
    let mut visited = HashSet::new();
    loop {
        if current == slug {
            anyhow::bail!("setting parent to '{new_parent}' would make '{slug}' its own ancestor");
        }
        if !visited.insert(current.clone()) {
            return Ok(());
        }
        match load_task(state_dir, &current).ok().and_then(|t| t.parent) {
            Some(p) => current = p,
            None => return Ok(()),
        }
    }
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
    let tasks = tasks_for_current_project(state_dir, list_tasks(state_dir));
    combine_reminders([
        wip_reminder(
            &tasks,
            "In-progress task(s) in this project",
            "Each is tagged with the session that started it. Resume one only if you \
             recognize it as your own earlier work in this conversation, then run \
             `llmenv task done <slug>` once finished. If you don't recognize a task, a \
             different, possibly still-active session owns it — leave it alone.",
        ),
        waiting_reminder(&tasks),
        session_finish_reminders(state_dir),
    ])
}

/// Stop hook (end-of-turn skip detection): if `wip` tasks remain at the end
/// of a turn, list them so the agent can update or finish its own.
///
/// Only `wip` tasks are actionable at end-of-turn, so only they surface here.
/// `waiting` tasks are deliberately silent on Stop — they're correctly paused
/// on something outside the agent's control, and re-injecting their FYI on
/// every turn nags about a state that is meant to be quiet (they still show at
/// session start via [`session_start_reminder`]).
///
/// Fires on every Stop while a `wip` task remains (advisory-only, never
/// blocks) — a session-scoped mtime filter (only fire if *this* session
/// touched the store) was considered and rejected: it would still need to
/// fire at least once per Stop to check, so it buys no frequency reduction
/// for real tracking complexity (threading session_id through task state).
///
/// Scoped to the current project (#949) via [`tasks_for_current_project`] — a
/// `wip` task from a different project sharing this task store must never
/// surface here. See [`wip_reminder`] for why, within the project, the
/// footer never presumes a listed task belongs to this conversation (#1028).
pub fn stop_hook_reminder(state_dir: &Path) -> String {
    let tasks = tasks_for_current_project(state_dir, list_tasks(state_dir));
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
        session_finish_reminders(state_dir),
    ])
}

/// Filter `tasks` down to those attributable to the current project
/// (resolved from the process's actual cwd — hooks run with cwd set to the
/// project directory — via [`project::current_tag`]): kept only if tagged to
/// a session — open or already closed, since a `wip`/`waiting` task's session
/// may no longer be "open" — whose `project` matches. Mirrors
/// [`session_finish_reminders`]'s own project resolution. Empty when the
/// project can't be resolved (degrades silently — hooks must never error).
fn tasks_for_current_project(state_dir: &Path, tasks: Vec<Task>) -> Vec<Task> {
    let project = match project::current_tag() {
        Ok(project) => project,
        Err(e) => {
            tracing::debug!("project::current_tag failed (non-fatal): {e}");
            return Vec::new();
        }
    };
    filter_tasks_for_project(state_dir, &project, tasks)
}

/// Keep only tasks whose session is tagged to `project` — any session ever
/// tagged to it, open or closed — the shared filter behind `task ls
/// --current-project` (#1117) and the SessionStart/Stop hook reminders
/// ([`tasks_for_current_project`]). Legacy tasks with no `session` (predate
/// mandatory sessions) are dropped: they can't be attributed to any
/// project, and the conservative default is to never surface a task we
/// can't attribute.
#[must_use]
pub fn filter_tasks_for_project(state_dir: &Path, project: &str, tasks: Vec<Task>) -> Vec<Task> {
    let session_ids = session::session_ids_for_project(state_dir, project);
    tasks
        .into_iter()
        .filter(|t| {
            t.session
                .as_deref()
                .is_some_and(|sid| session_ids.contains(sid))
        })
        .collect()
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
/// task it blocks). `None` if `current` isn't found in `session_tasks`, or
/// nothing after it qualifies.
#[must_use]
pub fn resolve_next_task(
    all_tasks: &[Task],
    session_tasks: &[Task],
    current: &Task,
) -> Option<Task> {
    let order = execution_order(session_tasks);
    let pos = order.iter().position(|t| t.slug == current.slug)?;
    let by_slug: HashMap<&str, &Task> = all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    order
        .into_iter()
        .skip(pos + 1)
        .find(|t| is_actionable(t, &by_slug))
}

/// True when `task` is neither `done` nor `waiting`, and every one of its
/// `blocked_on` refs is (recursively) done per [`is_done_including_descendants`].
/// A dangling or not-yet-done blocker keeps the task non-actionable — fail
/// closed rather than treat an unresolvable reference as satisfied.
fn is_actionable(task: &Task, by_slug: &HashMap<&str, &Task>) -> bool {
    !matches!(task.state, TaskState::Done | TaskState::Waiting)
        && task
            .blocked_on
            .iter()
            .all(|b| is_done_including_descendants(b, by_slug))
}

/// True when the task named `slug` is `done` and every task transitively
/// parented under it (its children, their children, …) is also `done` —
/// the resolution `blocked_on` uses (#1164), so blocking on a parent task
/// alone covers its whole child set without a block edge per sibling.
///
/// A dangling `slug` (no task in `by_slug`) resolves to `false` — fail
/// closed, same rationale as the old non-recursive check this replaces. A
/// malformed parent cycle terminates via the `visited` guard (mirroring
/// [`append_forest`]'s guard) instead of recursing forever; a cycle is
/// invalid data, not a case this needs to resolve "correctly" for, only
/// safely.
fn is_done_including_descendants(slug: &str, by_slug: &HashMap<&str, &Task>) -> bool {
    fn go<'a>(
        slug: &'a str,
        by_slug: &HashMap<&'a str, &'a Task>,
        visited: &mut HashSet<&'a str>,
    ) -> bool {
        if !visited.insert(slug) {
            return true;
        }
        let Some(&task) = by_slug.get(slug) else {
            return false;
        };
        task.state == TaskState::Done
            && by_slug
                .values()
                .filter(|t| t.parent.as_deref() == Some(slug))
                .all(|child| go(&child.slug, by_slug, visited))
    }
    go(slug, by_slug, &mut HashSet::new())
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

/// For every session open for the current project (resolved from the
/// process's actual cwd — hooks run with cwd set to the project directory)
/// whose tasks are all done, build a reminder nudging the agent to close it
/// out. Empty string if none qualify, or on any internal error (degrades
/// silently — hooks must never block the agent).
///
/// Like [`wip_reminder`], this can't tell whether a fully-done session
/// belongs to the current conversation or a different, concurrently running
/// one (#1028) — closing out someone else's session is a real mutation of
/// their bookkeeping, so the nudge is conditioned on recognizing the session
/// rather than issued as a bare command.
fn session_finish_reminders(state_dir: &Path) -> String {
    let project = match project::current_tag() {
        Ok(project) => project,
        Err(e) => {
            tracing::debug!("project::current_tag failed (non-fatal): {e}");
            return String::new();
        }
    };
    let mut lines = Vec::new();
    for session in session::open_sessions_for_project(state_dir, &project) {
        let (done, total) = session::session_progress(state_dir, &session.id);
        if total == 0 || done < total {
            continue;
        }
        let label = session.name.as_deref().unwrap_or(session.id.as_str());
        lines.push(format!(
            "All {total} task(s) in session '{label}' ({id}) are done. If you recognize this \
             as your own session, run `llmenv task session finish {id}` to close it out, or \
             `llmenv task add <title> --session {id}` to add more work to it. If you don't \
             recognize it, it belongs to a different session — leave it alone.",
            id = session.id
        ));
    }
    lines.join("\n\n")
}

/// Join non-empty reminder strings with a blank line between them; empty
/// parts are dropped rather than leaving stray blank lines.
fn combine_reminders(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
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
            format!("{} [session: {session}]", task_line(t))
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
mod tests {
    use super::session::{StartDecision, StartOutcome, open_sessions_for_project, start_session};
    use super::*;
    use tempfile::TempDir;

    const PROJECT: &str = "test-project-0000000000";

    /// Create a task tagged to a fixed test session id, exercising the same
    /// creation logic (`add_task_for_session`) the mandatory-session path
    /// resolves down to — lets the store-behavior tests below (slug/parent/
    /// nesting) skip the session-resolution dance, which has its own tests.
    /// `None` maps to `ParentSpec::Detached`, not `Auto` — existing callers
    /// pass `None` meaning "no parent" (independent tasks), predating #929's
    /// implicit-chain default; a handful of dedicated tests exercise `Auto`
    /// directly instead of through this helper.
    fn mk(dir: &Path, title: &str, parent: Option<&str>) -> anyhow::Result<Task> {
        let parent_spec = match parent {
            Some(p) => ParentSpec::Explicit(p),
            None => ParentSpec::Detached,
        };
        add_task_for_session(dir, title, parent_spec, "test-session")
    }

    #[test]
    fn try_list_tasks_missing_store_is_empty_not_error() {
        let dir = TempDir::new().unwrap();
        assert_eq!(try_list_tasks(dir.path()).unwrap(), Vec::new());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_store_dirs_and_lock_are_owner_only_from_creation() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        start_session(
            dir.path(),
            None,
            None,
            &current_project(),
            StartDecision::Auto,
        )
        .expect("test");
        mk(dir.path(), "seed task", None).expect("test");

        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_of(&tasks_dir(dir.path())),
            0o700,
            "tasks/ must be owner-only from creation"
        );
        assert_eq!(
            mode_of(&tasks_dir(dir.path()).join("sessions")),
            0o700,
            "tasks/sessions/ must be owner-only from creation"
        );
        assert_eq!(
            mode_of(&tasks_dir(dir.path()).join(".lock")),
            0o600,
            "the store lock file must be owner-only from creation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn try_list_tasks_unreadable_store_errors_instead_of_empty() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let tasks = tasks_dir(dir.path());
        std::fs::create_dir_all(&tasks).unwrap();
        mk(dir.path(), "before making unreadable", None).unwrap();
        std::fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o000)).unwrap();

        let readable_anyway = std::fs::read_dir(&tasks).is_ok();
        let result = try_list_tasks(dir.path());
        // The tolerant wrapper must degrade to empty (not panic/propagate) while
        // the store is still unreadable — check before restoring permissions.
        let tolerant_result = list_tasks(dir.path());

        std::fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "an unreadable tasks dir must be a genuine error, not an empty Vec: {result:?}"
        );
        assert_eq!(tolerant_result, Vec::new());
    }

    #[cfg(unix)]
    #[test]
    fn try_list_tasks_skips_both_corrupt_and_unreadable_individual_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let good = mk(dir.path(), "a real task", None).unwrap();
        let tasks = tasks_dir(dir.path());
        std::fs::write(tasks.join("corrupt.json"), b"not json").unwrap();
        let unreadable_file = tasks.join("unreadable.json");
        std::fs::write(&unreadable_file, b"{}").unwrap();
        std::fs::set_permissions(&unreadable_file, std::fs::Permissions::from_mode(0o000)).unwrap();

        let readable_anyway = std::fs::read_dir(&unreadable_file).is_ok()
            || std::fs::read_to_string(&unreadable_file).is_ok();
        let result = try_list_tasks(dir.path());

        std::fs::set_permissions(&unreadable_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        let tasks = result.expect("per-file errors must not fail the whole listing");
        assert_eq!(
            tasks.iter().map(|t| &t.slug).collect::<Vec<_>>(),
            vec![&good.slug],
            "corrupt and unreadable files are both skipped, the real task is kept"
        );
    }

    /// The real project tag for this test process's cwd (#949 reminder
    /// scoping resolves it via [`project::current_tag`], which reads actual
    /// cwd/`$HOME` — there's no injection point, so tests that need a task
    /// attributed to "the current project" must resolve the same real tag
    /// the code under test will use).
    fn current_project() -> String {
        project::current_tag().expect("test")
    }

    /// Create a fresh, real session tagged to `project` (unlike `mk`'s fixed
    /// "test-session" id, this session actually exists in the store — needed
    /// for the #949 project-scoping tests, which filter against real session
    /// records).
    fn session_for_project(dir: &Path, project: &str) -> String {
        let outcome = start_session(dir, None, None, project, StartDecision::New).expect("test");
        let StartOutcome::Created(session) = outcome else {
            panic!("expected Created");
        };
        session.id
    }

    /// Create a `wip` task tagged to a real session in `project`.
    fn wip_task_in_project(dir: &Path, title: &str, project: &str) -> Task {
        let session_id = session_for_project(dir, project);
        let task =
            add_task_for_session(dir, title, ParentSpec::Detached, &session_id).expect("test");
        start_task(dir, &task.slug, false).expect("test")
    }

    /// Create a `waiting` task tagged to a real session in `project`.
    fn waiting_task_in_project(dir: &Path, title: &str, project: &str, reason: &str) -> Task {
        let session_id = session_for_project(dir, project);
        let task =
            add_task_for_session(dir, title, ParentSpec::Detached, &session_id).expect("test");
        start_task(dir, &task.slug, false).expect("test");
        wait_task(dir, &task.slug, reason).expect("test")
    }

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
        let task = mk(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(task.slug, "fix-login-timeout");
        assert_eq!(task.state, TaskState::Open);
        assert!(task.parent.is_none());

        let loaded = load_task(dir.path(), "fix-login-timeout").expect("test");
        assert_eq!(loaded, task);
    }

    #[test]
    fn add_task_uniquifies_slug_on_collision() {
        let dir = TempDir::new().expect("test");
        let t1 = mk(dir.path(), "Fix login timeout", None).expect("test");
        let t2 = mk(dir.path(), "Fix login timeout", None).expect("test");
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
                    mk(&dir_path, "Race condition task", None).expect("test")
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
        let task = mk(dir.path(), "Shared task", None).expect("test");
        let dir_path = dir.path().to_path_buf();
        let slug = task.slug.clone();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir_path = dir_path.clone();
                let slug = slug.clone();
                std::thread::spawn(move || start_task(&dir_path, &slug, false))
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
        let parent = mk(dir.path(), "Parent task", None).expect("test");
        let child = mk(dir.path(), "Child task", Some(&parent.slug)).expect("test");
        assert_eq!(child.parent, Some(parent.slug));
    }

    #[test]
    fn add_task_with_unknown_parent_errors() {
        let dir = TempDir::new().expect("test");
        assert!(mk(dir.path(), "Orphan", Some("no-such-parent")).is_err());
    }

    #[test]
    fn add_task_parent_accepts_unambiguous_prefix() {
        let dir = TempDir::new().expect("test");
        let parent = mk(dir.path(), "Umbrella project", None).expect("test");
        let child = mk(dir.path(), "Sub piece", Some("umbrella")).expect("test");
        assert_eq!(child.parent, Some(parent.slug));
    }

    #[test]
    fn nested_chain_of_three_levels_resolves_correctly() {
        let dir = TempDir::new().expect("test");
        let grandparent = mk(dir.path(), "Epic", None).expect("test");
        let parent = mk(dir.path(), "Story", Some(&grandparent.slug)).expect("test");
        let child = mk(dir.path(), "Subtask", Some(&parent.slug)).expect("test");

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
        let parent = mk(dir.path(), "Parent with many kids", None).expect("test");
        let child_a = mk(dir.path(), "Child A", Some(&parent.slug)).expect("test");
        let child_b = mk(dir.path(), "Child B", Some(&parent.slug)).expect("test");
        let child_c = mk(dir.path(), "Child C", Some(&parent.slug)).expect("test");

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
        let parent = mk(dir.path(), "Parent", None).expect("test");
        let child = mk(dir.path(), "Child", Some(&parent.slug)).expect("test");
        start_task(dir.path(), &child.slug, false).expect("test");
        done_task(dir.path(), &child.slug).expect("test");

        let reloaded_parent = load_task(dir.path(), &parent.slug).expect("test");
        assert_eq!(reloaded_parent.state, TaskState::Open);
    }

    #[test]
    fn list_tasks_skips_corrupt_files_with_warning() {
        let dir = TempDir::new().expect("test");
        mk(dir.path(), "Good task", None).expect("test");
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
        let task = mk(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(
            resolve_identifier(dir.path(), &task.slug).expect("test"),
            task.slug
        );
    }

    #[test]
    fn resolve_identifier_unambiguous_prefix() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(
            resolve_identifier(dir.path(), "fix-log").expect("test"),
            task.slug
        );
    }

    #[test]
    fn resolve_identifier_ambiguous_prefix_errors_listing_candidates() {
        let dir = TempDir::new().expect("test");
        mk(dir.path(), "Fix login timeout", None).expect("test");
        mk(dir.path(), "Fix logout crash", None).expect("test");
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
        mk(dir.path(), "Fix login timeout", None).expect("test");
        assert!(resolve_identifier(dir.path(), "nope").is_err());
    }

    #[test]
    fn start_task_transitions_open_to_wip() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        let started = start_task(dir.path(), &task.slug, false).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn start_task_on_done_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        done_task(dir.path(), &task.slug).expect("test");
        assert!(start_task(dir.path(), &task.slug, false).is_err());
    }

    #[test]
    fn done_task_from_open_allowed() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        let done = done_task(dir.path(), &task.slug).expect("test");
        assert_eq!(done.state, TaskState::Done);
    }

    // #1164: blocked_on is a hard-block (an explicit dependency the user
    // configured on purpose), unlike parent (soft-block, see cli/mod.rs's
    // Start handler). Starting a task blocked on a not-done task must error.
    #[test]
    fn start_task_errors_when_blocked_on_open_task() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker task", None).expect("test");
        let task = mk(dir.path(), "Blocked task", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        let err = start_task(dir.path(), &task.slug, false).unwrap_err();
        assert!(
            err.to_string().contains(&blocker.slug),
            "error should name the unmet blocker: {err}"
        );
    }

    #[test]
    fn start_task_force_overrides_unmet_blocker() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker task", None).expect("test");
        let task = mk(dir.path(), "Blocked task", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        let started = start_task(dir.path(), &task.slug, true).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn start_task_allows_when_blocker_is_done() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker task", None).expect("test");
        let task = mk(dir.path(), "Blocked task", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        done_task(dir.path(), &blocker.slug).expect("test");
        let started = start_task(dir.path(), &task.slug, false).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    // #1164 fan-out design: blocking on a parent task with children means
    // "blocked until the parent AND all its children are done" -- so a
    // downstream task can hard-block on the whole set via one edge to the
    // parent, without hand-wiring a block edge per sibling.
    #[test]
    fn start_task_errors_when_blocker_has_undone_child() {
        let dir = TempDir::new().expect("test");
        let parent = mk(dir.path(), "Parent step", None).expect("test");
        let child = mk(dir.path(), "Sibling task", Some(&parent.slug)).expect("test");
        done_task(dir.path(), &parent.slug).expect("test");
        let downstream = mk(dir.path(), "Downstream", None).expect("test");
        block_task(dir.path(), &downstream.slug, &parent.slug).expect("test");
        let err = start_task(dir.path(), &downstream.slug, false).unwrap_err();
        assert!(
            err.to_string().contains(&parent.slug),
            "error should name the unmet blocker: {err}"
        );
        // Confirm the setup actually holds: child is the reason the parent
        // isn't considered fully done yet.
        assert_ne!(
            load_task(dir.path(), &child.slug).expect("test").state,
            TaskState::Done
        );
    }

    #[test]
    fn start_task_allows_when_blocker_and_all_children_done() {
        let dir = TempDir::new().expect("test");
        let parent = mk(dir.path(), "Parent step", None).expect("test");
        let child = mk(dir.path(), "Sibling task", Some(&parent.slug)).expect("test");
        done_task(dir.path(), &child.slug).expect("test");
        done_task(dir.path(), &parent.slug).expect("test");
        let downstream = mk(dir.path(), "Downstream", None).expect("test");
        block_task(dir.path(), &downstream.slug, &parent.slug).expect("test");
        let started = start_task(dir.path(), &downstream.slug, false).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn note_task_appends_note() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        let updated = note_task(dir.path(), &task.slug, "made progress").expect("test");
        assert_eq!(updated.notes.len(), 1);
        assert_eq!(updated.notes[0].text, "made progress");
    }

    #[test]
    fn wait_task_transitions_to_waiting_and_notes_reason() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        start_task(dir.path(), &task.slug, false).expect("test");
        let waiting = wait_task(dir.path(), &task.slug, "need spec review").expect("test");
        assert_eq!(waiting.state, TaskState::Waiting);
        assert_eq!(waiting.notes.len(), 1);
        assert!(waiting.notes[0].text.contains("need spec review"));
    }

    #[test]
    fn wait_task_on_done_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        done_task(dir.path(), &task.slug).expect("test");
        assert!(wait_task(dir.path(), &task.slug, "too late").is_err());
    }

    #[test]
    fn start_task_resumes_from_waiting() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Do thing", None).expect("test");
        wait_task(dir.path(), &task.slug, "blocked").expect("test");
        let resumed = start_task(dir.path(), &task.slug, false).expect("test");
        assert_eq!(resumed.state, TaskState::Wip);
    }

    #[test]
    fn block_task_records_dependency() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker", None).expect("test");
        let task = mk(dir.path(), "Blocked", None).expect("test");
        let updated = block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        assert_eq!(updated.blocked_on, vec![blocker.slug]);
    }

    #[test]
    fn block_task_on_unknown_target_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Blocked", None).expect("test");
        assert!(block_task(dir.path(), &task.slug, "no-such-task").is_err());
    }

    #[test]
    fn block_task_on_self_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Solo task", None).expect("test");
        assert!(block_task(dir.path(), &task.slug, &task.slug).is_err());
    }

    #[test]
    fn edit_task_retitles() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Original title", None).expect("test");
        let edit = TaskEdit {
            title: Some("New title"),
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.title, "New title");
    }

    #[test]
    fn edit_task_sets_and_clears_parent() {
        let dir = TempDir::new().expect("test");
        let parent = mk(dir.path(), "Parent", None).expect("test");
        let task = mk(dir.path(), "Child", None).expect("test");

        let edit = TaskEdit {
            parent: Some(&parent.slug),
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.parent, Some(parent.slug.clone()));

        let edit = TaskEdit {
            no_parent: true,
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.parent, None);
    }

    #[test]
    fn edit_task_parent_to_unknown_id_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let edit = TaskEdit {
            parent: Some("no-such-task"),
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_parent_to_self_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let edit = TaskEdit {
            parent: Some(&task.slug),
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_parent_creating_a_cycle_errors() {
        let dir = TempDir::new().expect("test");
        let a = mk(dir.path(), "A", None).expect("test");
        let b = mk(dir.path(), "B", Some(&a.slug)).expect("test");
        // A -> B already; making A's parent B would make A its own ancestor.
        let edit = TaskEdit {
            parent: Some(&b.slug),
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &a.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_adds_and_removes_blocked_on() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker", None).expect("test");
        let task = mk(dir.path(), "Blocked", None).expect("test");

        let block_on = vec![blocker.slug.clone()];
        let edit = TaskEdit {
            block_on: &block_on,
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.blocked_on, vec![blocker.slug.clone()]);

        let unblock = vec![blocker.slug.clone()];
        let edit = TaskEdit {
            unblock: &unblock,
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert!(updated.blocked_on.is_empty());
    }

    #[test]
    fn edit_task_block_on_is_idempotent() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker", None).expect("test");
        let task = mk(dir.path(), "Blocked", None).expect("test");
        let block_on = vec![blocker.slug.clone(), blocker.slug.clone()];
        let edit = TaskEdit {
            block_on: &block_on,
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.blocked_on, vec![blocker.slug]);
    }

    #[test]
    fn edit_task_unblock_absent_id_is_a_noop() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let other = mk(dir.path(), "Other", None).expect("test");
        let unblock = vec![other.slug];
        let edit = TaskEdit {
            unblock: &unblock,
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_ok());
    }

    #[test]
    fn edit_task_block_on_self_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let block_on = vec![task.slug.clone()];
        let edit = TaskEdit {
            block_on: &block_on,
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_block_on_unknown_id_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let block_on = vec!["no-such-task".to_string()];
        let edit = TaskEdit {
            block_on: &block_on,
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_adds_a_note() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let edit = TaskEdit {
            add_note: Some("progress note"),
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.notes.len(), 1);
        assert_eq!(updated.notes[0].text, "progress note");
    }

    #[test]
    fn edit_task_deletes_a_note_by_index() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        note_task(dir.path(), &task.slug, "first").expect("test");
        note_task(dir.path(), &task.slug, "second").expect("test");
        let edit = TaskEdit {
            delete_note: Some("0"),
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.notes.len(), 1);
        assert_eq!(updated.notes[0].text, "second");
    }

    #[test]
    fn edit_task_deletes_a_note_by_timestamp() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let with_note = note_task(dir.path(), &task.slug, "only note").expect("test");
        let at = with_note.notes[0].at.clone();
        let edit = TaskEdit {
            delete_note: Some(&at),
            ..Default::default()
        };
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert!(updated.notes.is_empty());
    }

    #[test]
    fn edit_task_delete_note_out_of_range_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let edit = TaskEdit {
            delete_note: Some("0"),
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_delete_note_unknown_timestamp_errors() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        note_task(dir.path(), &task.slug, "only note").expect("test");
        let edit = TaskEdit {
            delete_note: Some("2020-01-01T00:00:00Z"),
            ..Default::default()
        };
        assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
    }

    #[test]
    fn edit_task_with_no_fields_is_a_noop_but_bumps_updated_at() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Task", None).expect("test");
        let original_updated_at = task.updated_at.clone();
        let edit = TaskEdit::default();
        let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
        assert_eq!(updated.title, task.title);
        assert_eq!(updated.parent, task.parent);
        assert_eq!(updated.blocked_on, task.blocked_on);
        assert_eq!(updated.notes, task.notes);
        assert_ne!(updated.updated_at, original_updated_at);
    }

    #[test]
    fn edit_task_on_unknown_task_errors() {
        let dir = TempDir::new().expect("test");
        let edit = TaskEdit::default();
        assert!(edit_task(dir.path(), "no-such-task", &edit).is_err());
    }

    #[test]
    fn add_task_with_no_alphanumeric_title_falls_back_to_timestamp_slug() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "!!!", None).expect("test");
        assert!(!task.slug.is_empty());
        assert!(task.slug.starts_with("task-"));
        let loaded = load_task(dir.path(), &task.slug).expect("test");
        assert_eq!(loaded, task);
    }

    // A dangling blocked_on reference (deleted/corrupt blocker file) fails
    // closed -- same rationale as is_actionable's dangling-blocker handling
    // for `task show --next` -- rather than silently treating it as resolved.
    #[test]
    fn start_task_errors_when_blocked_on_deleted_task() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker", None).expect("test");
        let task = mk(dir.path(), "Blocked", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        std::fs::remove_file(
            dir.path()
                .join("tasks")
                .join(format!("{}.json", blocker.slug)),
        )
        .expect("test");
        let err = start_task(dir.path(), &task.slug, false).unwrap_err();
        assert!(err.to_string().contains(&blocker.slug));
    }

    #[test]
    fn start_task_force_overrides_dangling_blocker() {
        let dir = TempDir::new().expect("test");
        let blocker = mk(dir.path(), "Blocker", None).expect("test");
        let task = mk(dir.path(), "Blocked", None).expect("test");
        block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
        std::fs::remove_file(
            dir.path()
                .join("tasks")
                .join(format!("{}.json", blocker.slug)),
        )
        .expect("test");
        let started = start_task(dir.path(), &task.slug, true).expect("test");
        assert_eq!(started.state, TaskState::Wip);
    }

    #[test]
    fn session_start_reminder_empty_when_no_wip_tasks() {
        let dir = TempDir::new().expect("test");
        mk(dir.path(), "Open task", None).expect("test");
        assert!(session_start_reminder(dir.path()).is_empty());
    }

    #[test]
    fn session_start_reminder_lists_wip_tasks() {
        let dir = TempDir::new().expect("test");
        let task = wip_task_in_project(dir.path(), "In progress task", &current_project());
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
        let task = wip_task_in_project(dir.path(), "Left in progress", &current_project());
        let reminder = stop_hook_reminder(dir.path());
        assert!(reminder.contains(&task.slug));
    }

    #[test]
    fn stop_hook_reminder_silent_when_only_waiting_tasks() {
        let dir = TempDir::new().expect("test");
        let task = mk(dir.path(), "Blocked on review", None).expect("test");
        wait_task(dir.path(), &task.slug, "spec review").expect("test");
        // `waiting` is meant to be quiet — Stop must not re-inject its FYI.
        assert!(stop_hook_reminder(dir.path()).is_empty());
    }

    #[test]
    fn stop_hook_reminder_shows_wip_but_not_waiting() {
        let dir = TempDir::new().expect("test");
        let project = current_project();
        let wip = wip_task_in_project(dir.path(), "Actively working", &project);
        let waiting =
            waiting_task_in_project(dir.path(), "Blocked on review", &project, "spec review");

        let reminder = stop_hook_reminder(dir.path());
        assert!(reminder.contains(&wip.slug));
        assert!(reminder.contains("exhaust safe autonomous remediation"));
        // The waiting task and its FYI must stay silent on Stop.
        assert!(!reminder.contains(&waiting.slug));
        assert!(!reminder.contains("no action needed"));
    }

    // --- cross-session ownership framing (#1028) ---

    #[test]
    fn stop_hook_reminder_shows_session_tag_and_never_presumes_ownership() {
        // The bug (#1028): a WIP task belonging to a *different*, concurrently
        // running session must never be framed as a command to resume it —
        // the reminder can't tell whose session started this conversation, so
        // it must name the owning session and let the agent judge for itself
        // rather than asserting "you have tasks in progress."
        let dir = TempDir::new().expect("test");
        let task = wip_task_in_project(dir.path(), "Someone's task", &current_project());

        let reminder = stop_hook_reminder(dir.path());
        assert!(
            reminder.contains(task.session.as_deref().expect("test")),
            "reminder must name the owning session so the agent can tell whether it's its \
             own: {reminder}"
        );
        assert!(
            reminder.contains("recognize"),
            "must condition continuing the task on the agent recognizing it as its own: \
             {reminder}"
        );
    }

    #[test]
    fn session_start_reminder_shows_session_tag_and_never_presumes_ownership() {
        let dir = TempDir::new().expect("test");
        let task = wip_task_in_project(dir.path(), "Someone's task", &current_project());

        let reminder = session_start_reminder(dir.path());
        assert!(
            reminder.contains(task.session.as_deref().expect("test")),
            "reminder must name the owning session: {reminder}"
        );
        assert!(
            reminder.contains("recognize"),
            "must condition resuming the task on the agent recognizing it as its own: \
             {reminder}"
        );
    }

    #[test]
    fn session_finish_reminder_never_presumes_ownership_of_a_done_session() {
        // Variant of #1028: a fully-done session belonging to a different,
        // concurrently running conversation must not be framed as a bare
        // command to close it out — closing someone else's session mutates
        // their bookkeeping.
        let dir = TempDir::new().expect("test");
        let task = wip_task_in_project(dir.path(), "Someone's task", &current_project());
        done_task(dir.path(), &task.slug).expect("test");

        let reminder = session_start_reminder(dir.path());
        assert!(
            reminder.contains(task.session.as_deref().expect("test")),
            "reminder must name the owning session: {reminder}"
        );
        assert!(
            reminder.contains("recognize"),
            "must condition closing the session on the agent recognizing it as its own: \
             {reminder}"
        );
    }

    #[test]
    fn session_start_reminder_lists_waiting_tasks() {
        let dir = TempDir::new().expect("test");
        let task = waiting_task_in_project(
            dir.path(),
            "Blocked on review",
            &current_project(),
            "spec review",
        );
        let reminder = session_start_reminder(dir.path());
        // At session start (resume/wake) the waiting FYI is useful, unlike Stop.
        assert!(reminder.contains(&task.slug));
        assert!(reminder.contains("no action needed"));
        // Still a plain FYI, never the action-pushing wip footer.
        assert!(!reminder.contains("exhaust safe autonomous remediation"));
    }

    #[test]
    fn session_start_reminder_shows_both_wip_and_waiting() {
        let dir = TempDir::new().expect("test");
        let project = current_project();
        let wip = wip_task_in_project(dir.path(), "Actively working", &project);
        let waiting =
            waiting_task_in_project(dir.path(), "Blocked on review", &project, "spec review");

        let reminder = session_start_reminder(dir.path());
        assert!(reminder.contains(&wip.slug));
        assert!(reminder.contains(&waiting.slug));
        assert!(reminder.contains("no action needed"));
    }

    // --- reminder project scoping (#949) ---

    #[test]
    fn stop_hook_reminder_does_not_leak_wip_task_from_other_project() {
        let dir = TempDir::new().expect("test");
        let other_project = "other-project-9999999999";
        let leaked = wip_task_in_project(dir.path(), "Other project's task", other_project);
        // No session/task tagged to the real current project at all — a
        // `wip` task belonging to a different project must not surface.
        let reminder = stop_hook_reminder(dir.path());
        assert!(!reminder.contains(&leaked.slug));
        assert!(reminder.is_empty());
    }

    #[test]
    fn session_start_reminder_does_not_leak_waiting_task_from_other_project() {
        let dir = TempDir::new().expect("test");
        let other_project = "other-project-9999999999";
        let leaked =
            waiting_task_in_project(dir.path(), "Other project's task", other_project, "blocked");
        let reminder = session_start_reminder(dir.path());
        assert!(!reminder.contains(&leaked.slug));
        assert!(reminder.is_empty());
    }

    #[test]
    fn stop_hook_reminder_shows_only_current_projects_wip_task() {
        let dir = TempDir::new().expect("test");
        let project = current_project();
        let mine = wip_task_in_project(dir.path(), "My task", &project);
        let leaked = wip_task_in_project(dir.path(), "Other project's task", "other-project-999");

        let reminder = stop_hook_reminder(dir.path());
        assert!(reminder.contains(&mine.slug), "own-project task must show");
        assert!(
            !reminder.contains(&leaked.slug),
            "other-project task must not leak in"
        );
    }

    #[test]
    fn legacy_task_with_no_session_is_not_surfaced_cross_project() {
        let dir = TempDir::new().expect("test");
        // Predates mandatory sessions (`#[serde(default)]` on `Task::session`
        // keeps such a file loadable) — no session to attribute it to any
        // project, so it must stay silent rather than surface everywhere.
        let mut legacy = mk(dir.path(), "Legacy wip task", None).expect("test");
        legacy.session = None;
        legacy.state = TaskState::Wip;
        save_task(dir.path(), &legacy).expect("test");

        assert!(!stop_hook_reminder(dir.path()).contains(&legacy.slug));
        assert!(!session_start_reminder(dir.path()).contains(&legacy.slug));
    }

    // --- task ls ordering + filtering (#926) ---

    fn t(slug: &str, state: TaskState, parent: Option<&str>, session: Option<&str>) -> Task {
        Task {
            slug: slug.to_string(),
            title: format!("Title for {slug}"),
            state,
            parent: parent.map(str::to_string),
            blocked_on: Vec::new(),
            notes: Vec::new(),
            session: session.map(str::to_string),
            // Identical timestamps on purpose: exercises the slug tiebreak.
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn slugs(rows: &[DisplayRow]) -> Vec<(usize, String)> {
        rows.iter()
            .map(|r| (r.depth, r.task.slug.clone()))
            .collect()
    }

    #[test]
    fn filter_by_state_empty_keeps_all() {
        let tasks = vec![
            t("a", TaskState::Open, None, None),
            t("b", TaskState::Done, None, None),
        ];
        assert_eq!(filter_by_state(tasks, &[]).len(), 2);
    }

    #[test]
    fn filter_by_state_keeps_only_listed_states() {
        let tasks = vec![
            t("a", TaskState::Open, None, None),
            t("b", TaskState::Wip, None, None),
            t("c", TaskState::Waiting, None, None),
            t("d", TaskState::Done, None, None),
        ];
        let kept = filter_by_state(tasks, &[TaskState::Wip, TaskState::Waiting]);
        assert_eq!(
            kept.iter().map(|x| x.slug.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    // --- task ls --current-project / task show --current/--next (#1117) ---

    /// Like `t`, but with an explicit `updated_at` — needed to exercise
    /// [`resolve_current_task`]'s "most recently updated" tiebreak, which
    /// `t`'s fixed timestamp can't.
    fn t_updated(slug: &str, state: TaskState, session: Option<&str>, updated_at: &str) -> Task {
        Task {
            updated_at: updated_at.to_string(),
            ..t(slug, state, None, session)
        }
    }

    #[test]
    fn filter_tasks_for_project_keeps_only_tasks_in_that_projects_sessions() {
        let dir = TempDir::new().expect("test");
        let session_id = session_for_project(dir.path(), "proj-a");
        let in_project =
            add_task_for_session(dir.path(), "In proj-a", ParentSpec::Detached, &session_id)
                .expect("test");
        let other_session = t("other", TaskState::Open, None, Some("other-session"));
        let no_session = t("legacy", TaskState::Open, None, None);
        let tasks = vec![in_project.clone(), other_session, no_session];

        let kept = filter_tasks_for_project(dir.path(), "proj-a", tasks);
        assert_eq!(
            kept.iter().map(|x| x.slug.as_str()).collect::<Vec<_>>(),
            [in_project.slug.as_str()]
        );
    }

    #[test]
    fn resolve_current_task_prefers_wip_over_open() {
        let tasks = vec![
            t_updated("open", TaskState::Open, Some("s"), "2026-01-01T00:00:02Z"),
            t_updated("wip", TaskState::Wip, Some("s"), "2026-01-01T00:00:01Z"),
        ];
        let current = resolve_current_task(&tasks).expect("test");
        assert_eq!(current.slug, "wip");
    }

    #[test]
    fn resolve_current_task_falls_back_to_most_recently_updated_non_done_task() {
        let tasks = vec![
            t_updated("older", TaskState::Open, Some("s"), "2026-01-01T00:00:01Z"),
            t_updated(
                "newer",
                TaskState::Waiting,
                Some("s"),
                "2026-01-01T00:00:02Z",
            ),
            t_updated("done", TaskState::Done, Some("s"), "2026-01-01T00:00:03Z"),
        ];
        let current = resolve_current_task(&tasks).expect("test");
        assert_eq!(current.slug, "newer");
    }

    #[test]
    fn resolve_current_task_none_when_everything_is_done() {
        let tasks = vec![t("done", TaskState::Done, None, Some("s"))];
        assert!(resolve_current_task(&tasks).is_none());
    }

    #[test]
    fn resolve_next_task_skips_done_and_blocked_tasks() {
        let current = t("a", TaskState::Wip, None, Some("s"));
        let mut blocked = t("b", TaskState::Open, None, Some("s"));
        blocked.blocked_on = vec!["a".to_string()]; // "a" (current) isn't done yet
        let done = t("c", TaskState::Done, None, Some("s"));
        let actionable = t("d", TaskState::Open, None, Some("s"));
        let session_tasks = vec![current.clone(), blocked, done, actionable.clone()];

        let next = resolve_next_task(&session_tasks, &session_tasks, &current).expect("test");
        assert_eq!(next.slug, "d");
    }

    #[test]
    fn resolve_next_task_resolves_blockers_against_all_tasks_not_just_the_session() {
        let current = t("a", TaskState::Wip, None, Some("s"));
        // Blocker lives in a different session than the blocked task, and
        // isn't done yet — "b" stays non-actionable.
        let mut blocked = t("b", TaskState::Open, None, Some("s"));
        blocked.blocked_on = vec!["elsewhere".to_string()];
        let blocker = t("elsewhere", TaskState::Open, None, Some("other-session"));
        let session_tasks = vec![current.clone(), blocked];
        let all_tasks = vec![current.clone(), session_tasks[1].clone(), blocker];

        assert!(resolve_next_task(&all_tasks, &session_tasks, &current).is_none());
    }

    #[test]
    fn resolve_next_task_returns_a_task_once_its_cross_session_blocker_is_done() {
        // Same shape as the test above, but the blocker is `done` — pins the
        // `all_tasks`/`session_tasks` argument order: swapping them would
        // make the blocker unresolvable (not present in `session_tasks`) and
        // this would wrongly return `None` too.
        let current = t("a", TaskState::Wip, None, Some("s"));
        let mut blocked = t("b", TaskState::Open, None, Some("s"));
        blocked.blocked_on = vec!["elsewhere".to_string()];
        let blocker = t("elsewhere", TaskState::Done, None, Some("other-session"));
        let session_tasks = vec![current.clone(), blocked.clone()];
        let all_tasks = vec![current.clone(), blocked, blocker];

        let next = resolve_next_task(&all_tasks, &session_tasks, &current).expect("test");
        assert_eq!(next.slug, "b");
    }

    #[test]
    fn resolve_next_task_skips_waiting_tasks() {
        // `waiting` is paused on something outside the agent's control, not
        // a legitimate "next" step — distinct from `resolve_current_task`'s
        // fallback, which does treat `waiting` as a valid "in progress" hit.
        let current = t("a", TaskState::Wip, None, Some("s"));
        let waiting = t("b", TaskState::Waiting, None, Some("s"));
        let actionable = t("c", TaskState::Open, None, Some("s"));
        let session_tasks = vec![current.clone(), waiting, actionable];

        let next = resolve_next_task(&session_tasks, &session_tasks, &current).expect("test");
        assert_eq!(next.slug, "c");
    }

    #[test]
    fn resolve_next_task_prefers_child_over_next_sibling() {
        let parent = t("p", TaskState::Wip, None, Some("s"));
        let child = t("c", TaskState::Open, Some("p"), Some("s"));
        let sibling = t("sib", TaskState::Open, None, Some("s"));
        let session_tasks = vec![parent.clone(), child, sibling];

        let next = resolve_next_task(&session_tasks, &session_tasks, &parent).expect("test");
        assert_eq!(
            next.slug, "c",
            "next after a parent must be its child, not a sibling"
        );
    }

    #[test]
    fn resolve_next_task_none_when_current_not_found() {
        let stray = t("stray", TaskState::Wip, None, Some("s"));
        let session_tasks = vec![t("a", TaskState::Open, None, Some("s"))];
        assert!(resolve_next_task(&session_tasks, &session_tasks, &stray).is_none());
    }

    #[test]
    fn resolve_next_task_none_when_current_is_last() {
        let current = t("a", TaskState::Wip, None, Some("s"));
        let session_tasks = vec![current.clone()];
        assert!(resolve_next_task(&session_tasks, &session_tasks, &current).is_none());
    }

    #[test]
    fn display_rows_indents_subtasks_under_parent_in_creation_order() {
        // Deliberately unsorted input; slug tiebreak must produce a stable order.
        let tasks = vec![
            t("deploy", TaskState::Open, None, Some("s")),
            t("child-b", TaskState::Open, Some("api"), Some("s")),
            t("api", TaskState::Wip, None, Some("s")),
            t("child-a", TaskState::Done, Some("api"), Some("s")),
        ];
        let rows = display_rows(tasks, &["s".to_string()]);
        assert_eq!(
            slugs(&rows),
            vec![
                (0, "api".to_string()),
                (1, "child-a".to_string()),
                (1, "child-b".to_string()),
                (0, "deploy".to_string()),
            ]
        );
    }

    #[test]
    fn display_rows_orders_priority_sessions_first_then_others_then_none() {
        let tasks = vec![
            t("z", TaskState::Open, None, None),
            t("other", TaskState::Open, None, Some("zzz")),
            t("cur", TaskState::Open, None, Some("current")),
        ];
        let rows = display_rows(tasks, &["current".to_string()]);
        assert_eq!(
            rows.iter().map(|r| r.task.slug.clone()).collect::<Vec<_>>(),
            ["cur", "other", "z"]
        );
    }

    #[test]
    fn display_rows_keeps_unlisted_session_tasks() {
        // A session absent from the priority list must still get a group.
        let tasks = vec![t("only", TaskState::Open, None, Some("ghost"))];
        let rows = display_rows(tasks, &[]);
        assert_eq!(slugs(&rows), vec![(0, "only".to_string())]);
    }

    #[test]
    fn display_rows_orphan_parent_reference_renders_child_as_root() {
        // Parent not present in the group -> child is a depth-0 root, not lost.
        let tasks = vec![t("child", TaskState::Open, Some("missing"), Some("s"))];
        let rows = display_rows(tasks, &["s".to_string()]);
        assert_eq!(slugs(&rows), vec![(0, "child".to_string())]);
    }

    #[test]
    fn display_rows_survives_parent_cycle_without_infinite_recursion() {
        // Malformed data: a <-> b cite each other as parent. Must terminate.
        let mut a = t("a", TaskState::Open, Some("b"), Some("s"));
        let mut b = t("b", TaskState::Open, Some("a"), Some("s"));
        a.created_at = "2026-01-01T00:00:01Z".to_string();
        b.created_at = "2026-01-01T00:00:02Z".to_string();
        let rows = display_rows(vec![a, b], &["s".to_string()]);
        // Neither is a root (each cites the other), but the completeness guard
        // still renders both exactly once instead of dropping them or looping.
        let mut got: Vec<String> = rows.iter().map(|r| r.task.slug.clone()).collect();
        got.sort();
        assert_eq!(got, ["a", "b"]);
    }

    // --- mandatory-session wiring (2026-07-21 rework) ---

    fn created(outcome: StartOutcome) -> super::session::Session {
        match outcome {
            StartOutcome::Created(s) => s,
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[test]
    fn add_task_with_explicit_session_tags_it() {
        let dir = TempDir::new().expect("test");
        let session = created(
            start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test"),
        );
        let task = add_task(
            dir.path(),
            "Do thing",
            ParentSpec::Detached,
            Some(&session.id),
            PROJECT,
        )
        .expect("test");
        assert_eq!(task.session, Some(session.id));
    }

    #[test]
    fn add_task_explicit_session_rejects_unknown_id() {
        let dir = TempDir::new().expect("test");
        assert!(
            add_task(
                dir.path(),
                "Do thing",
                ParentSpec::Detached,
                Some("no-such-session"),
                PROJECT
            )
            .is_err()
        );
    }

    #[test]
    fn add_task_auto_resolves_when_exactly_one_open_session_for_project() {
        let dir = TempDir::new().expect("test");
        let session = created(
            start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test"),
        );
        let task =
            add_task(dir.path(), "Do thing", ParentSpec::Detached, None, PROJECT).expect("test");
        assert_eq!(task.session, Some(session.id));
    }

    #[test]
    fn add_task_errors_with_zero_open_sessions_for_project() {
        let dir = TempDir::new().expect("test");
        let err =
            add_task(dir.path(), "Do thing", ParentSpec::Detached, None, PROJECT).unwrap_err();
        assert!(err.to_string().contains("session start"));
    }

    #[test]
    fn add_task_errors_with_two_open_sessions_for_project() {
        let dir = TempDir::new().expect("test");
        start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT,
            StartDecision::Auto,
        )
        .expect("test");
        start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT,
            StartDecision::New,
        )
        .expect("test");
        let err =
            add_task(dir.path(), "Do thing", ParentSpec::Detached, None, PROJECT).unwrap_err();
        assert!(err.to_string().contains("--session"));
    }

    #[test]
    fn add_task_does_not_auto_resolve_a_different_projects_session() {
        let dir = TempDir::new().expect("test");
        start_session(
            dir.path(),
            Some("s"),
            None,
            "other-project-1111111111",
            StartDecision::Auto,
        )
        .expect("test");
        assert!(add_task(dir.path(), "Do thing", ParentSpec::Detached, None, PROJECT).is_err());
    }

    #[test]
    fn start_task_touches_its_sessions_last_activity() {
        let dir = TempDir::new().expect("test");
        let session = created(
            start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test"),
        );
        let original = session.last_activity.clone();
        let task = add_task(
            dir.path(),
            "Do thing",
            ParentSpec::Detached,
            Some(&session.id),
            PROJECT,
        )
        .expect("test");
        start_task(dir.path(), &task.slug, false).expect("test");
        let reloaded = open_sessions_for_project(dir.path(), PROJECT)
            .into_iter()
            .find(|s| s.id == session.id)
            .expect("test");
        assert_ne!(reloaded.last_activity, original);
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

        /// Like [`arb_task`], but with the slug restricted to characters that
        /// survive a filesystem round-trip on every platform (no NUL, no path
        /// separators, no lone surrogates/unpaired combining marks that trip
        /// APFS's Unicode normalization). `arb_task`'s fully-arbitrary slug is
        /// fine for the in-memory JSON round-trip, but real `Task`s only ever
        /// get a slug via [`slugify`] (lowercase alnum + hyphen) — this
        /// generator matches that realistic shape for the file-backed
        /// [`save_task`]/[`load_task`] round-trip (#1283).
        fn arb_task_with_fs_safe_slug() -> impl Strategy<Value = Task> {
            ("[A-Za-z0-9_-]{1,40}", arb_task()).prop_map(|(slug, mut task)| {
                task.slug = slug;
                task
            })
        }

        /// A single-session task forest with unique slugs `t0..tn` where each
        /// task's parent (if any) is an earlier index — guaranteeing an acyclic
        /// forest so the depth/parent-order invariants are well-defined.
        fn arb_forest() -> impl Strategy<Value = Vec<Task>> {
            proptest::collection::vec((arb_task_state(), proptest::option::of(0usize..7)), 1..8)
                .prop_map(|specs| {
                    specs
                        .into_iter()
                        .enumerate()
                        .map(|(i, (state, parent_pick))| {
                            let parent = parent_pick.filter(|&p| p < i).map(|p| format!("t{p}"));
                            Task {
                                slug: format!("t{i}"),
                                title: format!("task {i}"),
                                state,
                                parent,
                                blocked_on: Vec::new(),
                                notes: Vec::new(),
                                session: Some("s".to_string()),
                                created_at: format!("2026-01-01T00:00:{:02}Z", i.min(59)),
                                updated_at: "2026-01-01T00:00:00Z".to_string(),
                            }
                        })
                        .collect()
                })
        }

        /// Like [`arb_forest`], but each task also carries 0..2 `blocked_on`
        /// refs to earlier-or-equal indices (including itself, exercising
        /// [`is_done_including_descendants`]'s cycle guard) — the shape
        /// `resolve_next_task`'s blocker-resolution invariants need (#1121).
        fn arb_forest_with_blockers() -> impl Strategy<Value = Vec<Task>> {
            proptest::collection::vec(
                (
                    arb_task_state(),
                    proptest::option::of(0usize..7),
                    proptest::collection::vec(0usize..7, 0..2),
                ),
                1..8,
            )
            .prop_map(|specs| {
                let n = specs.len();
                specs
                    .into_iter()
                    .enumerate()
                    .map(|(i, (state, parent_pick, blocker_picks))| {
                        let parent = parent_pick.filter(|&p| p < i).map(|p| format!("t{p}"));
                        let blocked_on = blocker_picks
                            .into_iter()
                            .filter(|&b| b < n)
                            .map(|b| format!("t{b}"))
                            .collect();
                        Task {
                            slug: format!("t{i}"),
                            title: format!("task {i}"),
                            state,
                            parent,
                            blocked_on,
                            notes: Vec::new(),
                            session: Some("s".to_string()),
                            created_at: format!("2026-01-01T00:00:{:02}Z", i.min(59)),
                            updated_at: "2026-01-01T00:00:00Z".to_string(),
                        }
                    })
                    .collect()
            })
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
            fn save_task_load_task_file_roundtrips(task in arb_task_with_fs_safe_slug()) {
                let dir = tempfile::TempDir::new().unwrap();
                save_task(dir.path(), &task).unwrap();
                let loaded = load_task(dir.path(), &task.slug).unwrap();
                prop_assert_eq!(loaded, task);
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
                    let task = mk(dir.path(), title, None).unwrap();
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
                    let task = mk(dir.path(), title, prev_slug.as_deref()).unwrap();
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

            #[test]
            fn display_rows_is_complete_and_indents_children(tasks in arb_forest()) {
                let input: std::collections::BTreeSet<String> =
                    tasks.iter().map(|t| t.slug.clone()).collect();
                let rows = display_rows(tasks.clone(), &["s".to_string()]);
                // Completeness: every input task appears exactly once.
                prop_assert_eq!(rows.len(), tasks.len());
                let output: std::collections::BTreeSet<String> =
                    rows.iter().map(|r| r.task.slug.clone()).collect();
                prop_assert_eq!(output, input);
                // Depth invariant + parent-before-child ordering.
                let depth: std::collections::HashMap<String, usize> =
                    rows.iter().map(|r| (r.task.slug.clone(), r.depth)).collect();
                let pos: std::collections::HashMap<String, usize> = rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (r.task.slug.clone(), i))
                    .collect();
                for r in &rows {
                    match &r.task.parent {
                        Some(p) => {
                            prop_assert_eq!(r.depth, depth[p] + 1);
                            prop_assert!(pos[p] < pos[&r.task.slug]);
                        }
                        None => prop_assert_eq!(r.depth, 0),
                    }
                }
            }

            #[test]
            fn filter_by_state_keeps_exactly_matching_states(
                tasks in proptest::collection::vec(arb_task(), 0..12),
                states in proptest::collection::hash_set(arb_task_state(), 0..4),
            ) {
                let want: Vec<TaskState> = states.iter().copied().collect();
                let kept = filter_by_state(tasks.clone(), &want);
                if want.is_empty() {
                    prop_assert_eq!(kept.len(), tasks.len());
                } else {
                    prop_assert!(kept.iter().all(|t| want.contains(&t.state)));
                    let expected = tasks.iter().filter(|t| want.contains(&t.state)).count();
                    prop_assert_eq!(kept.len(), expected);
                }
            }

            #[test]
            fn resolve_next_task_never_returns_done_or_waiting(
                tasks in arb_forest(),
                idx in 0usize..8,
            ) {
                let current = tasks[idx % tasks.len()].clone();
                if let Some(next) = resolve_next_task(&tasks, &tasks, &current) {
                    prop_assert_ne!(next.state, TaskState::Done);
                    prop_assert_ne!(next.state, TaskState::Waiting);
                }
            }

            // -- #1121: filter_tasks_for_project --

            #[test]
            fn filter_tasks_for_project_membership_subset_and_drops_sessionless(
                n_a in 0usize..4,
                n_b in 0usize..4,
                task_kinds in proptest::collection::vec(0usize..3, 0..10),
            ) {
                let dir = TempDir::new().unwrap();

                let mut a_ids = Vec::new();
                for _ in 0..n_a {
                    let StartOutcome::Created(s) =
                        start_session(dir.path(), None, None, "proj-a", StartDecision::New).unwrap()
                    else {
                        panic!("StartDecision::New always creates");
                    };
                    a_ids.push(s.id);
                }
                let mut b_ids = Vec::new();
                for _ in 0..n_b {
                    let StartOutcome::Created(s) =
                        start_session(dir.path(), None, None, "proj-b", StartDecision::New).unwrap()
                    else {
                        panic!("StartDecision::New always creates");
                    };
                    b_ids.push(s.id);
                }

                let tasks: Vec<Task> = task_kinds
                    .iter()
                    .enumerate()
                    .map(|(i, &kind)| {
                        let session = match kind {
                            1 if !a_ids.is_empty() => Some(a_ids[i % a_ids.len()].clone()),
                            2 if !b_ids.is_empty() => Some(b_ids[i % b_ids.len()].clone()),
                            _ => None,
                        };
                        Task {
                            slug: format!("t{i}"),
                            title: format!("task {i}"),
                            state: TaskState::Open,
                            parent: None,
                            blocked_on: Vec::new(),
                            notes: Vec::new(),
                            session,
                            created_at: "2026-01-01T00:00:00Z".to_string(),
                            updated_at: "2026-01-01T00:00:00Z".to_string(),
                        }
                    })
                    .collect();

                let input_slugs: HashSet<String> = tasks.iter().map(|t| t.slug.clone()).collect();
                let a_id_set: HashSet<String> = a_ids.into_iter().collect();

                let expected_count = tasks
                    .iter()
                    .filter(|t| t.session.as_deref().is_some_and(|s| a_id_set.contains(s)))
                    .count();

                let result = filter_tasks_for_project(dir.path(), "proj-a", tasks);

                for t in &result {
                    // Subset: every returned task was in the input.
                    prop_assert!(input_slugs.contains(&t.slug));
                    // Membership: every returned task's session is a proj-a session.
                    prop_assert!(t.session.as_deref().is_some_and(|s| a_id_set.contains(s)));
                }
                // Complement: nothing that should have been kept was dropped —
                // without this, the two checks above pass vacuously whenever
                // `result` is empty, and "drops_sessionless" (the test's own
                // name) was never actually pinned (#1121 pre-pr-review).
                prop_assert_eq!(result.len(), expected_count);
            }

            // -- #1121: resolve_current_task --

            #[test]
            fn resolve_current_task_never_returns_done(tasks in arb_forest()) {
                if let Some(current) = resolve_current_task(&tasks) {
                    prop_assert_ne!(current.state, TaskState::Done);
                }
            }

            #[test]
            fn resolve_current_task_prefers_wip_when_present(tasks in arb_forest()) {
                if tasks.iter().any(|t| t.state == TaskState::Wip) {
                    let current = resolve_current_task(&tasks);
                    prop_assert_eq!(current.map(|t| t.state), Some(TaskState::Wip));
                }
            }

            #[test]
            fn resolve_current_task_none_when_everything_done(mut tasks in arb_forest()) {
                for t in &mut tasks {
                    t.state = TaskState::Done;
                }
                prop_assert_eq!(resolve_current_task(&tasks), None);
            }

            // -- #1121: resolve_next_task --

            #[test]
            fn resolve_next_task_blockers_all_resolve_to_done(
                tasks in arb_forest_with_blockers(),
                idx in 0usize..8,
            ) {
                let current = tasks[idx % tasks.len()].clone();
                if let Some(next) = resolve_next_task(&tasks, &tasks, &current) {
                    let by_slug: HashMap<&str, &Task> =
                        tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
                    for blocker in &next.blocked_on {
                        prop_assert!(is_done_including_descendants(blocker, &by_slug));
                    }
                }
            }

            #[test]
            fn resolve_next_task_comes_strictly_after_current_in_execution_order(
                tasks in arb_forest(),
                idx in 0usize..8,
            ) {
                let current = tasks[idx % tasks.len()].clone();
                if let Some(next) = resolve_next_task(&tasks, &tasks, &current) {
                    let order = execution_order(&tasks);
                    let current_pos = order
                        .iter()
                        .position(|t| t.slug == current.slug)
                        .expect("current came from tasks, so execution_order(tasks) must contain it");
                    let next_pos = order
                        .iter()
                        .position(|t| t.slug == next.slug)
                        .expect("resolve_next_task draws next from execution_order(tasks) itself");
                    prop_assert!(next_pos > current_pos);
                }
            }

            #[test]
            fn resolve_next_task_none_when_current_not_in_session_tasks(tasks in arb_forest()) {
                let dangling = Task {
                    slug: "not-in-forest".to_string(),
                    title: "dangling".to_string(),
                    state: TaskState::Open,
                    parent: None,
                    blocked_on: Vec::new(),
                    notes: Vec::new(),
                    session: Some("s".to_string()),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                };
                prop_assert_eq!(resolve_next_task(&tasks, &tasks, &dangling), None);
            }

            #[test]
            fn resolve_next_task_none_when_current_is_last_in_execution_order(tasks in arb_forest()) {
                let order = execution_order(&tasks);
                let last = order.last().unwrap().clone();
                prop_assert_eq!(resolve_next_task(&tasks, &tasks, &last), None);
            }

            // -- #930: edit_task parent-change cycle rejection --

            #[test]
            fn edit_task_parent_change_cycle_rejection_matches_forest_structure(
                forest in arb_forest(),
                a_idx in 0usize..8,
                b_idx in 0usize..8,
            ) {
                let a = forest[a_idx % forest.len()].clone();
                let b = forest[b_idx % forest.len()].clone();
                prop_assume!(a.slug != b.slug);

                let dir = TempDir::new().unwrap();
                for t in &forest {
                    save_task(dir.path(), t).unwrap();
                }

                // Independent reference computation (doesn't call reject_cycle):
                // is `a` among `b`'s ancestors, per the forest's own `parent`
                // links? `arb_forest` guarantees every parent index is earlier
                // than its child's, so this walk always terminates.
                let mut a_is_ancestor_of_b = false;
                let mut current = b.parent.clone();
                while let Some(p) = current {
                    if p == a.slug {
                        a_is_ancestor_of_b = true;
                        break;
                    }
                    current = forest.iter().find(|t| t.slug == p).and_then(|t| t.parent.clone());
                }

                let edit = TaskEdit {
                    parent: Some(&b.slug),
                    ..Default::default()
                };
                let result = edit_task(dir.path(), &a.slug, &edit);

                prop_assert_eq!(
                    result.is_err(),
                    a_is_ancestor_of_b,
                    "setting '{}' parent to '{}': expected cycle rejection = {}, got is_err = {}",
                    a.slug,
                    b.slug,
                    a_is_ancestor_of_b,
                    result.is_err(),
                );
            }
        }
    }
}
