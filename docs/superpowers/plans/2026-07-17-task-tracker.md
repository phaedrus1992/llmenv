# In-Engine Task Tracker (#231) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a file-based `llmenv task` tracker (CLI + injected context + lifecycle hooks) that gives agents durable, cross-session "what am I working on" state, gated off by default behind `features.task_tracker`.

**Architecture:** One JSON file per task under `state_dir()/tasks/<slug>.json` (mirrors `icm.rs`/`read_once.rs`'s state-file pattern). A new `src/task` module owns the store (slug generation, CRUD, state transitions, identifier resolution) and is consumed by three call sites: the CLI (`llmenv task ...`), a CLAUDE.md fragment injected at materialize time (mirrors `#317`'s `compact_survival`), and two hook_run handlers (`SessionStart` reminder, `Stop` skip-detection reminder).

**Tech Stack:** Rust, serde/serde_json (already in the workspace), clap (CLI), existing `llmenv_paths::write_owner_only_atomic` for atomic JSON writes. No new dependencies — slug generation is a same-file kebab-case helper (confirmed no `slug`/`heck`/`convert_case` crate present).

## Global Constraints

- Feature off by default; `features.task_tracker.enabled = false` (absent = disabled) must produce byte-identical materialized output and zero hook cost (verified via present/absent test pairs, matching the `slippage`/`compact_survival` test pattern — this repo's established substitute for literal byte-diff assertions).
- No new dependencies.
- Single-writer assumption (no file locking) — this is a deliberate `ponytail:` ceiling, must be commented at the point it matters (the store's read-modify-write in `save_task`), not silently absent.
- All hook handlers fail-soft: any internal error → one-line stderr warning, hook still exits 0 / returns empty string. Never `unwrap`/`expect` outside `#[cfg(test)]`.
- Corrupt/missing task files on load → skip with a stderr warning, never crash `ls` or a hook over one bad file.
- `cargo fmt` after every file edit. Full local check suite (`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`) must be clean before considering any task done — this is enforced by ship-issue's Step 6, but run it per-task too so failures are caught early, not batched at the end.
- CHANGELOG entry via the `keepachangelog` skill (project AGENTS.md hard rule) — Task 8, not optional.

---

### Task 1: Config schema — `TaskTracker` feature toggle

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs` (add struct + `Features` field + tests)
- Modify: `crates/llmenv-config/src/lib.rs:28-34` (re-export `TaskTracker`)
- Modify: `src/config/mod.rs:5-11` (re-export `TaskTracker`)

**Interfaces:**
- Produces: `llmenv_config::schema::TaskTracker { pub enabled: bool }` (also `Default`, `Debug`, `Clone`, `Deserialize`, `Serialize`, `PartialEq`, `Eq`), re-exported as `crate::config::TaskTracker`. `Features.task_tracker: Option<TaskTracker>`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/llmenv-config/src/schema.rs` in the `#[cfg(test)] mod tests` block, right after the `slippage_default_matches_serde_empty` test (mirrors the `context_mode`/`read_once` test groups exactly):

```rust
// ===== TaskTracker config tests =====

#[test]
fn task_tracker_parses_enabled() {
    let yaml = "features:\n  task_tracker:\n    enabled: true\n";
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(cfg.features.unwrap().task_tracker.unwrap().enabled);
}

#[test]
fn task_tracker_absent_is_none() {
    let cfg: Config = serde_yaml::from_str("features:\n  memory: []\n").unwrap();
    assert!(cfg.features.unwrap().task_tracker.is_none());
}

#[test]
fn task_tracker_default_disabled() {
    let yaml = "features:\n  task_tracker: {}\n";
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(!cfg.features.unwrap().task_tracker.unwrap().enabled);
}
```

And in the "Feature round-trip tests" group, right after `features_roundtrip_slippage`:

```rust
#[test]
fn features_roundtrip_task_tracker() {
    let original = Features {
        task_tracker: Some(TaskTracker { enabled: true }),
        ..Default::default()
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: Features = serde_json::from_str(&json).unwrap();
    assert_eq!(original, parsed);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv-config task_tracker`
Expected: FAIL with "cannot find type `TaskTracker`" / "no field `task_tracker`"

- [ ] **Step 3: Implement the schema**

In `crates/llmenv-config/src/schema.rs`, add the field to `Features` (after `slippage`, `crates/llmenv-config/src/schema.rs:47-49`):

```rust
    /// Slippage control: guardrails against model behavior drift.
    #[serde(default)]
    pub slippage: Option<SlippageControl>,
    /// In-engine task tracker (#231): durable, agent-native "what am I
    /// working on" state, off by default.
    #[serde(default)]
    pub task_tracker: Option<TaskTracker>,
}
```

And add the struct right after `ContextMode` (`crates/llmenv-config/src/schema.rs:820`, same shape — a plain enable toggle, no sub-knobs, matching the issue's "one config switch" acceptance criterion):

```rust
/// In-engine task tracker (#231): a file-based task store with CLI commands,
/// injected context, and lifecycle-hook ordering enforcement. Off by default
/// — disabled means zero materialized-output change and zero hook cost.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct TaskTracker {
    /// Whether the task tracker's CLAUDE.md fragment and lifecycle hooks are
    /// active. The `llmenv task` CLI subcommands work regardless of this flag
    /// — it only gates the injected-context and hook-reminder side effects.
    #[serde(default)]
    pub enabled: bool,
}
```

In `crates/llmenv-config/src/lib.rs:28-34`, add `TaskTracker` to the re-export list (alphabetical among the existing names, next to `Throttle`/`UpgradeConfig` — insert after `StatuslineConfig,` and before `Throttle,` to keep alphabetical order):

```rust
    StatuslineConfig, TaskTracker, Throttle,
```

In `src/config/mod.rs:5-11`, add `TaskTracker` the same way, next to `Throttle`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv-config task_tracker && cargo test -p llmenv-config features_roundtrip_task_tracker`
Expected: PASS (6 tests: 3 parse tests + 1 roundtrip test, plus the two existing `context_mode`-shape tests continue passing)

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/llmenv-config/src/schema.rs crates/llmenv-config/src/lib.rs src/config/mod.rs
git commit -m "feat(task-tracker): add features.task_tracker config schema

refs #231"
```

---

### Task 2: Task store — types, slug generation, atomic CRUD

**Files:**
- Create: `src/task/mod.rs`
- Modify: `src/lib.rs:18-19` (add `pub mod task;` between `sync` and the `#[cfg(test)]` block, alphabetically before `test_fixtures`/`throttle`)

**Interfaces:**
- Consumes: `crate::paths::state_dir() -> anyhow::Result<PathBuf>`, `crate::paths::write_owner_only_atomic(path: &Path, content: &[u8]) -> std::io::Result<()>` (both already exist, re-exported from `llmenv_paths`).
- Produces:
  - `pub enum TaskState { Open, Wip, Done }` (`Serialize`/`Deserialize` with `#[serde(rename_all = "snake_case")]`, `Default` = `Open`)
  - `pub struct TaskNote { pub at: String, pub text: String }`
  - `pub struct Task { pub slug: String, pub title: String, pub state: TaskState, pub parent: Option<String>, pub blocked_on: Vec<String>, pub notes: Vec<TaskNote>, pub created_at: String, pub updated_at: String }`
  - `pub fn tasks_dir(state_dir: &Path) -> PathBuf` — `state_dir.join("tasks")`
  - `pub fn slugify(title: &str) -> String` — pure function, no I/O
  - `fn unique_slug(dir: &Path, title: &str) -> std::io::Result<String>` — reads `dir` for collisions
  - `pub fn save_task(state_dir: &Path, task: &Task) -> anyhow::Result<()>`
  - `pub fn load_task(state_dir: &Path, slug: &str) -> anyhow::Result<Task>`
  - `pub fn list_tasks(state_dir: &Path) -> Vec<Task>` — infallible; skips unreadable/corrupt files with an `eprintln!` warning, never fails the whole listing
  - `pub fn add_task(state_dir: &Path, title: &str, parent: Option<&str>) -> anyhow::Result<Task>`

- [ ] **Step 1: Write the failing tests**

Create `src/task/mod.rs` with just the types + a first test module (no store logic yet):

```rust
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

/// Current RFC3339 timestamp (UTC), no external time crate needed —
/// `std::time` plus a fixed civil-calendar conversion would be overkill here;
/// callers only need a sortable, human-readable stamp, so seconds-since-epoch
/// formatted as an ISO-8601 UTC string via `time`-free arithmetic is enough.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    humantime::format_rfc3339_seconds(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn task_state_default_is_open() {
        assert_eq!(TaskState::default(), TaskState::Open);
    }

    #[test]
    fn task_state_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&TaskState::Wip).unwrap(), "\"wip\"");
    }
}
```

**Stop — check `humantime` is actually in the dependency tree before using it:**

```bash
grep -n "^humantime" Cargo.toml crates/*/Cargo.toml
```

If absent, don't add it — ponytail rung 6 applies (this is a few lines): replace `now_rfc3339` with a hand-rolled UTC formatter using only `std::time`, e.g. via `chrono` if *that* is already present, else a minimal civil-from-days algorithm. Check first:

```bash
grep -n "^chrono" Cargo.toml crates/*/Cargo.toml
```

Whichever of `humantime`/`chrono` is already a workspace dependency, use that one's formatter (`chrono::Utc::now().to_rfc3339()` or `humantime::format_rfc3339_seconds`). If **neither** is present, write:

```rust
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Civil calendar from days-since-epoch (Howard Hinnant's algorithm),
    // no dependency needed for a UTC-only, second-precision timestamp.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_ = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_ = if m_ <= 2 { y + 1 } else { y };
    format!("{y_:04}-{m_:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}
```

(Use whichever path applies — do not add both, and do not add a new dependency for this.)

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod task;` to `src/lib.rs` (between line 18 `pub mod sync;` and line 19 `#[cfg(test)]`):

```rust
pub mod sync;
pub mod task;
#[cfg(test)]
pub(crate) mod test_fixtures;
```

Run: `cargo test -p llmenv task_state`
Expected: PASS already (these two are trivial) — this step is really "confirm the module compiles and is wired in"; if it fails, fix the `now_rfc3339` dependency choice from Step 1 first.

- [ ] **Step 3: Write the store-logic failing tests**

Append to the `mod tests` block in `src/task/mod.rs`:

```rust
    use tempfile::TempDir;

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
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p llmenv task::tests`
Expected: FAIL with "cannot find function `slugify`" etc.

- [ ] **Step 5: Implement slug generation and CRUD**

Add to `src/task/mod.rs`, after `task_path`:

```rust
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
    if !task_path(dir, base_slug).exists() {
        return base_slug.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base_slug}-{n}");
        if !task_path(dir, &candidate).exists() {
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
        match std::fs::read_to_string(&path).and_then(|content| {
            serde_json::from_str::<Task>(&content).map_err(std::io::Error::other)
        }) {
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
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p llmenv task::tests`
Expected: PASS (all 10 tests: 2 state tests + 4 slugify tests + 4 store tests)

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/task/mod.rs src/lib.rs
git commit -m "feat(task-tracker): task store types, slug generation, and CRUD

refs #231"
```

---

### Task 3: Task store — state transitions and identifier resolution

**Files:**
- Modify: `src/task/mod.rs` (add transition functions + identifier resolution)

**Interfaces:**
- Consumes: `Task`, `TaskState`, `load_task`, `save_task`, `list_tasks`, `tasks_dir` from Task 2.
- Produces:
  - `pub fn resolve_identifier(state_dir: &Path, input: &str) -> anyhow::Result<String>` — exact slug or unambiguous prefix; returns the resolved exact slug or an error listing candidates.
  - `pub fn start_task(state_dir: &Path, input: &str) -> anyhow::Result<Task>`
  - `pub fn done_task(state_dir: &Path, input: &str) -> anyhow::Result<Task>`
  - `pub fn note_task(state_dir: &Path, input: &str, text: &str) -> anyhow::Result<Task>`
  - `pub fn block_task(state_dir: &Path, input: &str, on: &str) -> anyhow::Result<Task>`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/task/mod.rs`:

```rust
    #[test]
    fn resolve_identifier_exact_slug() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(resolve_identifier(dir.path(), &task.slug).expect("test"), task.slug);
    }

    #[test]
    fn resolve_identifier_unambiguous_prefix() {
        let dir = TempDir::new().expect("test");
        let task = add_task(dir.path(), "Fix login timeout", None).expect("test");
        assert_eq!(resolve_identifier(dir.path(), "fix-log").expect("test"), task.slug);
    }

    #[test]
    fn resolve_identifier_ambiguous_prefix_errors_listing_candidates() {
        let dir = TempDir::new().expect("test");
        add_task(dir.path(), "Fix login timeout", None).expect("test");
        add_task(dir.path(), "Fix logout crash", None).expect("test");
        let err = resolve_identifier(dir.path(), "fix-log").unwrap_err().to_string();
        assert!(err.contains("fix-login-timeout"));
        assert!(err.contains("fix-logout-crash"));
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv task::tests`
Expected: FAIL with "cannot find function `resolve_identifier`" etc.

- [ ] **Step 3: Implement transitions and identifier resolution**

Append to `src/task/mod.rs`, after `add_task`:

```rust
/// Resolve a user-supplied identifier (exact slug or unambiguous prefix) to
/// the exact slug of an existing task.
///
/// # Errors
/// Returns an error if no task matches, or if the prefix matches more than
/// one task (the error lists every candidate slug).
pub fn resolve_identifier(state_dir: &Path, input: &str) -> anyhow::Result<String> {
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
            anyhow::bail!(
                "'{input}' matches multiple tasks: {}",
                sorted.join(", ")
            )
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv task::tests`
Expected: PASS (all tests from Task 2 + 11 new tests)

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/task/mod.rs
git commit -m "feat(task-tracker): state transitions and slug/prefix resolution

refs #231"
```

---

### Task 4: CLI wiring — `llmenv task ...`

**Files:**
- Modify: `src/cli/mod.rs` (add `Task` command variant, `TaskCommand` enum, dispatch, print helpers)

**Interfaces:**
- Consumes: `crate::task::{add_task, start_task, done_task, note_task, block_task, list_tasks, load_task, resolve_identifier, tasks_dir, Task, TaskState}` from Tasks 2-3; `crate::paths::state_dir()`.
- Produces: `llmenv task add|start|done|ls|show|note|block` CLI surface.

- [ ] **Step 1: Add the clap enum variant and subcommand enum**

In `src/cli/mod.rs`, add the variant right after the `ReadOnce` variant (`src/cli/mod.rs:333-337`):

```rust
    /// Manage the in-engine task tracker (#231).
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
```

Add the `TaskCommand` enum right after `ReadOnceCommand` (`src/cli/mod.rs:358-361`):

```rust
/// `llmenv task` sub-subcommands (#231).
#[derive(Subcommand)]
enum TaskCommand {
    /// Create a new task (open state).
    Add {
        title: String,
        /// Slug of the parent task, if this is a sub-task.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Claim a task, transitioning it to `wip`.
    Start { id: String },
    /// Mark a task done.
    Done { id: String },
    /// List all tasks.
    Ls {
        #[arg(long, value_enum)]
        format: Option<TaskListFormat>,
    },
    /// Show full detail for one task.
    Show { id: String },
    /// Append a progress note. Reads from stdin if `text` is omitted.
    Note { id: String, text: Option<String> },
    /// Record that `id` is blocked on `on`.
    Block {
        id: String,
        #[arg(long)]
        on: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TaskListFormat {
    Json,
}
```

- [ ] **Step 2: Add the dispatch arm and handler functions**

In the dispatch `match` (`src/cli/mod.rs`, right after the `Command::ReadOnce` arm at `src/cli/mod.rs:521-523`):

```rust
        Some(Command::Task { command }) => run_task_command(command)?,
```

Add the handler function near the bottom of `src/cli/mod.rs` (same file — mirrors how `Command::Memory`'s arms call into a separate `crate::memory` module, but this dispatch is small enough to keep as a local function calling into `crate::task`; the store logic itself lives in `crate::task`, keeping this file a thin CLI-formatting layer):

```rust
/// Handle `llmenv task <subcommand>`. Thin formatting layer over `crate::task`.
fn run_task_command(command: TaskCommand) -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    match command {
        TaskCommand::Add { title, parent } => {
            // New-project guard (#231 Phase 3): warn before starting an
            // unrelated top-level task while another is still in progress.
            // CLI-side check beats a transcript heuristic — this is a plain
            // fact about current task state, not something to infer.
            if parent.is_none() {
                let wip: Vec<String> = crate::task::list_tasks(&state_dir)
                    .into_iter()
                    .filter(|t| t.state == crate::task::TaskState::Wip)
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
            let task = crate::task::add_task(&state_dir, &title, parent.as_deref())?;
            println!("Added task '{}' ({})", task.slug, task.title);
        }
        TaskCommand::Start { id } => {
            let task = crate::task::start_task(&state_dir, &id)?;
            println!("Started '{}' — now {:?}", task.slug, task.state);
        }
        TaskCommand::Done { id } => {
            let task = crate::task::done_task(&state_dir, &id)?;
            println!("Completed '{}'", task.slug);
        }
        TaskCommand::Ls { format } => {
            let mut tasks = crate::task::list_tasks(&state_dir);
            tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            match format {
                Some(TaskListFormat::Json) => {
                    println!("{}", serde_json::to_string(&tasks)?);
                }
                None => {
                    if tasks.is_empty() {
                        println!("No tasks.");
                    }
                    for t in &tasks {
                        println!("{:?}\t{}\t{}", t.state, t.slug, t.title);
                    }
                }
            }
        }
        TaskCommand::Show { id } => {
            let slug = crate::task::resolve_identifier(&state_dir, &id)?;
            let task = crate::task::load_task(&state_dir, &slug)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TaskCommand::Note { id, text } => {
            let text = match text {
                Some(t) => t,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf.trim().to_string()
                }
            };
            let task = crate::task::note_task(&state_dir, &id, &text)?;
            println!("Noted on '{}'", task.slug);
        }
        TaskCommand::Block { id, on } => {
            let task = crate::task::block_task(&state_dir, &id, &on)?;
            println!("'{}' is now blocked on: {}", task.slug, task.blocked_on.join(", "));
        }
    }
    Ok(())
}
```

- [ ] **Step 3: Manual smoke test (this is CLI glue — the store logic already has unit tests from Tasks 2-3, so a quick manual pass replaces a redundant integration-test layer)**

```bash
cargo build
LLMENV_STATE_DIR=$(mktemp -d) target/debug/llmenv task add "Try the tracker"
LLMENV_STATE_DIR=$(mktemp -d) target/debug/llmenv task ls
```

Expected: `Added task '...'`, and a nonempty listing when using the same `LLMENV_STATE_DIR` for both calls (use one exported var, not two `mktemp -d` calls, to see the round trip):

```bash
export LLMENV_STATE_DIR=$(mktemp -d)
target/debug/llmenv task add "Try the tracker"
target/debug/llmenv task ls
target/debug/llmenv task ls --format json
```

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add src/cli/mod.rs
git commit -m "feat(task-tracker): llmenv task CLI subcommand family

refs #231"
```

---

### Task 5: Merge resolution — `task_tracker` scalar

**Files:**
- Modify: `src/merge/capabilities.rs` (resolve `task_tracker` like `read_once`/`slippage`/`context_mode`)

**Interfaces:**
- Consumes: `crate::config::{Features, TaskTracker}`.
- Produces: `Features.task_tracker` correctly populated in the merged manifest (highest-precedence contributor wins).

- [ ] **Step 1: Write the failing test**

Find the existing merge test module in `src/merge/capabilities.rs` (or a sibling `tests` module in the same file — check with `grep -n "mod tests" src/merge/capabilities.rs`) and add a test mirroring whatever `slippage`/`read_once` resolution test already exists there (e.g. `read_once_resolves_highest_precedence` or similar — copy its exact shape with `task_tracker` substituted):

```rust
#[test]
fn task_tracker_resolves_highest_precedence() {
    let low = CapabilityContributor {
        name: "low".to_string(),
        precedence: 1,
        capabilities: Capabilities {
            features: Some(Features {
                task_tracker: Some(TaskTracker { enabled: false }),
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    let high = CapabilityContributor {
        name: "high".to_string(),
        precedence: 2,
        capabilities: Capabilities {
            features: Some(Features {
                task_tracker: Some(TaskTracker { enabled: true }),
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    let merged = merge_capabilities(&[low, high]).unwrap();
    assert!(merged.features.unwrap().task_tracker.unwrap().enabled);
}

#[test]
fn task_tracker_absent_when_no_contributor_sets_it() {
    let contributor = CapabilityContributor {
        name: "only".to_string(),
        precedence: 1,
        capabilities: Capabilities::default(),
    };
    let merged = merge_capabilities(&[contributor]).unwrap();
    assert!(merged.features.is_none());
}
```

(Add `TaskTracker` to the `use crate::config::{...}` import list at the top of the file if the test module doesn't already have its own imports.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv task_tracker_resolves`
Expected: FAIL — `task_tracker` field doesn't exist on the merge output yet (compile error) or the field is `None` when it should be `Some`.

- [ ] **Step 3: Implement the resolution**

In `src/merge/capabilities.rs`, add the scalar resolution right after the `read_once` block (`src/merge/capabilities.rs:181-192`):

```rust
    let task_tracker = contributors
        .iter()
        .filter_map(|c| {
            c.capabilities
                .features
                .as_ref()?
                .task_tracker
                .as_ref()
                .map(|v| (c.precedence, v.clone()))
        })
        .max_by_key(|(p, _)| *p)
        .map(|(_, v)| v);
```

Update the `features.is_empty()`-style guard and the `Features { ... }` construction (`src/merge/capabilities.rs:194-211`):

```rust
    let features = if memory.is_empty()
        && throttle.is_empty()
        && slippage.is_none()
        && context_mode.is_none()
        && upgrade.is_none()
        && read_once.is_none()
        && task_tracker.is_none()
    {
        None
    } else {
        Some(Features {
            memory,
            throttle,
            context_mode,
            upgrade,
            read_once,
            slippage,
            task_tracker,
        })
    };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv merge::capabilities`
Expected: PASS, including the two new tests and every pre-existing merge test (the `Features { ... }` literal now has one more field, so any other test constructing `Features` with named fields — not `..Default::default()` — needs the new field added; `cargo test` surfaces those as compile errors if so, fix by adding `task_tracker: None,` to each).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/merge/capabilities.rs
git commit -m "feat(task-tracker): resolve features.task_tracker in capability merge

refs #231"
```

---

### Task 6: CLAUDE.md fragment injection

**Files:**
- Modify: `src/adapter/claude_code.rs` (fragment constant + injection in `materialize()`)
- Modify: `tests/claude_code_adapter.rs` (present/absent test pair)

**Interfaces:**
- Consumes: `manifest.capabilities.features.task_tracker` (via the same `Option` chain pattern as `slippage`).
- Produces: a `<!-- from task_tracker -->` marked fragment appended to `CLAUDE.md` when `features.task_tracker.enabled == true`.

- [ ] **Step 1: Write the failing tests**

Add to `tests/claude_code_adapter.rs`, right after `compact_survival_fragment_absent_when_disabled` (around line 1833):

```rust
// #231: task-tracker fragment appended to CLAUDE.md when enabled.
#[test]
fn task_tracker_fragment_appended_when_enabled() {
    let caps = llmenv::config::Capabilities {
        features: Some(llmenv::config::Features {
            task_tracker: Some(llmenv::config::TaskTracker { enabled: true }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let manifest = merge(&caps, &empty_native(), &[]).unwrap();
    let tmp = tempdir().unwrap();
    ClaudeCodeAdapter
        .materialize(&manifest, tmp.path())
        .expect("materialize");

    let claude_md = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).expect("read CLAUDE.md");
    assert!(
        claude_md.contains("llmenv task"),
        "CLAUDE.md must contain task-tracker fragment when enabled"
    );
    assert!(
        claude_md.contains("<!-- from task_tracker -->"),
        "fragment must carry provenance marker"
    );
}

// #231: task-tracker fragment NOT appended when disabled/absent.
#[test]
fn task_tracker_fragment_absent_when_disabled() {
    let caps = llmenv::config::Capabilities {
        features: Some(llmenv::config::Features {
            task_tracker: Some(llmenv::config::TaskTracker { enabled: false }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let manifest = merge(&caps, &empty_native(), &[]).unwrap();
    let tmp = tempdir().unwrap();
    ClaudeCodeAdapter
        .materialize(&manifest, tmp.path())
        .expect("materialize");

    let claude_md = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).expect("read CLAUDE.md");
    assert!(
        !claude_md.contains("<!-- from task_tracker -->"),
        "CLAUDE.md must NOT contain task-tracker fragment when disabled"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test claude_code_adapter task_tracker_fragment`
Expected: FAIL — `llmenv::config::TaskTracker` doesn't exist yet in scope, or fragment text absent.

(If Task 1 already landed, `TaskTracker` exists — this will fail purely on the missing fragment/injection logic.)

- [ ] **Step 3: Implement the fragment and injection**

In `src/adapter/claude_code.rs`, add the fragment constant right after `COMPACT_SURVIVAL_FRAGMENT` (`src/adapter/claude_code.rs:63`):

```rust
/// #231: fragment appended to CLAUDE.md when the task tracker is enabled.
/// Steers the agent to use `llmenv task` for durable cross-session state.
const TASK_TRACKER_FRAGMENT: &str = concat!(
    "# Task Tracker\n",
    "\n",
    "This project has the llmenv task tracker enabled. Use it to record durable,\n",
    "cross-session state instead of relying on in-session TODOs:\n",
    "\n",
    "- `llmenv task add \"<title>\"` before starting new work.\n",
    "- `llmenv task start <slug>` to claim a task you're actively working on.\n",
    "- `llmenv task done <slug>` when it's finished.\n",
    "- `llmenv task add \"<title>\" --parent <slug>` for a sub-task instead of\n",
    "  abandoning the current task to start something unrelated.\n",
    "- `llmenv task note <slug> \"<text>\"` to record progress before a context\n",
    "  compaction or session end.\n",
    "\n",
    "If a session starts with `wip` tasks already recorded, resume or finish\n",
    "them before starting new top-level work.\n",
);
```

Extend the injection block in `materialize()` (`src/adapter/claude_code.rs:253-266`) — append a second, independent `if let` after the existing `slippage` one, both writing into the same `claude_md_content`:

```rust
        // #231: append task-tracker fragment when features.task_tracker.enabled.
        if let Some(tt) = manifest
            .capabilities
            .features
            .as_ref()
            .and_then(|f| f.task_tracker.as_ref())
            && tt.enabled
        {
            claude_md_content.push_str("\n\n<!-- from task_tracker -->\n");
            claude_md_content.push_str(TASK_TRACKER_FRAGMENT);
        }
```

Place it immediately after the existing `slippage`/`compact_survival` block and before `crate::paths::write_owner_only(&out.join("CLAUDE.md"), ...)` — both fragments append to the same growing `String` before the single write.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test claude_code_adapter task_tracker_fragment`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/adapter/claude_code.rs tests/claude_code_adapter.rs
git commit -m "feat(task-tracker): inject CLAUDE.md fragment when enabled

refs #231"
```

---

### Task 7: Lifecycle hooks — SessionStart reminder + Stop skip-detection

**Files:**
- Modify: `src/task/mod.rs` (add `session_start_reminder` and `stop_hook_reminder`)
- Modify: `src/hook_run/mod.rs` (wire both into `run_inner`)
- Modify: `tests/hook_run_failsoft.rs` (add `"stop"` to the fail-soft loop; add task-tracker-specific tests)

**Interfaces:**
- Consumes: `crate::task::list_tasks`, `TaskState`, `crate::config::TaskTracker`, `crate::paths::state_dir()`.
- Produces:
  - `pub fn session_start_reminder(state_dir: &Path) -> String` — infallible, empty string when no `wip` tasks or on any internal error (logged to stderr).
  - `pub fn stop_hook_reminder(state_dir: &Path) -> String` — infallible, same fail-soft contract. For this first cut, mirrors the SessionStart reminder's `wip`-task check (the "session touched a task" mtime heuristic from the design doc is deliberately deferred — see Step 3 note) rather than a full transcript/mtime heuristic.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/task/mod.rs`:

```rust
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
```

Also add `"stop"` to the loop in `tests/hook_run_failsoft.rs::all_events_fail_soft_without_backend` (line 258):

```rust
    for event in ["session_start", "turn_start", "session_end", "pre_tool_use", "stop"] {
```

And add two new fail-soft integration tests to `tests/hook_run_failsoft.rs`, after `pre_tool_use_with_read_once_deny_config_passes_through` (mirrors `config_with_read_once`'s shape — add a sibling `config_with_task_tracker` helper near it):

```rust
fn config_with_task_tracker() -> String {
    format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

bundle:
  - name: test-bundle
    when: [test]

features:
  task_tracker:
    enabled: true

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
    )
}

#[test]
fn session_start_with_task_tracker_enabled_passes_soft() {
    let (dir, config_path) = setup_config(&config_with_task_tracker());
    hook_cmd(dir.path(), &config_path, "session_start")
        .timeout(Duration::from_secs(10))
        .assert()
        .success();
}

#[test]
fn stop_with_task_tracker_enabled_exits_zero() {
    let (dir, config_path) = setup_config(&config_with_task_tracker());
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "test-stop",
        "last_assistant_message": "done for now",
    })
    .to_string();
    hook_cmd(dir.path(), &config_path, "stop")
        .write_stdin(payload.as_str())
        .timeout(Duration::from_secs(10))
        .assert()
        .success();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv task::tests::session_start_reminder task::tests::stop_hook_reminder`
Expected: FAIL — functions don't exist yet.

Run: `cargo test --test hook_run_failsoft stop`
Expected: `all_events_fail_soft_without_backend` still passes (Stop already fails-soft generically today via the early-exit at `src/hook_run/mod.rs:434-444`); the two new task-tracker tests fail to compile/run until Step 3 wires the config field through (should actually pass immediately once Task 1 lands, since Stop already fails-soft — re-run after Step 3 to confirm the *new* reminder logic doesn't regress that).

- [ ] **Step 3: Implement the reminder functions and hook wiring**

Append to `src/task/mod.rs`:

```rust
/// SessionStart hook: if any `wip` tasks exist, build a reminder nudging the
/// agent to resume or close them before starting new work. Empty string when
/// there are none, or on any internal error (logged to stderr, never
/// propagated — hooks must never block the agent).
pub fn session_start_reminder(state_dir: &Path) -> String {
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
    format!(
        "In-progress tasks from a previous session:\n{list}\n\
         Resume one of these or run `llmenv task done <slug>` before starting new work."
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
    format!(
        "You still have task(s) in progress:\n{list}\n\
         Run `llmenv task done <slug>` when finished, or `llmenv task note <slug> \"...\"` \
         to record progress before this session ends."
    )
}
```

In `src/hook_run/mod.rs`, wire `Stop` as an early special case right after the existing `read_once` block (`src/hook_run/mod.rs:415-425`), so it fires even when no session-log sink is configured (mirrors why `read_once` needed the same early placement — both must run before the `#702` early-exit at line 434):

```rust
    // #231: task-tracker Stop hook — runs before the #702 early-exit since it
    // doesn't need scope/memory resolution, same reasoning as read_once above.
    if event == HookEvent::Stop
        && let Some(ref features) = config.features
        && let Some(ref tt) = features.task_tracker
        && tt.enabled
    {
        let state_dir = crate::paths::state_dir()?;
        return Ok(crate::task::stop_hook_reminder(&state_dir));
    }
```

For `SessionStart`, append the reminder to `out` right before the final `Ok::<String, anyhow::Error>(out)` inside the `rt.block_on` async block (`src/hook_run/mod.rs:592`, i.e. immediately before that line, still inside the `async move { ... }` — this is synchronous file I/O but runs alongside the existing synchronous dedup-snapshot write at lines 586-590, so it's consistent with existing practice):

```rust
        // #231: append task-tracker SessionStart reminder, if enabled.
        if event == HookEvent::SessionStart
            && let Some(ref features) = config.features
            && let Some(ref tt) = features.task_tracker
            && tt.enabled
        {
            let state_dir = crate::paths::state_dir()?;
            let reminder = crate::task::session_start_reminder(&state_dir);
            if !reminder.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&reminder);
            }
        }

        Ok::<String, anyhow::Error>(out)
```

- [ ] **Step 4: Run tests to verify they pass**

Run:
```bash
cargo test -p llmenv task::tests
cargo test --test hook_run_failsoft
cargo test --test hook_run_failsoft -- --include-ignored stop
```
Expected: PASS across the board, including the full `all_events_fail_soft_without_backend` loop with `"stop"` added.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/task/mod.rs src/hook_run/mod.rs tests/hook_run_failsoft.rs
git commit -m "feat(task-tracker): SessionStart reminder and Stop skip-detection hooks

refs #231"
```

---

### Task 8: Docs + CHANGELOG

**Files:**
- Modify: `website/docs/commands.md` (new `## \`task\`` section, after the existing `## \`read-once\`` section at line 256-264)
- Modify: `CHANGELOG-3.md` (via the `keepachangelog` skill — do not hand-edit; invoke the skill)

**Interfaces:** None (docs-only; no code interface).

- [ ] **Step 1: Add the CLI reference section**

Insert into `website/docs/commands.md`, right after the `## \`read-once\`` section (before `## \`login\`` at line 266):

```markdown
## `task`

```text
llmenv task add <title> [--parent SLUG]
llmenv task start <id>
llmenv task done <id>
llmenv task ls [--format json]
llmenv task show <id>
llmenv task note <id> [text]
llmenv task block <id> --on <other>
```

In-engine task tracker (#231): durable, cross-session "what am I working on"
state, backed by one JSON file per task. `<id>` accepts an exact slug or any
unambiguous prefix of one.

- `task add <title> [--parent SLUG]` — create a task (`open` state); pass
  `--parent` to record it as a sub-task instead of starting unrelated
  top-level work.
- `task start <id>` — claim a task, moving it to `wip`.
- `task done <id>` — mark a task complete.
- `task ls [--format json]` — list all tasks; `--format json` for
  machine-readable output (stable schema, consumed by the SessionStart/Stop
  hooks below).
- `task show <id>` — full detail for one task (notes, parent, blockers).
- `task note <id> [text]` — append a progress note; reads from stdin if
  `text` is omitted.
- `task block <id> --on <other>` — record that `id` is blocked on `other`.

The CLI subcommands always work. The injected CLAUDE.md guidance and the
`SessionStart`/`Stop` lifecycle reminders (nudging the agent to resume or
close `wip` tasks) are gated behind `features.task_tracker.enabled` (default
`false`):

```yaml
features:
  task_tracker:
    enabled: true
```
```

- [ ] **Step 2: Add the CHANGELOG entry**

Invoke the `keepachangelog` skill to add an entry under `## [Unreleased]` → `### Added` in `CHANGELOG-3.md`, describing the shipped feature (task CLI, CLAUDE.md fragment, SessionStart/Stop hooks, `features.task_tracker`, issue `#231`). Follow the skill's own formatting guidance rather than hand-writing the entry here.

- [ ] **Step 3: Commit**

```bash
git add website/docs/commands.md CHANGELOG-3.md
git commit -m "docs(task-tracker): document llmenv task CLI and changelog entry

refs #231"
```

---

## Self-Review Notes (for whoever executes this plan)

- **Spec coverage:** Task 1 = schema/config switch (AC: "one config switch"). Task 2-3 = store + CLI (issue Scope §1, AC: slug/prefix addressing, transitions, corrupt-file tolerance, JSON output shape). Task 4 = CLI surface (issue Scope §1's verb list) + new-project guard (Phase 3 of the design doc). Task 5 = merge resolution (needed for multi-bundle setups, matches sibling features). Task 6 = injected context (issue Scope §2 / design doc Phase 2). Task 7 = hooks (issue Scope §3 / design doc Phase 3, both SessionStart cross-session pickup and Stop skip-detection). Task 8 = docs + changelog (design doc Phase 4). Out-of-scope items (GitHub sync, multi-agent locking, priorities/deadlines, TUI) are deliberately not tasked — matches the issue's own "Out of scope" section.
- **Deferred within scope, flagged not silently dropped:** the Stop hook's "only fire if *this session* touched the store" mtime heuristic is simplified to "fire on any remaining wip task" — documented inline in Task 7 Step 3 with a `ponytail:` comment naming the upgrade path, per the design doc's own allowance ("keep it a heuristic, note the ceiling").
- **Type consistency check:** `Task`/`TaskState`/`TaskNote` (Task 2) are the same types threaded through Task 3 (transitions), Task 4 (CLI), and Task 7 (hooks) — no renaming across tasks. `state_dir(&Path)`-taking functions consistently take `&Path` (not `PathBuf`) so tests can pass a `TempDir::path()` directly, matching the `read_once` module's own signatures.
