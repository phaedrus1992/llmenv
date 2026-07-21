# Task Tracker: Mandatory Sessions, Project Tagging, and an `llmenv` Skill — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `llmenv task` sessions mandatory and project-tagged so two concurrent
windows in the same project can't silently collide, add a `session start`
resume/replace/new checkpoint, and ship a minimal `llmenv` skill that teaches
the agent the new model — all while keeping the task/session store exactly
where it is today (flat, global-per-engine, no partitioning).

**Architecture:** A new pure function (`src/task/project.rs`) resolves a
`{name}-{hash}` project tag from cwd (git-root-first, `.llmenv.yaml` fallback,
cwd-literal last resort). `Session` gains `project`/`description`/
`last_activity` fields, and the whole "one global active-session pointer"
model is deleted in favor of querying sessions by `(open, project)` directly
— any number of sessions can be open at once, globally or per project.
`task add` requires resolving to exactly one session (explicit `--session`,
or auto-resolved when exactly one is open for the current project).
`session start` gets a checkpoint: zero existing same-project sessions creates
cleanly, one-or-more requires `--resume <id>` / `--replace` / `--new`. The
statusline `tasks` widget and the Stop/SessionStart hook reminders are
rescoped from "the one active session" to "sessions open for this project".
Finally, a new `llmenv` skill (thin router + 4 conditional reference files,
embedded via `include_str!` like the existing `setup-llmenv` skill) replaces
`TASK_TRACKER_FRAGMENT`, materialized directly into every adapter's `skills/`
output (bypassing `SkillSource`/`write_first_class_skills`, which requires a
pre-existing on-disk source dir we don't have — our skill is embedded Rust
string constants, not a user-configured bundle skill).

**Tech Stack:** Rust (this repo's existing `llmenv` binary crate), `sha2`
(already a direct `Cargo.toml` dependency, used by `src/materialize/cache.rs`
for content-hash folder naming), `humantime` (already a direct dependency,
used throughout `src/task/`), `clap` derive macros (already used for every
other `llmenv task` subcommand), `proptest` (already a workspace dependency).
No new dependencies.

## Global Constraints

- **Prerequisite — rebase first.** This branch (`feat/task-project-scoping`)
  forked from `release/3.x` *before* PR #907 (`fix/task-waiting-state`)
  merged. That PR added `TaskState::Waiting` and `task ls --session`. Before
  starting Task 1, rebase this branch onto the latest `release/3.x` (once
  #907 has merged) so this plan builds on top of `TaskState::{Open, Wip,
  Waiting, Done}` and the existing `Ls { format, session: Option<String> }`
  CLI shape, instead of re-deriving either. If #907 hasn't merged yet when
  you start, ask before proceeding — don't duplicate that work here.
- **No store partitioning.** The task/session store stays exactly where it
  is (`state_dir()/tasks/`, `state_dir()/tasks/sessions/`) — every change in
  this plan is schema/query-level, never a path change. Nothing to migrate.
- **No new dependencies.** `sha2` and `humantime` are already direct
  `Cargo.toml` deps (not in `[workspace.dependencies]`, but that's fine — the
  new code lives in the existing root binary crate, not a new crate).
- **Reuse before adding.** `crate::task::slugify` already produces a
  lowercase, hyphenated, fs-safe string — reuse it for the project tag's name
  component rather than writing a second sanitizer.
- **Workspace lints apply.** `unwrap_used = "deny"`, `panic = "deny"` — use
  `?`/`anyhow::bail!` everywhere, `.expect("test")` is fine only inside
  `#[cfg(test)]` modules (matches the existing convention throughout
  `src/task/`).
- **`cargo fmt` after every edit, before staging.** Run
  `cargo clippy --all-targets --all-features -- -D warnings` and
  `cargo test` (workspace-wide; budget ~2-3 minutes, use an extended timeout)
  before every commit in this plan.
- **Changelog.** Per `AGENTS.md`, every user-facing change needs a
  `CHANGELOG-3.md` entry under `## [Unreleased]` before this ships — Task 10
  covers it; don't add entries piecemeal in earlier tasks.

---

### Task 1: Project tag resolution (`src/task/project.rs`)

**Files:**

- Create: `src/task/project.rs`
- Modify: `src/task/mod.rs:14` (add `pub mod project;` next to the existing
  `pub mod session;`)
- Test: inline `#[cfg(test)] mod tests` in `src/task/project.rs`

**Interfaces:**

- Consumes: `crate::task::slugify(title: &str) -> String` (already exists,
  `src/task/mod.rs:102`), `crate::scope::matcher::{discover_project, Env}`
  (already exist, `src/scope/matcher.rs:48,327`).
- Produces: `pub fn resolve_project_tag(cwd: &Path, home: Option<&Path>) -> String`
  — used by Task 3 (`session::start_session`'s auto-resolve), Task 4
  (`add_task`'s auto-resolve and the reminders), and Task 6 (statusline
  collector).

- [ ] **Step 1: Write the failing tests**

```rust
// src/task/project.rs (bottom of file)
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn git_dir_root_uses_directory_basename_when_no_llmenv_yaml() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("my-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test");
        let sub = root.join("src").join("nested");
        std::fs::create_dir_all(&sub).expect("test");

        let tag = resolve_project_tag(&sub, None);
        assert!(tag.starts_with("my-repo-"), "tag was {tag}");
        assert_eq!(tag.len(), "my-repo-".len() + 10);
    }

    #[test]
    fn git_file_worktree_pointer_counts_as_a_git_root() {
        // Existence-only check per the design doc: a `.git` *file* (worktree
        // or submodule pointer) counts, its target is never resolved.
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("worktree-repo");
        std::fs::create_dir_all(&root).expect("test");
        std::fs::write(root.join(".git"), "gitdir: ../elsewhere/.git/worktrees/x")
            .expect("test");

        let tag = resolve_project_tag(&root, None);
        assert!(tag.starts_with("worktree-repo-"), "tag was {tag}");
    }

    #[test]
    fn llmenv_yaml_id_overrides_basename_when_colocated_with_git_root() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("basename-ignored");
        std::fs::create_dir_all(root.join(".git")).expect("test");
        std::fs::write(root.join(".llmenv.yaml"), "id: custom-id\n").expect("test");

        let tag = resolve_project_tag(&root, None);
        assert!(tag.starts_with("custom-id-"), "tag was {tag}");
    }

    #[test]
    fn no_git_anywhere_falls_back_to_llmenv_yaml_walkup() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("marker-project");
        std::fs::create_dir_all(&root).expect("test");
        std::fs::write(root.join(".llmenv.yaml"), "id: marker-id\n").expect("test");
        let sub = root.join("nested");
        std::fs::create_dir_all(&sub).expect("test");

        let tag = resolve_project_tag(&sub, Some(dir.path()));
        assert!(tag.starts_with("marker-id-"), "tag was {tag}");
    }

    #[test]
    fn neither_git_nor_marker_falls_back_to_literal_cwd_basename() {
        let dir = TempDir::new().expect("test");
        let cwd = dir.path().join("plain-dir");
        std::fs::create_dir_all(&cwd).expect("test");

        let tag = resolve_project_tag(&cwd, Some(dir.path()));
        assert!(tag.starts_with("plain-dir-"), "tag was {tag}");
    }

    #[test]
    fn same_root_always_produces_the_same_tag() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("stable-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test");

        let a = resolve_project_tag(&root, None);
        let b = resolve_project_tag(&root, None);
        assert_eq!(a, b);
    }

    proptest::proptest! {
        #[test]
        fn tag_is_bounded_length_and_fs_safe(name in "[a-zA-Z0-9_-]{1,40}") {
            let dir = tempfile::TempDir::new().unwrap();
            let root = dir.path().join(&name);
            std::fs::create_dir_all(root.join(".git")).unwrap();
            let tag = resolve_project_tag(&root, None);
            prop_assert!(tag.len() <= 80, "tag too long: {tag}");
            prop_assert!(
                tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "tag has unsafe chars: {tag}"
            );
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib task::project -- --nocapture
```

Expected: compile error (`resolve_project_tag` doesn't exist yet).

- [ ] **Step 3: Write the implementation**

```rust
// src/task/project.rs
//! Project tag resolution (mandatory-sessions design, #493-follow-up): a
//! `{name}-{hash}` tag computed fresh on every invocation from cwd, stored as
//! metadata on a `Session` (never used to partition the task/session store —
//! see `docs/superpowers/specs/2026-07-21-task-project-scoping-design.md`).

use std::path::Path;

use sha2::{Digest, Sha256};

/// Resolve the project tag for `cwd`: `{name}-{hash}`, where `hash` is the
/// first 10 hex characters of `SHA-256(canonicalized absolute root path)`.
///
/// Resolution order:
/// 1. Walk up from `cwd` looking for a `.git` entry (file or directory —
///    existence check only, a worktree/submodule pointer's target is never
///    resolved). If found, that directory is the root. If a `.llmenv.yaml`
///    also exists in that same directory, its `id` field is used as `name`
///    instead of the root's basename (the root itself is still the git
///    root either way).
/// 2. If no `.git` is found anywhere walking up: fall back to the existing
///    `.llmenv.yaml` marker discovery (bounded at `home`, when given).
/// 3. If neither is found: `cwd` itself is the root, named from its
///    basename.
///
/// `name` is passed through [`super::slugify`] so the tag stays fs-safe and
/// bounded-length regardless of source (directory basename, `.llmenv.yaml`
/// `id`, or a Unicode-heavy path) — reuses the same sanitizer task titles
/// already go through rather than a second one.
#[must_use]
pub fn resolve_project_tag(cwd: &Path, home: Option<&Path>) -> String {
    let (root, raw_name) = find_git_root(cwd)
        .map(|root| {
            let name = read_llmenv_yaml_id(&root).unwrap_or_else(|| basename(&root));
            (root, name)
        })
        .or_else(|| {
            let env = crate::scope::matcher::Env {
                hostname: String::new(),
                user: String::new(),
                cwd: cwd.to_string_lossy().into_owned(),
                gateway_mac: None,
                home: home.map(Path::to_path_buf),
                os: String::new(),
            };
            crate::scope::matcher::discover_project(&env).map(|p| (p.root, p.id))
        })
        .unwrap_or_else(|| (cwd.to_path_buf(), basename(cwd)));

    let mut name = super::slugify(&raw_name);
    if name.is_empty() {
        name = "project".to_string();
    }
    let hash = hash_root(&root);
    format!("{name}-{hash}")
}

/// Walk `start` upward looking for a `.git` file or directory (existence
/// check only — a worktree/submodule pointer's target is never resolved).
/// Unbounded upward (unlike the `.llmenv.yaml` fallback): a `.git` entry is
/// definitionally the true project root wherever it lives, there's no
/// "hostile marker above home" concern the way there is for a
/// user-droppable `.llmenv.yaml`.
fn find_git_root(start: &Path) -> Option<std::path::PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn read_llmenv_yaml_id(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join(".llmenv.yaml")).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;
    value.get("id")?.as_str().map(str::to_string)
}

fn basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

fn hash_root(root: &Path) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}
```

Add the module declaration:

```rust
// src/task/mod.rs:14, change from:
pub mod session;
// to:
pub mod session;
pub mod project;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib task::project
```

Expected: all `task::project::tests::*` pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/task/project.rs src/task/mod.rs
git commit -m "feat(task): add project tag resolution"
```

---

### Task 2: Mandatory sessions — core rewrite (`src/task/session.rs`)

This is the load-bearing task: it replaces the single global "active session"
pointer with project-tagged, queryable, N-concurrent sessions. It must land
as one commit — `src/task/mod.rs`'s `add_task` (Task 3) and every CLI call
site (Task 4) depend on the new signatures, and the crate won't compile with
the old and new session APIs half-migrated.

**Files:**

- Modify: `src/task/session.rs` (full rewrite of `Session`, removal of
  `active_session`/`active_pointer_path`, new `start_session`/`finish_session`
  signatures, new `list_sessions`/`open_sessions_for_project`/
  `touch_last_activity`)
- Test: same file's `#[cfg(test)] mod tests` (full rewrite — every existing
  test calling the old `start_session(dir, name, force)` / `active_session()`
  / `finish_session()` signatures must be updated)

**Interfaces:**

- Consumes: `super::{Task, TaskNote, TaskState, list_tasks, now_rfc3339,
  slugify, task_path, tasks_dir, unique_slug, with_store_lock, save_task}`
  (all already exist in `src/task/mod.rs`).
- Produces (used by Task 3's `add_task` and Task 4's CLI, Task 6's
  statusline collector):
  - `pub struct Session { id, name, project, description, last_activity,
    started_at, finished_at, abandoned_at }`
  - `pub fn list_sessions(state_dir: &Path) -> Vec<Session>`
  - `pub fn open_sessions_for_project(state_dir: &Path, project: &str) -> Vec<Session>`
  - `pub fn touch_last_activity(state_dir: &Path, session_id: &str) -> anyhow::Result<()>`
  - `pub enum StartDecision { Auto, Resume(String), Replace, New }`
  - `pub enum StartOutcome { Created(Session), Resumed(Session), Replaced { session: Session, abandoned: Vec<Session> } }`
  - `pub fn start_session(state_dir: &Path, name: Option<&str>, description: Option<&str>, project: &str, decision: StartDecision) -> anyhow::Result<StartOutcome>`
  - `pub fn finish_session(state_dir: &Path, id: &str) -> anyhow::Result<Session>`
  - `pub fn session_progress(state_dir: &Path, session_id: &str) -> (u64, u64)` (unchanged)
  - `pub fn delete_tasks_in_session(state_dir: &Path, session_id: &str) -> anyhow::Result<Vec<Task>>` (unchanged)

- [ ] **Step 1: Write the failing tests (replace the entire existing `mod tests` block)**

```rust
// src/task/session.rs — replace the `#[cfg(test)] mod tests { ... }` block
// at the bottom of the file with this.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::task::{add_task_for_session, done_task, load_task, save_task, start_task};
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
        start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
            .expect("test");
        let err = start_session(dir.path(), Some("second"), None, PROJECT_A, StartDecision::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains("first"), "error should list existing session: {err}");
        assert!(err.contains("--resume"));
        assert!(err.contains("--replace"));
        assert!(err.contains("--new"));
    }

    #[test]
    fn start_session_auto_does_not_see_sessions_from_a_different_project() {
        let dir = TempDir::new().expect("test");
        start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
            .expect("test");
        let outcome =
            start_session(dir.path(), Some("second"), None, PROJECT_B, StartDecision::Auto)
                .expect("test");
        assert!(matches!(outcome, StartOutcome::Created(_)));
    }

    #[test]
    fn start_session_resume_adopts_existing_session_without_new_id() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(first) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
        let StartOutcome::Created(first) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
            panic!("expected Created");
        };
        let outcome =
            start_session(dir.path(), Some("second"), None, PROJECT_A, StartDecision::Replace)
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
        let StartOutcome::Created(first) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
            panic!("expected Created");
        };
        let open_task = add_task_for_session(dir.path(), "Still open", None, &first.id).expect("test");
        let done_task_ = add_task_for_session(dir.path(), "Finished", None, &first.id).expect("test");
        done_task(dir.path(), &done_task_.slug).expect("test");

        start_session(dir.path(), Some("second"), None, PROJECT_A, StartDecision::Replace)
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
        let StartOutcome::Created(first) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
            panic!("expected Created");
        };
        let outcome =
            start_session(dir.path(), Some("second"), None, PROJECT_A, StartDecision::New)
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
        assert_eq!(reloaded.description.as_deref(), Some("dev-sprint issue 493"));
    }

    #[test]
    fn last_activity_updates_on_touch() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
    fn last_activity_updates_on_resume() {
        let dir = TempDir::new().expect("test");
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("first"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("sprint 1"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
        let StartOutcome::Created(session) =
            start_session(dir.path(), Some("sprint 1"), None, PROJECT_A, StartDecision::Auto)
                .expect("test")
        else {
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
```

Note this test file references `crate::task::add_task_for_session`, a small
test-only-convenient wrapper added in Task 3 (`add_task` with an explicit
session id, skipping the mandatory-session resolution dance so `session.rs`'s
own tests don't need to fake a "current project" for every task they add).
Task 3 adds it; `session.rs`'s tests won't compile until Task 3 lands, which
is expected — see the note at the top of this task.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib task::session -- --nocapture
```

Expected: compile errors (new `Session` fields, `StartDecision`,
`StartOutcome`, `list_sessions`, `open_sessions_for_project`,
`touch_last_activity`, and `add_task_for_session` don't exist yet).

- [ ] **Step 3: Write the implementation (replace the whole file above the test module)**

```rust
// src/task/session.rs
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
    fn is_open(&self) -> bool {
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
    Replaced { session: Session, abandoned: Vec<Session> },
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
            Err(e) => eprintln!("llmenv: skipping corrupt session file {}: {e}", path.display()),
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
/// this touch.
pub fn touch_last_activity(state_dir: &Path, session_id: &str) -> anyhow::Result<()> {
    let Ok(mut session) = load_session(state_dir, session_id) else {
        return Ok(());
    };
    if !session.is_open() {
        return Ok(());
    }
    session.last_activity = now_rfc3339();
    save_session(state_dir, &session)
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
                abandon_session(state_dir, session.clone())?;
                abandoned.push(reload_abandoned(state_dir, &session.id)?);
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

fn reload_abandoned(state_dir: &Path, id: &str) -> anyhow::Result<Session> {
    load_session(state_dir, id)
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

/// Build the `session start` checkpoint error message: lists every existing
/// open same-project session with enough detail (id, name, description,
/// idle duration) that the agent or a human can decide `--resume`,
/// `--replace`, or `--new` without needing to inspect anything further.
fn checkpoint_error(existing: &[Session]) -> String {
    let now = std::time::SystemTime::now();
    let lines: Vec<String> = existing
        .iter()
        .map(|s| {
            let idle = humantime::parse_rfc3339(&s.last_activity)
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| humantime::format_duration(std::time::Duration::from_secs(d.as_secs())).to_string())
                .unwrap_or_else(|| "unknown".to_string());
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
/// historical record. Caller must already hold the store lock.
fn abandon_session(state_dir: &Path, mut session: Session) -> anyhow::Result<()> {
    let now = now_rfc3339();
    session.abandoned_at = Some(now.clone());
    save_session(state_dir, &session)?;

    let id = session.id.clone();
    let label = session.name.unwrap_or(session.id);
    for mut task in tasks_in_session(state_dir, &id)
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
    Ok(())
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
```

- [ ] **Step 4: Run the tests to verify they pass**

This will still fail to compile until Task 3 adds `add_task_for_session` —
that's expected (see the note under Step 1). Move directly to Task 3, then
come back and run:

```bash
cargo test --lib task::session
```

Expected: all `task::session::tests::*` pass, once Task 3 is also in place.

- [ ] **Step 5: Format, lint — do not commit yet**

```bash
cargo fmt
```

Commit together with Task 3 (Step 5 there) — the crate doesn't compile with
only this task's changes applied, per the note at the top of this task.

---

### Task 3: Mandatory-session wiring in `src/task/mod.rs`

**Files:**

- Modify: `src/task/mod.rs` (`add_task` signature + mandatory-session
  resolution, `start_task`/`done_task`/`note_task` touch `last_activity`,
  reminders rescoped from the deleted `active_session()` singleton to
  project-scoped queries)
- Test: same file's `#[cfg(test)] mod tests`

**Interfaces:**

- Consumes: `session::{open_sessions_for_project, touch_last_activity,
  Session}` (Task 2).
- Produces: `pub fn add_task(state_dir, title, parent, session_id: Option<&str>, project: &str) -> anyhow::Result<Task>`,
  `pub fn add_task_for_session(state_dir, title, parent, session_id: &str) -> anyhow::Result<Task>`
  (thin explicit-session convenience wrapper, used by `session.rs`'s own
  tests and Task 4's CLI when `--session` is passed), used by Task 4 (CLI)
  and Task 9 (integration tests).

- [ ] **Step 1: Write the failing tests**

```rust
// src/task/mod.rs — add to the existing `#[cfg(test)] mod tests` block
// (after the existing add_task tests, before the proptests submodule).
use super::session::{StartDecision, open_sessions_for_project, start_session};

const PROJECT: &str = "test-project-0000000000";

#[test]
fn add_task_with_explicit_session_tags_it() {
    let dir = TempDir::new().expect("test");
    let session = start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto)
        .expect("test");
    let crate::task::session::StartOutcome::Created(session) = session else {
        panic!("expected Created");
    };
    let task = add_task(dir.path(), "Do thing", None, Some(&session.id), PROJECT).expect("test");
    assert_eq!(task.session, Some(session.id));
}

#[test]
fn add_task_explicit_session_rejects_unknown_id() {
    let dir = TempDir::new().expect("test");
    assert!(add_task(dir.path(), "Do thing", None, Some("no-such-session"), PROJECT).is_err());
}

#[test]
fn add_task_auto_resolves_when_exactly_one_open_session_for_project() {
    let dir = TempDir::new().expect("test");
    let crate::task::session::StartOutcome::Created(session) =
        start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test")
    else {
        panic!("expected Created");
    };
    let task = add_task(dir.path(), "Do thing", None, None, PROJECT).expect("test");
    assert_eq!(task.session, Some(session.id));
}

#[test]
fn add_task_errors_with_zero_open_sessions_for_project() {
    let dir = TempDir::new().expect("test");
    let err = add_task(dir.path(), "Do thing", None, None, PROJECT).unwrap_err();
    assert!(err.to_string().contains("session start"));
}

#[test]
fn add_task_errors_with_two_open_sessions_for_project() {
    let dir = TempDir::new().expect("test");
    start_session(dir.path(), Some("first"), None, PROJECT, StartDecision::Auto).expect("test");
    start_session(dir.path(), Some("second"), None, PROJECT, StartDecision::New).expect("test");
    let err = add_task(dir.path(), "Do thing", None, None, PROJECT).unwrap_err();
    assert!(err.to_string().contains("--session"));
}

#[test]
fn add_task_does_not_auto_resolve_a_different_projects_session() {
    let dir = TempDir::new().expect("test");
    start_session(dir.path(), Some("s"), None, "other-project-1111111111", StartDecision::Auto)
        .expect("test");
    assert!(add_task(dir.path(), "Do thing", None, None, PROJECT).is_err());
}

#[test]
fn start_task_touches_its_sessions_last_activity() {
    let dir = TempDir::new().expect("test");
    let crate::task::session::StartOutcome::Created(session) =
        start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test")
    else {
        panic!("expected Created");
    };
    let original = session.last_activity.clone();
    let task = add_task(dir.path(), "Do thing", None, Some(&session.id), PROJECT).expect("test");
    std::thread::sleep(std::time::Duration::from_secs(1));
    start_task(dir.path(), &task.slug).expect("test");
    let reloaded = open_sessions_for_project(dir.path(), PROJECT)
        .into_iter()
        .find(|s| s.id == session.id)
        .expect("test");
    assert_ne!(reloaded.last_activity, original);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib task:: -- --nocapture
```

Expected: compile errors — `add_task`'s signature doesn't yet take
`session_id`/`project`, `add_task_for_session` doesn't exist.

- [ ] **Step 3: Write the implementation**

Replace the existing `add_task` (currently `src/task/mod.rs:217-248`):

```rust
/// Create a new task in `open` state and persist it, tagged to a resolved
/// session.
///
/// # Errors
/// Errors if `parent` doesn't resolve to an existing task. Errors on the
/// session resolution per the mandatory-sessions design: `session_id`
/// explicit but unknown/closed → error; `session_id` omitted with zero or
/// 2+ open sessions for `project` → error telling the agent to run
/// `llmenv task session start` or pass `--session`; omitted with exactly
/// one open session for `project` → auto-resolved.
pub fn add_task(
    state_dir: &Path,
    title: &str,
    parent: Option<&str>,
    session_id: Option<&str>,
    project: &str,
) -> anyhow::Result<Task> {
    let resolved_session = resolve_session_for_add(state_dir, session_id, project)?;
    let task = add_task_for_session(state_dir, title, parent, &resolved_session)?;
    session::touch_last_activity(state_dir, &resolved_session)?;
    Ok(task)
}

/// `add_task`, but with the session id already resolved — skips the
/// mandatory-session lookup dance. Used by `add_task` itself, and directly
/// by `session.rs`'s own tests (and any caller that already has a session id
/// in hand, e.g. the CLI's `--session <id>` path after it's been validated).
pub fn add_task_for_session(
    state_dir: &Path,
    title: &str,
    parent: Option<&str>,
    session_id: &str,
) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        let dir = tasks_dir(state_dir);
        let parent_slug = match parent {
            Some(p) => Some(resolve_identifier(state_dir, p)?),
            None => None,
        };
        let now = now_rfc3339();
        let mut base_slug = slugify(title);
        if base_slug.is_empty() {
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

fn resolve_session_for_add(
    state_dir: &Path,
    session_id: Option<&str>,
    project: &str,
) -> anyhow::Result<String> {
    if let Some(id) = session_id {
        let open = session::open_sessions_for_project(state_dir, project);
        if !open.iter().any(|s| s.id == id) {
            // Also check globally (a different project's session id is
            // still a real, explicit choice by the caller — only reject if
            // it doesn't exist or isn't open at all).
            let exists_open = session::list_sessions(state_dir)
                .into_iter()
                .any(|s| s.id == id && s.finished_at.is_none() && s.abandoned_at.is_none());
            if !exists_open {
                anyhow::bail!("session '{id}' does not exist or is not open");
            }
        }
        return Ok(id.to_string());
    }
    let open = session::open_sessions_for_project(state_dir, project);
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
```

Update `start_task`, `done_task`, `note_task` to touch the session's
`last_activity` after a successful mutation (each currently ends with
`save_task(state_dir, &task)?; Ok(task)` inside `with_store_lock` — add one
line after the lock closure returns, since `touch_last_activity` takes its
own lock-free path and must not be called while still holding
`with_store_lock`, matching how `session::active_session` used to be called
lock-free from inside `add_task`'s closure. Simplest: call it right after
each function's `with_store_lock(...)?` call returns):

```rust
pub fn start_task(state_dir: &Path, input: &str) -> anyhow::Result<Task> {
    let task = with_store_lock(state_dir, || {
        // ...unchanged body...
    })?;
    if let Some(session_id) = &task.session {
        session::touch_last_activity(state_dir, session_id)?;
    }
    Ok(task)
}
```

Apply the same two-line pattern (`if let Some(session_id) = &task.session {
session::touch_last_activity(state_dir, session_id)?; }` right after the
`with_store_lock` call, before the function's final `Ok(task)`) to
`done_task` and `note_task`.

Rescope the reminders (currently `session_start_reminder`, `stop_hook_reminder`,
`session_finish_reminder` at `src/task/mod.rs:391-449`) from the deleted
global singleton to "sessions open for the current project":

```rust
/// SessionStart hook: if any `wip`/`waiting` tasks exist, build a reminder
/// nudging the agent to resume or close them before starting new work.
pub fn session_start_reminder(state_dir: &Path) -> String {
    combine_reminders([
        wip_reminder(
            state_dir,
            "In-progress tasks from a previous session",
            "Resume one of these or run `llmenv task done <slug>` before starting new work.",
        ),
        session_finish_reminders(state_dir),
    ])
}

/// Stop hook (end-of-turn skip detection): if `wip` tasks remain, remind the
/// agent to update or finish them.
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
        session_finish_reminders(state_dir),
    ])
}

/// For every session open for the current project (resolved from the
/// process's actual cwd — hooks always run with cwd set to the project
/// directory) whose tasks are all done, build a reminder nudging the agent
/// to close it out. Empty string if none qualify, or on any internal error
/// (degrades silently — hooks must never block the agent).
fn session_finish_reminders(state_dir: &Path) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let project = project::resolve_project_tag(&cwd, home.as_deref());
    let mut lines = Vec::new();
    for session in session::open_sessions_for_project(state_dir, &project) {
        let (done, total) = session::session_progress(state_dir, &session.id);
        if total == 0 || done < total {
            continue;
        }
        let label = session.name.as_deref().unwrap_or(session.id.as_str());
        lines.push(format!(
            "All {total} task(s) in session '{label}' ({}) are done — run \
             `llmenv task session finish {}` to close it out, or `llmenv task add <title> \
             --session {}` to add more work to it.",
            session.id, session.id, session.id
        ));
    }
    lines.join("\n\n")
}
```

Remove the now-unused `open_task_count` and change `current_wip_title`'s
signature (currently `src/task/mod.rs:188-208`) from a single
`Option<&str>` session filter to a set of session ids (needed for Task 6's
2+-open-sessions sum case):

```rust
/// Title of the most recently updated `wip` task among tasks tagged to any
/// of `session_ids` — the statusline's "what's in progress right now"
/// fill-in. `None` when nothing matching is currently `wip`, or when
/// `session_ids` is empty.
#[must_use]
pub fn current_wip_title(state_dir: &Path, session_ids: &[String]) -> Option<String> {
    if session_ids.is_empty() {
        return None;
    }
    list_tasks(state_dir)
        .into_iter()
        .filter(|t| t.state == TaskState::Wip)
        .filter(|t| t.session.as_deref().is_some_and(|sid| session_ids.iter().any(|s| s == sid)))
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .map(|t| t.title)
}
```

Delete `open_task_count` entirely (`src/task/mod.rs:185-193`) — its only
caller was the statusline collector's old "no session" fallback, which no
longer exists per the design (Task 6 removes that call site too).

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib task::
```

Expected: every `task::` and `task::session::` test passes (this closes out
Task 2's deferred Step 4 as well).

- [ ] **Step 5: Format, lint, commit (Tasks 2 + 3 together)**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/task/mod.rs src/task/session.rs
git commit -m "feat(task): make sessions mandatory and project-tagged"
```

---

### Task 4: CLI surface (`src/cli/mod.rs`)

**Files:**

- Modify: `src/cli/mod.rs` (`TaskCommand::Add` gains `--session`;
  `TaskSessionCommand` gains `Start`'s new flags, `Finish`/`Show` take an
  optional `id`, plus new `Ls`; `run_task_command`/`run_task_session_command`
  updated to match)
- Test: `tests/task_cli.rs` (Task 9 covers the full integration suite; this
  task's own step just needs the CLI to compile and the existing smoke test
  at the top of `tests/task_cli.rs` — `full_lifecycle_add_start_note_done` —
  to keep passing, updated for the new mandatory-session requirement)

**Interfaces:**

- Consumes: `crate::task::{add_task, project::resolve_project_tag}`,
  `crate::task::session::{start_session, finish_session, list_sessions,
  open_sessions_for_project, StartDecision, StartOutcome}` (Tasks 1-3).

- [ ] **Step 1: Update the failing smoke test first**

`tests/task_cli.rs`'s existing `full_lifecycle_add_start_note_done` predates
mandatory sessions and will now fail (no open session for a fresh temp
store). Update it to start a session first:

```rust
// tests/task_cli.rs — inside full_lifecycle_add_start_note_done, before the
// existing "task add" call, insert:
llmenv(dir.path())
    .args(["task", "session", "start", "sprint"])
    .assert()
    .success();
```

- [ ] **Step 2: Run it to verify it now fails for the right reason**

```bash
cargo build --bin llmenv 2>&1 | head -50
```

Expected: compile errors in `src/cli/mod.rs` (old `add_task`/`start_session`
signatures don't match) — this is the signal to proceed to Step 3.

- [ ] **Step 3: Update `TaskCommand` and `TaskSessionCommand`**

Replace the `Add` variant and the whole `TaskSessionCommand` enum
(`src/cli/mod.rs:368-411`):

```rust
#[derive(Subcommand)]
enum TaskCommand {
    /// Create a new task (open state). Requires exactly one open session
    /// for the current project (auto-resolved), or an explicit --session.
    Add {
        title: String,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        session: Option<String>,
    },
    Start { id: String },
    Done { id: String },
    Ls {
        #[arg(long, value_enum)]
        format: Option<TaskListFormat>,
        #[arg(long)]
        session: Option<String>,
    },
    Show { id: String },
    Note { id: String, text: Option<String> },
    Wait { id: String, reason: Option<String> },
    Block {
        id: String,
        #[arg(long)]
        on: String,
    },
    Clear {
        ids: Vec<String>,
        #[arg(long, conflicts_with = "ids")]
        session: Option<String>,
    },
    Session {
        #[command(subcommand)]
        command: TaskSessionCommand,
    },
}

/// `llmenv task session` sub-subcommands.
#[derive(Subcommand)]
enum TaskSessionCommand {
    /// Start a new session for the current project. Errors if one or more
    /// sessions are already open for this project, unless --resume,
    /// --replace, or --new resolves the conflict.
    Start {
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Adopt an existing open session instead of creating a new one.
        #[arg(long, conflicts_with_all = ["replace", "new"])]
        resume: Option<String>,
        /// Abandon every existing open session for this project, then
        /// create a fresh one.
        #[arg(long, conflicts_with_all = ["resume", "new"])]
        replace: bool,
        /// Create a new session anyway, leaving existing open session(s)
        /// for this project untouched — true concurrency.
        #[arg(long, conflicts_with_all = ["resume", "replace"])]
        new: bool,
    },
    /// Finish a session by id. Auto-resolves when exactly one session is
    /// open for the current project.
    Finish { id: Option<String> },
    /// Show one session's progress. Auto-resolves like `Finish`.
    Show { id: Option<String> },
    /// List every currently open session, current-project matches first.
    Ls,
}
```

Note `Wait { id: String, reason: Option<String> }` is already present after
the Task 0 rebase (PR #907) — listed here only so the enum's final shape is
visible in one place; don't duplicate it if the rebase already landed it.

- [ ] **Step 4: Update `run_task_command` and `run_task_session_command`**

```rust
fn run_task_command(command: TaskCommand) -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    let project = current_project_tag()?;
    match command {
        TaskCommand::Add { title, parent, session } => {
            if parent.is_none() {
                let wip: Vec<String> = crate::task::list_tasks(&state_dir)
                    .into_iter()
                    .filter(|t| {
                        matches!(
                            t.state,
                            crate::task::TaskState::Wip | crate::task::TaskState::Waiting
                        )
                    })
                    .map(|t| t.title)
                    .collect();
                if !wip.is_empty() {
                    println!(
                        "Note: you have {} task(s) already in progress ({}). \
                         Consider `--parent <slug>` to make this a sub-task, \
                         or finish the current work first.",
                        wip.len(),
                        wip.join(", ")
                    );
                }
            }
            let task =
                crate::task::add_task(&state_dir, &title, parent.as_deref(), session.as_deref(), &project)?;
            println!("Added task '{}' ({})", task.slug, task.title);
        }
        TaskCommand::Session { command } => {
            run_task_session_command(&state_dir, &project, command)?;
        }
        // ...Start/Done/Ls/Show/Note/Wait/Block/Clear unchanged from the
        // pre-existing implementation (Ls already threads its --session
        // filter per PR #907; nothing else in this match arm depends on
        // project resolution)...
        other => run_task_command_unchanged_arms(&state_dir, other)?,
    }
    Ok(())
}

/// Resolve the project tag for the current process's cwd — every `llmenv
/// task` invocation runs with cwd set to wherever the agent invoked it from.
fn current_project_tag() -> anyhow::Result<String> {
    let cwd = std::env::current_dir()?;
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    Ok(crate::task::project::resolve_project_tag(&cwd, home.as_deref()))
}

fn run_task_session_command(
    state_dir: &std::path::Path,
    project: &str,
    command: TaskSessionCommand,
) -> anyhow::Result<()> {
    use crate::task::session::{self, StartDecision, StartOutcome};
    match command {
        TaskSessionCommand::Start { name, description, resume, replace, new } => {
            let decision = match (resume, replace, new) {
                (Some(id), false, false) => StartDecision::Resume(id),
                (None, true, false) => StartDecision::Replace,
                (None, false, true) => StartDecision::New,
                (None, false, false) => StartDecision::Auto,
                _ => unreachable!("clap's conflicts_with_all enforces at most one"),
            };
            let outcome = session::start_session(
                state_dir,
                name.as_deref(),
                description.as_deref(),
                project,
                decision,
            )?;
            match outcome {
                StartOutcome::Created(s) => println!(
                    "Started session '{}'{}",
                    s.id,
                    s.name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default()
                ),
                StartOutcome::Resumed(s) => println!("Resumed session '{}'", s.id),
                StartOutcome::Replaced { session, abandoned } => {
                    for a in &abandoned {
                        println!(
                            "Abandoned session '{}' — its incomplete tasks were untagged and noted as orphaned.",
                            a.id
                        );
                    }
                    println!("Started session '{}'", session.id);
                }
            }
        }
        TaskSessionCommand::Finish { id } => {
            let id = resolve_session_id(state_dir, project, id)?;
            let session = session::finish_session(state_dir, &id)?;
            let (done, total) = session::session_progress(state_dir, &session.id);
            println!("Finished session '{}' ({done}/{total} done)", session.id);
        }
        TaskSessionCommand::Show { id } => {
            let id = resolve_session_id(state_dir, project, id)?;
            let (done, total) = session::session_progress(state_dir, &id);
            println!("Session '{id}': {done}/{total} done");
        }
        TaskSessionCommand::Ls => {
            let mut sessions: Vec<_> =
                session::list_sessions(state_dir).into_iter().filter(|s| {
                    s.finished_at.is_none() && s.abandoned_at.is_none()
                }).collect();
            sessions.sort_by_key(|s| (s.project != project, s.started_at.clone()));
            if sessions.is_empty() {
                println!("No open sessions.");
            }
            for s in &sessions {
                println!(
                    "{}\t{}\t{}\t{}",
                    s.id,
                    s.name.as_deref().unwrap_or("-"),
                    s.project,
                    s.description.as_deref().unwrap_or("-"),
                );
            }
        }
    }
    Ok(())
}

/// Resolve an explicit-or-omitted session id the same way `add_task` does:
/// omitted + exactly one open session for `project` auto-resolves, omitted +
/// zero/2+ errors.
fn resolve_session_id(
    state_dir: &std::path::Path,
    project: &str,
    id: Option<String>,
) -> anyhow::Result<String> {
    if let Some(id) = id {
        return Ok(id);
    }
    let open = crate::task::session::open_sessions_for_project(state_dir, project);
    match open.len() {
        0 => anyhow::bail!("no open session for this project — pass an id explicitly"),
        1 => Ok(open[0].id.clone()),
        n => anyhow::bail!("{n} open sessions for this project — pass an id explicitly"),
    }
}
```

The `run_task_command_unchanged_arms` placeholder above is a writing aid,
not real code — inline the existing `Start`/`Done`/`Ls`/`Show`/`Note`/
`Wait`/`Block`/`Clear` match arms directly into `run_task_command`'s `match`
exactly as they exist today (they don't change in this task); don't
introduce an actual function with that name.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --test task_cli
```

Expected: `full_lifecycle_add_start_note_done` passes with the session-start
line added in Step 1.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/cli/mod.rs tests/task_cli.rs
git commit -m "feat(task): wire mandatory sessions into the task/session CLI"
```

---

### Task 5: Statusline `tasks` widget — project-scoped rework

**Files:**

- Modify: `src/cli/statusline/data.rs` (`TasksData` shape)
- Modify: `src/materialize/status_data.rs` (`collect_tasks`/
  `collect_tasks_from_state_dir`)
- Modify: `src/cli/statusline/llmenv_widget.rs` (`render_tasks`)
- Test: `src/materialize/status_data.rs`'s existing
  `collect_tasks_*` tests (around line 942+) and `llmenv_widget.rs`'s
  `render_tasks` tests

**Interfaces:**

- Consumes: `crate::task::{current_wip_title, project::resolve_project_tag}`,
  `crate::task::session::{open_sessions_for_project, session_progress}`
  (Tasks 1, 3).

- [ ] **Step 1: Write the failing tests**

```rust
// src/materialize/status_data.rs — replace the existing collect_tasks_*
// tests (around line 942+) with:
#[test]
fn collect_tasks_zero_open_sessions_for_project_is_none() {
    let dir = TempDir::new().expect("test");
    let data = collect_tasks_from_state_dir(dir.path(), "some-project-0000000000");
    assert!(data.session.is_none());
    assert!(data.current.is_none());
}

#[test]
fn collect_tasks_one_open_session_reports_done_over_total() {
    let dir = TempDir::new().expect("test");
    let project = "proj-0000000000";
    let crate::task::session::StartOutcome::Created(session) =
        crate::task::session::start_session(
            dir.path(),
            Some("s"),
            None,
            project,
            crate::task::session::StartDecision::Auto,
        )
        .expect("test")
    else {
        panic!("expected Created");
    };
    let t1 = crate::task::add_task_for_session(dir.path(), "one", None, &session.id).expect("test");
    crate::task::add_task_for_session(dir.path(), "two", None, &session.id).expect("test");
    crate::task::done_task(dir.path(), &t1.slug).expect("test");

    let data = collect_tasks_from_state_dir(dir.path(), project);
    assert_eq!(data.session, Some(SessionProgress { done: 1, total: 2 }));
}

#[test]
fn collect_tasks_two_open_sessions_sums_across_project_only() {
    let dir = TempDir::new().expect("test");
    let project = "proj-0000000000";
    let other = "other-1111111111";
    let crate::task::session::StartOutcome::Created(s1) = crate::task::session::start_session(
        dir.path(),
        Some("first"),
        None,
        project,
        crate::task::session::StartDecision::Auto,
    )
    .expect("test") else {
        panic!("expected Created");
    };
    let crate::task::session::StartOutcome::Created(s2) = crate::task::session::start_session(
        dir.path(),
        Some("second"),
        None,
        project,
        crate::task::session::StartDecision::New,
    )
    .expect("test") else {
        panic!("expected Created");
    };
    crate::task::session::start_session(
        dir.path(),
        Some("unrelated"),
        None,
        other,
        crate::task::session::StartDecision::Auto,
    )
    .expect("test");

    let t1 = crate::task::add_task_for_session(dir.path(), "a", None, &s1.id).expect("test");
    crate::task::done_task(dir.path(), &t1.slug).expect("test");
    crate::task::add_task_for_session(dir.path(), "b", None, &s2.id).expect("test");

    let data = collect_tasks_from_state_dir(dir.path(), project);
    assert_eq!(data.session, Some(SessionProgress { done: 1, total: 2 }));
}

#[test]
fn collect_tasks_current_is_scoped_to_the_projects_open_sessions() {
    let dir = TempDir::new().expect("test");
    let project = "proj-0000000000";
    let crate::task::session::StartOutcome::Created(session) =
        crate::task::session::start_session(
            dir.path(),
            Some("s"),
            None,
            project,
            crate::task::session::StartDecision::Auto,
        )
        .expect("test")
    else {
        panic!("expected Created");
    };
    let task = crate::task::add_task_for_session(dir.path(), "In progress", None, &session.id)
        .expect("test");
    crate::task::start_task(dir.path(), &task.slug).expect("test");

    let data = collect_tasks_from_state_dir(dir.path(), project);
    assert_eq!(data.current.as_deref(), Some("In progress"));
}
```

```rust
// src/cli/statusline/llmenv_widget.rs — replace the render_tasks tests with:
#[test]
fn render_tasks_none_session_renders_empty() {
    let data = StatusData { tasks: Some(TasksData { session: None, current: None }), ..Default::default() };
    assert_eq!(render_tasks(&data, None), "");
}

#[test]
fn render_tasks_some_session_shows_done_over_total() {
    let data = StatusData {
        tasks: Some(TasksData {
            session: Some(SessionProgress { done: 2, total: 5 }),
            current: None,
        }),
        ..Default::default()
    };
    assert_eq!(render_tasks(&data, None), "\u{2611} 2/5");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib materialize::status_data::tests::collect_tasks
cargo test --lib cli::statusline::llmenv_widget::tests::render_tasks
```

Expected: compile errors (`collect_tasks_from_state_dir` doesn't take a
project param yet; `TasksData` still has `open`).

- [ ] **Step 3: Write the implementation**

`src/cli/statusline/data.rs` — replace the `TasksData` doc comment/struct
(currently lines ~59-70):

```rust
/// Task-tracker progress, scoped to sessions open for the current project
/// (mandatory-sessions design). `session` is `None` when zero sessions are
/// open for this project (render empty — no active work tracked here);
/// `Some` gives the summed `(done, total)` across every session open for
/// this project (a single open session's own totals when there's just one).
/// `current` is the title of the most recently updated `wip` task among
/// those sessions — `None` when nothing is currently in progress.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TasksData {
    pub session: Option<SessionProgress>,
    pub current: Option<String>,
}
```

`src/materialize/status_data.rs` — replace `collect_tasks`/
`collect_tasks_from_state_dir` (currently lines 233-267) and its call site:

```rust
// collect_status_data's `tasks: collect_tasks(),` field (line 87) becomes:
tasks: collect_tasks(),

fn collect_tasks() -> Option<TasksData> {
    let state_dir = match crate::paths::state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            tracing::debug!("task stat collection unavailable (non-fatal): {e}");
            return None;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            tracing::debug!("task stat collection unavailable (no cwd): {e}");
            return None;
        }
    };
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let project = crate::task::project::resolve_project_tag(&cwd, home.as_deref());
    Some(collect_tasks_from_state_dir(&state_dir, &project))
}

fn collect_tasks_from_state_dir(state_dir: &Path, project: &str) -> TasksData {
    let open_sessions = crate::task::session::open_sessions_for_project(state_dir, project);
    let session = if open_sessions.is_empty() {
        None
    } else {
        let (done, total) = open_sessions.iter().fold((0, 0), |(ad, at), s| {
            let (d, t) = crate::task::session::session_progress(state_dir, &s.id);
            (ad + d, at + t)
        });
        Some(SessionProgress { done, total })
    };
    let session_ids: Vec<String> = open_sessions.into_iter().map(|s| s.id).collect();
    let current = crate::task::current_wip_title(state_dir, &session_ids);
    TasksData { session, current }
}
```

`src/cli/statusline/llmenv_widget.rs` — replace `render_tasks` (currently
lines 170-199):

```rust
/// Task-tracker progress for the current project's open session(s):
/// `{done}`/`{total}` from the summed session progress, empty string when
/// no session is open for this project. `{current}` (the title of the task
/// currently `wip`) is always available for a custom format, e.g.
/// `"{done}/{total} — {current}"`, but isn't in the default.
fn render_tasks(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(tasks) = &data.tasks else {
        return String::new();
    };
    let Some(session) = tasks.session else {
        return String::new();
    };
    let current = tasks
        .current
        .as_deref()
        .map(super::sanitize)
        .unwrap_or_default();
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("\u{2611} {done}/{total}"); // ☑
    format
        .replace("{done}", &session.done.to_string())
        .replace("{total}", &session.total.to_string())
        .replace("{current}", &current)
}
```

Note `{open}` is no longer a substitutable placeholder — `TasksData.open`
was deleted in Task 3. If any `config.yaml` example or docs page references
`{open}` in a custom `tasks` widget format, Task 10 must update it.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib materialize::status_data
cargo test --lib cli::statusline
```

Expected: all pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/cli/statusline/data.rs src/cli/statusline/llmenv_widget.rs src/materialize/status_data.rs
git commit -m "feat(statusline): scope tasks widget to project-tagged sessions"
```

---

### Task 6: `llmenv` skill content (4 reference files + router)

**Files:**

- Create: `skills/llmenv/SKILL.md`
- Create: `skills/llmenv/references/task-tracker.md`
- Create: `skills/llmenv/references/memory.md`
- Create: `skills/llmenv/references/context-mode.md`
- Create: `skills/llmenv/references/codebase-memory.md`

This task is content-only — no Rust code, no tests (the materialization
logic and its tests are Task 7). Following the `skills/setup-llmenv/`
precedent (`src/cli/setup.rs:141-160`), these are plain markdown files
embedded via `include_str!` in Task 7.

- [ ] **Step 1: Write `skills/llmenv/SKILL.md`**

```markdown
---
name: llmenv
description: >
  How to use llmenv's built-in features (task tracker, memory, context-mode,
  codebase-memory) effectively. Load this first; it points to a reference
  file per enabled feature rather than dumping all of them into context.
---

# llmenv Built-ins

This project has one or more llmenv built-in features enabled. Load only the
reference file for what you're about to do — don't read all of them
up front.

- Tracking durable, cross-session work (tasks, sessions) →
  `references/task-tracker.md`
- Recalling or storing project memory (ICM) → `references/memory.md`
- Reducing token usage for large tool outputs → `references/context-mode.md`
- Looking up code structure/architecture in an indexed repo →
  `references/codebase-memory.md`

Only the reference files for features enabled in this project's config exist
under `references/` — if one of the above isn't listed there, that feature
isn't enabled here.
```

- [ ] **Step 2: Write `skills/llmenv/references/task-tracker.md`**

````markdown
# Task Tracker

Durable, cross-session task state — use it instead of relying on in-session
TODOs.

## Sessions are mandatory

Every task belongs to a session. Before your first `task add`:

```
llmenv task session start "<name>" [--description "<text>"]
```

Pass `--description` whenever you have enough context to make one useful —
a dev-sprint issue number, a brainstorming topic, whatever helps a human
skimming `task session ls` tell your session apart from another one in the
same project. `--description` is separate from `<name>`; keep the name
short.

**If one or more sessions are already open for this project**, `session
start` errors and lists them (id, name, description, idle time). Pick one:

- `--resume <id>` — this is your session from before (e.g. after a context
  compaction wiped your memory of it). Adopts it, no new id.
- `--replace` — the listed session(s) are stale/abandoned. Untags their
  incomplete tasks (noting what happened) and starts fresh.
- `--new` — you are deliberately running alongside another active session
  in this same project (rare — two windows genuinely working in parallel).

## Adding and working tasks

```
llmenv task add "<title>"                # auto-tags to your one open session
llmenv task add "<title>" --session <id> # explicit, if you have 2+ open
llmenv task start <slug>                 # claim it
llmenv task done <slug>                  # finish it
llmenv task note <slug> "<text>"         # record progress before compaction
llmenv task wait <slug> "<reason>"       # blocked on external/human input
```

`task add` errors if zero or 2+ sessions are open for this project and you
didn't pass `--session` — it will not silently create one for you.

## Surviving a context compaction

If you no longer remember your session id, run `llmenv task session ls`. In
the common case (one agent, one project) there's exactly one match — use it.
If there are two or more matches for this project, that means real
concurrency is in play and you need to have durably noted your specific
session id somewhere in your own context before the compaction — there's no
engine-level mechanism that preserves it for you across a compaction.

## Sub-tasks and dependencies

```
llmenv task add "<title>" --parent <slug>   # sub-task
llmenv task block <slug> --on <other-slug>  # ordering dependency
```

## Closing out

```
llmenv task session finish [<id>]   # auto-resolves if exactly one is open
llmenv task session show [<id>]
```
````

- [ ] **Step 3: Write `skills/llmenv/references/memory.md`**

```markdown
# Memory (ICM)

llmenv's memory backend. Use the `icm_*` MCP tools directly (never the
`icm` CLI — see this project's own instructions on why, if the host repo
has any). Typical flow:

- `icm_wake_up` at session start to recall relevant context automatically
  (llmenv already injects this via its `session_start` hook — you usually
  don't need to call it yourself).
- `icm_memory_recall` for a targeted query mid-session.
- `icm_memory_store` to persist something worth remembering across sessions
  — a decision, a gotcha, a solved problem.

Be aggressive about storing: any nontrivial code change, design decision, or
research finding is worth a `icm_memory_store` call, not just session-end
cleanup.
```

- [ ] **Step 4: Write `skills/llmenv/references/context-mode.md`**

```markdown
# Context Mode

Token-efficiency tooling for large tool outputs — runs analysis in a sandbox
and returns only the derived answer, keeping raw bytes out of your context.

- `ctx_batch_execute` — run several shell commands in parallel, each
  auto-indexed; pass `queries` to get matching sections back in the same
  round trip.
- `ctx_search` — follow-up questions against anything already indexed
  (including auto-captured session memory) — batch multiple questions in one
  call.
- `ctx_execute` / `ctx_execute_file` — derive an answer from data you've
  already gathered (filter, count, aggregate) without pulling the raw data
  into your conversation.

Reach for these before reading a large file or command output directly
whenever you only need a derived answer, not the raw content.
```

- [ ] **Step 5: Write `skills/llmenv/references/codebase-memory.md`**

```markdown
# Codebase Memory

An indexed knowledge graph of this repo's code structure — use it instead of
grepping blind when you need architecture-level answers.

- `search_graph` / `search_code` — find functions, routes, symbols by
  meaning, not just text match.
- `trace_path` — follow a call chain from one symbol to another.
- `get_architecture` — a structural overview of the indexed project.
- `index_status` / `index_repository` — check or (re)build the index if it
  looks stale or missing.

Prefer this over an open-ended `grep`/`find` sweep when the question is
"where does X connect to Y" or "what's the shape of this subsystem" rather
than "find this exact string."
```

- [ ] **Step 6: Commit**

```bash
git add skills/llmenv/
git commit -m "docs(skill): write the llmenv built-ins skill content"
```

---

### Task 7: Materialize the `llmenv` skill across all 3 adapters

**Files:**

- Create: `src/adapter/llmenv_skill.rs`
- Modify: `src/adapter/mod.rs` (add `pub(crate) mod llmenv_skill;`)
- Modify: `src/adapter/claude_code.rs` (call the new materializer, delete
  `TASK_TRACKER_FRAGMENT` and its call site)
- Modify: `src/adapter/opencode.rs` (call the new materializer)
- Modify: `src/adapter/crush.rs` (call the new materializer)
- Test: inline in `src/adapter/llmenv_skill.rs`, plus one assertion added to
  each of the 3 adapters' existing `materialize` test suites

**Interfaces:**

- Consumes: `crate::config::Features` (already exists,
  `crates/llmenv-config/src/schema.rs:22`), `crate::paths::write_owner_only`.
- Produces: `pub(crate) fn materialize_llmenv_skill(out: &Path, features: &Features) -> anyhow::Result<Vec<PathBuf>>`
  — called from all 3 adapters' `materialize()`, appended to each adapter's
  `owned` paths vec (the same list `write_first_class_skills` already
  contributes to).

- [ ] **Step 1: Write the failing tests**

```rust
// src/adapter/llmenv_skill.rs (bottom of file)
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::{CodebaseMemory, ContextMode, Features, Memory, TaskTracker};
    use tempfile::TempDir;

    fn features_with(task_tracker: bool, memory: bool, context_mode: bool, codebase_memory: bool) -> Features {
        Features {
            task_tracker: task_tracker.then(|| TaskTracker { enabled: true }),
            memory: if memory {
                vec![Memory {
                    server_host: "h".to_string(),
                    port: 1,
                    ..Default::default()
                }]
            } else {
                Vec::new()
            },
            context_mode: context_mode.then(|| ContextMode { enabled: true }),
            codebase_memory: if codebase_memory { vec![CodebaseMemory::default()] } else { Vec::new() },
            ..Default::default()
        }
    }

    #[test]
    fn no_features_enabled_materializes_nothing() {
        let dir = TempDir::new().expect("test");
        let owned = materialize_llmenv_skill(dir.path(), &features_with(false, false, false, false))
            .expect("test");
        assert!(owned.is_empty());
        assert!(!dir.path().join("skills/llmenv").exists());
    }

    #[test]
    fn task_tracker_only_writes_router_and_one_reference() {
        let dir = TempDir::new().expect("test");
        materialize_llmenv_skill(dir.path(), &features_with(true, false, false, false)).expect("test");
        assert!(dir.path().join("skills/llmenv/SKILL.md").exists());
        assert!(dir.path().join("skills/llmenv/references/task-tracker.md").exists());
        assert!(!dir.path().join("skills/llmenv/references/memory.md").exists());
        assert!(!dir.path().join("skills/llmenv/references/context-mode.md").exists());
        assert!(!dir.path().join("skills/llmenv/references/codebase-memory.md").exists());
    }

    #[test]
    fn all_four_features_writes_all_four_references() {
        let dir = TempDir::new().expect("test");
        materialize_llmenv_skill(dir.path(), &features_with(true, true, true, true)).expect("test");
        for name in ["task-tracker", "memory", "context-mode", "codebase-memory"] {
            assert!(
                dir.path().join(format!("skills/llmenv/references/{name}.md")).exists(),
                "missing {name}.md"
            );
        }
    }

    #[test]
    fn returned_paths_are_relative_to_out() {
        let dir = TempDir::new().expect("test");
        let owned = materialize_llmenv_skill(dir.path(), &features_with(true, false, false, false))
            .expect("test");
        assert!(owned.iter().any(|p| p == std::path::Path::new("skills/llmenv/SKILL.md")));
        assert!(owned.iter().all(|p| p.is_relative()));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --lib adapter::llmenv_skill -- --nocapture
```

Expected: compile error (module doesn't exist yet).

- [ ] **Step 3: Write the implementation**

```rust
// src/adapter/llmenv_skill.rs
//! Materializes the built-in `llmenv` skill (thin router + per-feature
//! reference files) directly into an adapter's `out/skills/llmenv/`,
//! shared across all 3 adapters. Unlike `super::skills::write_first_class_skills`
//! (which copies a user-configured `SkillSource` from an existing on-disk
//! directory), this skill's content is embedded Rust string constants — the
//! same pattern `src/cli/setup.rs`'s `SETUP_SKILL_SOURCE` uses for the
//! one-shot setup wizard skill, applied here to a skill materialized on
//! every `export`/`regenerate` instead.
//!
//! Replaces the old `TASK_TRACKER_FRAGMENT` CLAUDE.md fragment entirely
//! (Claude-Code-only, hand-appended text) with a cross-engine skill that
//! covers all 4 first-party features, materialized only for the ones
//! actually enabled.

use std::path::{Path, PathBuf};

use crate::config::Features;

const SKILL_ROUTER: &str = include_str!("../../skills/llmenv/SKILL.md");
const TASK_TRACKER_REF: &str = include_str!("../../skills/llmenv/references/task-tracker.md");
const MEMORY_REF: &str = include_str!("../../skills/llmenv/references/memory.md");
const CONTEXT_MODE_REF: &str = include_str!("../../skills/llmenv/references/context-mode.md");
const CODEBASE_MEMORY_REF: &str = include_str!("../../skills/llmenv/references/codebase-memory.md");

/// Write `out/skills/llmenv/SKILL.md` plus one reference file per enabled
/// feature. Writes nothing (returns an empty vec, no directory created) when
/// none of the 4 features are enabled.
///
/// # Errors
/// Propagates any I/O error writing the skill files.
pub(crate) fn materialize_llmenv_skill(
    out: &Path,
    features: &Features,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut refs: Vec<(&str, &str)> = Vec::new();
    if features.task_tracker.as_ref().is_some_and(|t| t.enabled) {
        refs.push(("task-tracker.md", TASK_TRACKER_REF));
    }
    if !features.memory.is_empty() {
        refs.push(("memory.md", MEMORY_REF));
    }
    if features.context_mode.as_ref().is_some_and(|c| c.enabled) {
        refs.push(("context-mode.md", CONTEXT_MODE_REF));
    }
    if !features.codebase_memory.is_empty() {
        refs.push(("codebase-memory.md", CODEBASE_MEMORY_REF));
    }
    if refs.is_empty() {
        return Ok(Vec::new());
    }

    let skill_dir = out.join("skills").join("llmenv");
    let mut owned = Vec::new();
    crate::paths::write_owner_only(&skill_dir.join("SKILL.md"), SKILL_ROUTER.as_bytes())?;
    owned.push(PathBuf::from("skills/llmenv/SKILL.md"));

    let refs_dir = skill_dir.join("references");
    for (name, content) in refs {
        crate::paths::write_owner_only(&refs_dir.join(name), content.as_bytes())?;
        owned.push(PathBuf::from("skills/llmenv/references").join(name));
    }
    Ok(owned)
}
```

Register the module in `src/adapter/mod.rs` (alongside the existing
`mod skills;` declaration):

```rust
pub(crate) mod llmenv_skill;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib adapter::llmenv_skill
```

Expected: all pass.

- [ ] **Step 5: Wire it into all 3 adapters, and delete `TASK_TRACKER_FRAGMENT`**

In `src/adapter/claude_code.rs`: delete the `TASK_TRACKER_FRAGMENT` const
(lines 65-99) and its materialize-time append block (the `if let Some(tt) =
manifest.capabilities.features... push_str(TASK_TRACKER_FRAGMENT)` block
inside `materialize`, ~lines 291-299). In its place, after the block that
appends `COMPACT_SURVIVAL_FRAGMENT` and writes `CLAUDE.md`, add:

```rust
owned.extend(crate::adapter::llmenv_skill::materialize_llmenv_skill(
    out,
    manifest.capabilities.features.as_ref().unwrap_or(&Default::default()),
)?);
```

In `src/adapter/opencode.rs` and `src/adapter/crush.rs`: add the same
`owned.extend(crate::adapter::llmenv_skill::materialize_llmenv_skill(out,
...)?);` call inside each adapter's `materialize()`, near where each already
calls `write_first_class_skills`/`project_plugin_skills` (`crush.rs:122`,
`opencode.rs:394` per Task 0's research) — same `out` and
`manifest.capabilities.features` values are already in scope at those call
sites.

- [ ] **Step 6: Add one cross-engine assertion per adapter**

Add to each of `tests/claude_code_adapter.rs`, and the equivalent opencode/
crush adapter test files, a test confirming the skill is gated correctly for
that engine, e.g.:

```rust
// tests/claude_code_adapter.rs
#[test]
fn llmenv_skill_materializes_when_task_tracker_enabled() {
    let (manifest, out) = /* existing test harness pattern for this file —
        build a MergedManifest with capabilities.features.task_tracker =
        Some(TaskTracker { enabled: true }), call ClaudeCodeAdapter::materialize */;
    assert!(out.join("skills/llmenv/SKILL.md").exists());
    assert!(out.join("skills/llmenv/references/task-tracker.md").exists());
    assert!(!out.join("CLAUDE.md").exists() || {
        let content = std::fs::read_to_string(out.join("CLAUDE.md")).unwrap();
        !content.contains("Task Tracker")
    });
}

#[test]
fn llmenv_skill_absent_when_no_features_enabled() {
    let (manifest, out) = /* MergedManifest with capabilities.features = None */;
    assert!(!out.join("skills/llmenv").exists());
}
```

Follow the exact `MergedManifest`/harness construction pattern already used
by the neighboring tests in each of these 3 files (each adapter test file
already has helpers for building a minimal manifest and calling
`materialize` — reuse those, don't invent a new harness).

- [ ] **Step 7: Run the full test suite**

```bash
cargo test
```

Expected: all pass, including the 3 new adapter tests and no leftover
references to `TASK_TRACKER_FRAGMENT` anywhere (`rg -n
"TASK_TRACKER_FRAGMENT|Task Tracker" src/` should return nothing outside
`skills/llmenv/references/task-tracker.md`'s own heading).

- [ ] **Step 8: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/adapter/llmenv_skill.rs src/adapter/mod.rs src/adapter/claude_code.rs \
        src/adapter/opencode.rs src/adapter/crush.rs \
        tests/claude_code_adapter.rs
git commit -m "feat(adapter): materialize the llmenv skill, replacing the task-tracker CLAUDE.md fragment"
```

---

### Task 8: Full integration test pass (`tests/task_cli.rs`)

**Files:**

- Modify: `tests/task_cli.rs`

**Interfaces:**

- Consumes: the compiled `llmenv` binary via `assert_cmd`, same harness
  pattern already established (`llmenv(state_dir)` helper +
  `.env("LLMENV_STATE_DIR", ...)`).

- [ ] **Step 1: Write the failing tests**

```rust
// tests/task_cli.rs — append
#[test]
fn task_add_without_a_session_errors() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "add", "Orphan task"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("session start"));
}

#[test]
fn session_start_then_task_add_auto_resolves() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "start", "sprint", "--description", "issue 493"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Ship it"])
        .assert()
        .success();
    let ls = llmenv(dir.path())
        .args(["task", "ls", "--format", "json"])
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert!(tasks[0]["session"].is_string());
}

#[test]
fn session_start_twice_without_a_flag_errors_listing_the_existing_one() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "second"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("first"));
}

#[test]
fn session_start_replace_abandons_and_creates_fresh() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "second", "--replace"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Abandoned"));
}

#[test]
fn session_start_new_allows_concurrent_sessions_in_the_same_project() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "second", "--new"])
        .assert()
        .success();
    let ls = llmenv(dir.path())
        .args(["task", "session", "ls"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(ls.stdout).unwrap();
    assert!(stdout.contains("first"));
    assert!(stdout.contains("second"));
}

#[test]
fn task_add_with_two_open_sessions_requires_explicit_session_flag() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "start", "first"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "start", "second", "--new"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "add", "Ambiguous"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--session"));
}

#[test]
fn session_finish_by_id_closes_it_out() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "session", "start", "sprint"])
        .assert()
        .success();
    let ls = llmenv(dir.path())
        .args(["task", "session", "ls"])
        .output()
        .unwrap();
    let id = String::from_utf8(ls.stdout).unwrap().lines().next().unwrap().split('\t').next().unwrap().to_string();
    llmenv(dir.path())
        .args(["task", "session", "finish", &id])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "session", "ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No open sessions"));
}
```

- [ ] **Step 2: Run the tests to verify they fail, then pass**

```bash
cargo test --test task_cli
```

Expected: fails first (behavior not wired for the reworked CLI edge cases
this test file didn't cover before), passes once Tasks 2-4 are correctly in
place — if any of these fail after Task 4's commit, that's a real bug to fix
before moving on, not a plan error to route around.

- [ ] **Step 3: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add tests/task_cli.rs
git commit -m "test(task): integration-test the mandatory-session CLI flow end to end"
```

---

### Task 9: Docs

**Files:**

- Modify: `website/docs/commands.md` (the `task`/`task session` command
  list — add `--session` on `task add`, the new `session start` flags,
  `session finish/show <id>`, `session ls`)
- Modify: `website/docs/configuration.md` (if it documents the `tasks`
  statusline widget's `{open}` placeholder — remove it, per Task 5's note)
- Check: `docs/licensing.md`/`THIRD-PARTY-LICENSES.md` — not applicable,
  no new dependency

- [ ] **Step 1: Update `website/docs/commands.md`**

Find the existing `task`/`task session` documentation block (added by
PR #907's `0f0a6e6` commit) and update it to describe: sessions are
mandatory before `task add` works; `task add --session <id>`; `session
start [name] [--description <text>] [--resume <id> | --replace | --new]`;
`session finish [id]` / `session show [id]` (auto-resolve when
unambiguous); `session ls`.

- [ ] **Step 2: Search for and remove any `{open}` widget-format references**

```bash
rg -n '\{open\}' website/docs/
```

If any hits appear in `configuration.md`'s `tasks` widget example, replace
with `{done}`/`{total}`/`{current}` (the surviving placeholders per Task 5).

- [ ] **Step 3: Commit**

```bash
git add website/docs/commands.md website/docs/configuration.md
git commit -m "docs: document mandatory task sessions and the reworked tasks widget"
```

---

### Task 10: Changelog

**Files:**

- Modify: `CHANGELOG-3.md`

Per `AGENTS.md`'s hard rule, every user-facing change needs an entry under
`## [Unreleased]`. This is a breaking behavior change (`task add` now
requires a session) plus a new feature (`llmenv` skill, `session ls`,
resume/replace/new) — read `RELEASING.md` before editing if anything about
version/heading state is unclear (this plan never bumps a version or adds a
`## [X.Y.Z]` heading — only adds to `[Unreleased]`).

- [ ] **Step 1: Invoke the `keepachangelog` skill**

Per `AGENTS.md`: "Invoke the `keepachangelog` skill to check and write
entries." Don't hand-write the entry without it — follow whatever that
skill's own process requires (categorization under `### Added`/`### Changed`,
line-length/formatting checks, reconciling against the older release line
per `AGENTS.md`'s forward-merge note if applicable).

- [ ] **Step 2: Verify docs back the entry**

Per `AGENTS.md`: "changelog entries must be backed by up-to-date docs" —
confirm Task 9's `commands.md` updates already cover this end-to-end before
finalizing the entry (link to `commands.md` rather than restating detail
inline, per the changelog style guide).

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG-3.md website/docs/changelog.md
git commit -m "docs: changelog entry for mandatory task sessions and the llmenv skill"
```

## Self-Review Notes

- **Spec coverage:** §1 → Task 1. §2 → Tasks 3-4. §3 → Tasks 2, 4. §4 →
  Task 2 (schema). §5 (`session ls`) → Task 4. §6 (compaction) → Task 6's
  `task-tracker.md` content, no code change needed (§6 is explicitly a
  documented limitation, not a mechanism to build). §7 (statusline) → Task
  5. §8 (skill) → Tasks 6-7.
- **Type consistency check:** `Session.project`/`description`/
  `last_activity` (Task 2) match the field names Task 4's CLI prints and
  Task 6's skill doc describes. `add_task`'s new `session_id`/`project`
  params (Task 3) match every call site touched in Tasks 4-5.
  `current_wip_title`'s new `&[String]` signature (Task 3) matches Task 5's
  collector call.
- **Non-goals respected:** no store partitioning anywhere in this plan
  (every change is to `Session`'s fields/queries, never its file location);
  no migration step (nothing changes shape on disk in a way that breaks
  loading old session files — `project`/`description` are `#[serde(default)]`-
  free on `project` deliberately, since a session created before this change
  simply won't exist anymore once this ships to a fresh-enough state dir;
  if that's a concern for an in-place upgrade, flag it during Task 2's
  review rather than silently adding a default that would make an
  old-format session pass `open_sessions_for_project`'s project-tag filter
  incorrectly).

## Execution Handoff

Plan complete and saved to
`docs/superpowers/plans/2026-07-21-task-project-scoping.md`. Two execution
options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task,
   review between tasks, fast iteration.
2. **Inline Execution** — execute tasks in this session using
   `executing-plans`, batch execution with checkpoints.

Which approach?
