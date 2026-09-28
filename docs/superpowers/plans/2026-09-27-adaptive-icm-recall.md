# Adaptive ICM Recall Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task, inline in the current session.
> Never use superpowers:subagent-driven-development for this plan (user override: subagent work is not visible).
> Steps use checkbox (`- [ ]`) syntax for tracking.
> This plan is the implementation phase of `nbl-dev:ship-issue`; the review stage is that skill's `nbl-dev:pre-pr-review`.

**Goal:** Make ICM recall send each memory once per model context, pick per-turn memories from session activity, add related topics, inject on tool failure and subagent start, and deliver the `SessionStart` context that Claude Code discards today.

**Architecture:** A per-session JSON ledger (`session_ledger.rs`) records the hashes of injected records, a ring of tool activity, a ring of tool errors, and queued subagent tasks.
Pure functions (`relevance.rs`) turn those signals into recall queries and fanout targets.
An orchestration module (`adaptive.rs`) runs the recall waves concurrently, filters against the ledger, applies the byte budget, and records what it sent; `run_inner` calls it through one thin branch.

**Tech Stack:** Rust 1.95+ (`std::fs::File::try_lock`), tokio current-thread runtime (`tokio::join!`), `sha2` (already a dependency), `serde_json`, `wiremock` and `proptest` for tests.

**Spec:** `docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md`

**Issues:** #2249 (feature), #2251 (SessionStart context discarded).

## Global Constraints

- Base branch `release/3.x`; work branch `feat/2249-adaptive-icm-recall`.
- Per-call MCP timeout stays `HOOK_TIMEOUT = 2s` (`src/hook_run/mod.rs:137`).
- `TurnStart` budget 8,000 bytes (`RECALL_BUDGET_BYTES`); `PostToolUseFailure` budget 2,000 bytes; `SubagentStart` budget 4,000 bytes.
- Relevance query cap 600 characters; prompt cap 500; transcript tail cap 300; error head 300 bytes; subagent task head 600 characters.
- Rings: activity 20 entries, errors 5 entries, pending subagents 8 entries with a 5-minute TTL.
- Wave 1: main recall `limit: 10`, up to 2 keyword recalls `limit: 3`, one cross-project recall `limit: 3`; wave 2: up to 2 topic recalls `limit: 3`, skipped when wave 1 took more than 1.5 seconds.
- Ledger lock wait 200 ms, then skip the write and log.
- Config key `features.memory[].adaptive_recall`, bool, default `true`; `false` restores the stateless recall byte for byte.
- A hook never blocks or breaks a turn: every failure degrades to less context, never to an error on stdout.
- No new dependency.
- Code limits: functions at most 100 lines, complexity at most 8, at most 5 positional parameters, 100-character lines, comments in STE and only for a reason.
- Every commit passes `cargo fmt --check` and `cargo clippy --all-features --tests -- -D warnings` (pre-commit runs both).

## Review Focus

1. A `session_id` or `agent_id` with `/`, `..`, or an absolute path: the ledger must refuse it and fall back to stateless recall, never write outside `state_dir()` (Task 3 test `invalid_session_id_is_refused`, Task 7 test `invalid_agent_id_records_nothing`).
2. A truncated or hand-edited ledger JSON: the hook must treat it as a fresh epoch, not fail (Task 3 test `corrupt_ledger_loads_as_default`).
3. A prompt or error text with multibyte characters at the cap boundary: caps must cut on a character boundary, never panic (Task 4 property test `caps_never_split_a_char`).
4. An ICM backend that answers one call and times out on another: the answered records must still be injected (Task 7 test `one_failed_call_keeps_the_others`).
5. Two hooks that update one ledger at the same time: neither update may be lost (Task 3 test `concurrent_updates_are_not_lost`).

---

## File map

| File | Change | Responsibility |
| --- | --- | --- |
| `crates/llmenv-config/src/schema.rs` | modify | `Memory::adaptive_recall` field, default `true`. |
| `src/mcp/resolve.rs` | modify | Replace `ResolvedMcp::wakeup_max_tokens` with `memory_hook: Option<MemoryHookSettings>`. |
| `src/adapter/claude_code.rs` | modify | `SessionStart` envelope (#2251), new hook registrations, `CLAUDE_CODE_HOOK_EVENTS`. |
| `src/hook_run/session_ledger.rs` | create | Ledger types, record hash, locked load and update. |
| `src/hook_run/relevance.rs` | create | Pure query and fanout functions. |
| `src/hook_run/transcript.rs` | modify | `last_assistant_text`. |
| `src/hook_run/action.rs` | modify | `Action::RecallQuery(RecallQuery)`. |
| `src/hook_run/recall.rs` | modify | Budget limit, `sent` filter, kept hashes, counter rename. |
| `src/hook_run/adaptive.rs` | create | Event flows and concurrent waves. |
| `src/hook_run/mod.rs` | modify | New `HookEvent` variants, `scope_recall_actions`, thin wiring in `run_inner`. |
| `website/docs/configuration.md`, `website/docs/commands.md`, `CHANGELOG.md`, `docs/design/issue-2159-2141-icm-recall-prioritization.md` | modify | Docs and changelog. |

---

### Task 1: New hook events in the engine-neutral enum

**Files:**
- Modify: `src/hook_run/mod.rs:149-217` (enum, `FromStr`, `Display`), `:231-265` (`dispatch`), `:295-306` (`event_to_log_kind`), `:314-348` (`event_content`)
- Test: `src/hook_run/mod.rs` tests module

**Interfaces:**
- Produces: `HookEvent::PostToolBatch` (`"post_tool_batch"`), `HookEvent::PostToolUseFailure` (`"post_tool_use_failure"`), `HookEvent::SubagentStart` (`"subagent_start"`).
  `dispatch` returns `vec![]` for all three; `event_to_log_kind` returns `None`; `event_content` returns `(None, String::new())`.

- [ ] **Step 1: Write the failing test** (add to the tests module in `src/hook_run/mod.rs`)

```rust
#[test]
fn adaptive_recall_events_round_trip_through_their_names() {
    for (name, event) in [
        ("post_tool_batch", HookEvent::PostToolBatch),
        ("post_tool_use_failure", HookEvent::PostToolUseFailure),
        ("subagent_start", HookEvent::SubagentStart),
    ] {
        assert_eq!(name.parse::<HookEvent>().unwrap(), event);
        assert_eq!(event.to_string(), name);
        assert!(dispatch(event, &[], &[], &BTreeMap::new(), None).is_empty());
        assert_eq!(event_to_log_kind(event), None);
    }
}
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `cargo nextest run -p llmenv adaptive_recall_events_round_trip`
Expected: compile error, `no variant named PostToolBatch`.

- [ ] **Step 3: Add the variants**

In the enum, after `PreCompact`:

```rust
    /// A batch of tool calls resolved (Claude Code: `PostToolBatch`).
    PostToolBatch,
    /// A tool call failed (Claude Code: `PostToolUseFailure`).
    PostToolUseFailure,
    /// A subagent is spawned or resumed (Claude Code: `SubagentStart`).
    SubagentStart,
```

In `FromStr`, add the three arms and extend the error text:

```rust
            "post_tool_batch" => Ok(HookEvent::PostToolBatch),
            "post_tool_use_failure" => Ok(HookEvent::PostToolUseFailure),
            "subagent_start" => Ok(HookEvent::SubagentStart),
            other => Err(anyhow::anyhow!(
                "unknown hook event '{other}' (expected session_start|turn_start|session_end|\
                 user_prompt_submit|pre_tool_use|post_tool_use|notification|stop|\
                 subagent_stop|pre_compact|post_tool_batch|post_tool_use_failure|\
                 subagent_start)"
            )),
```

In `Display`:

```rust
            HookEvent::PostToolBatch => "post_tool_batch",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::SubagentStart => "subagent_start",
```

In `dispatch`, add the three variants to the arm that returns `vec![]`.
In `event_to_log_kind`, add them to the arm that returns `None`.
In `event_content`, add them to the arm that returns `(None, String::new())`.
Run `cargo check` and add the variants to any other exhaustive match that the compiler reports, with the same "no action" result.

- [ ] **Step 4: Run the test and confirm it passes**

Run: `cargo nextest run -p llmenv adaptive_recall_events_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/hook_run/mod.rs
git commit -m "feat(hook-run): add batch, failure, subagent-start events"
```

---

### Task 2: `adaptive_recall` config key and settings threading

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs` (`Memory` struct near line 1365 and its manual `Default`)
- Modify: `src/mcp/resolve.rs:44-48` (field), `:255` (memory entry), every `wakeup_max_tokens: None` in a `ResolvedMcp` literal
- Modify: `src/hook_run/mod.rs:1686-1781` (`MemoryEndpoint::Active`, accessors, `ResolvedMemoryClient`), `:1144`, `:1883-1893`
- Test: `crates/llmenv-config` schema tests, `src/mcp/resolve.rs` tests near line 535

**Interfaces:**
- Produces: `crate::mcp::resolve::MemoryHookSettings { pub wakeup_max_tokens: Option<u32>, pub adaptive_recall: bool }` (derives `Debug, Clone, Copy, PartialEq, Eq`).
- Produces: `ResolvedMcp::memory_hook: Option<MemoryHookSettings>` (replaces `wakeup_max_tokens`).
- Produces: `ResolvedMemoryClient { client: McpHttpClient, settings: MemoryHookSettings }` in `hook_run`.

- [ ] **Step 1: Write the failing tests**

In the schema tests:

```rust
#[test]
fn memory_adaptive_recall_defaults_to_true_and_accepts_false() {
    let on: Memory = serde_yaml::from_str("server_host: h\nport: 1\nwhen: [t]\n").unwrap();
    assert!(on.adaptive_recall);
    assert!(Memory::default().adaptive_recall);
    let off: Memory =
        serde_yaml::from_str("server_host: h\nport: 1\nwhen: [t]\nadaptive_recall: false\n")
            .unwrap();
    assert!(!off.adaptive_recall);
}
```

Use the YAML crate the schema tests already import; if the fields `server_host`, `port`, and `when` are not the required set, copy the minimal `Memory` YAML from an existing schema test.

In `src/mcp/resolve.rs` tests, change the test near line 535 to:

```rust
        mem.wakeup_max_tokens = Some(750);
        mem.adaptive_recall = false;
        // ...existing resolve call...
        assert_eq!(
            resolved[0].memory_hook,
            Some(MemoryHookSettings { wakeup_max_tokens: Some(750), adaptive_recall: false })
        );
```

and the assertion near line 544 to `assert_eq!(resolved[0].memory_hook, None);` for the non-memory server.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo nextest run --workspace memory_adaptive_recall_defaults resolve`
Expected: compile errors: no field `adaptive_recall`, no field `memory_hook`.

- [ ] **Step 3: Add the config field**

In `Memory`, after `wakeup_max_tokens`:

```rust
    /// Per-session adaptive recall (#2249). When `true`, the hooks send each memory once
    /// per model context and pick per-turn memories from session activity. `false`
    /// restores the stateless per-turn recall.
    #[serde(default = "default_adaptive_recall")]
    pub adaptive_recall: bool,
```

Add beside `default_listen_host`:

```rust
fn default_adaptive_recall() -> bool {
    true
}
```

In the manual `impl Default for Memory`, add `adaptive_recall: default_adaptive_recall(),`.

- [ ] **Step 4: Replace the resolved field**

In `src/mcp/resolve.rs`, replace the `wakeup_max_tokens` field and its doc comment with:

```rust
    /// Hook-time settings from the source `features.memory` entry for the ICM MCP
    /// (#1216, #2249). `None` for every other server. Consumed by `hook_run`'s live
    /// dispatch pipeline, not by static materialization.
    pub memory_hook: Option<MemoryHookSettings>,
```

and add the type above `ResolvedMcp`:

```rust
/// Settings that `hook_run` reads from the active `features.memory` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryHookSettings {
    /// Token budget for the `icm_wake_up` call (#1216).
    pub wakeup_max_tokens: Option<u32>,
    /// Whether adaptive recall is on (#2249).
    pub adaptive_recall: bool,
}
```

At line 255 replace `wakeup_max_tokens: mem.wakeup_max_tokens,` with:

```rust
        memory_hook: Some(MemoryHookSettings {
            wakeup_max_tokens: mem.wakeup_max_tokens,
            adaptive_recall: mem.adaptive_recall,
        }),
```

Run `cargo check --workspace --tests`.
For each error at a `ResolvedMcp` literal, replace `wakeup_max_tokens: None` with `memory_hook: None`.
For each error at a config `Memory` literal (missing field `adaptive_recall`), add `adaptive_recall: true`.
Repeat until `cargo check --workspace --tests` is clean.

- [ ] **Step 5: Thread the settings through `hook_run`**

Add `use crate::mcp::resolve::MemoryHookSettings;` to the imports of `src/hook_run/mod.rs`.
In `MemoryEndpoint::Active`, replace `wakeup_max_tokens: Option<u32>` with `settings: MemoryHookSettings` and update its doc comment.
Replace `wakeup_max_tokens()` and `into_url_and_wakeup_max_tokens()` with:

```rust
    /// The active entry's hook settings, or `None` for every non-active variant.
    fn settings(&self) -> Option<MemoryHookSettings> {
        match self {
            Self::Active { settings, .. } => Some(*settings),
            _ => None,
        }
    }

    /// Consume into `(url, settings)`, erroring exactly as [`Self::into_url`].
    fn into_url_and_settings(self) -> anyhow::Result<(String, Option<MemoryHookSettings>)> {
        let settings = self.settings();
        Ok((self.into_url()?, settings))
    }
```

At lines 1883-1893:

```rust
    let matched = resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some((url, m.memory_hook)),
        _ => None,
    });
    Ok(match matched {
        Some((url, settings)) => MemoryEndpoint::Active {
            url,
            settings: settings.unwrap_or(MemoryHookSettings {
                wakeup_max_tokens: None,
                adaptive_recall: true,
            }),
        },
```

`ResolvedMemoryClient` becomes `{ client: McpHttpClient, settings: MemoryHookSettings }`; in `resolve_memory_client`, call `into_url_and_settings` and build `settings` with the same `unwrap_or` default.
At line 1144: `let settings = resolved_client.as_ref().map(|r| r.settings);` and `let wakeup_max_tokens = settings.and_then(|s| s.wakeup_max_tokens);`.
Update the test at line 3173 to `assert_eq!(endpoint.settings().and_then(|s| s.wakeup_max_tokens), Some(750));`.

- [ ] **Step 6: Run the tests and confirm they pass**

Run: `cargo nextest run --workspace`
Expected: PASS, including the two new tests.

- [ ] **Step 7: Commit**

```bash
git add -A crates/llmenv-config src/mcp src/hook_run/mod.rs src/materialize src/merge src/cli
git status --short
git commit -m "feat(config): add features.memory[].adaptive_recall"
```

---

### Task 3: Session ledger

**Files:**
- Create: `src/hook_run/session_ledger.rs`
- Modify: `src/hook_run/mod.rs` (add `pub(crate) mod session_ledger;` beside the other `mod` lines)

**Interfaces:**
- Produces:
  - `pub(crate) const MAIN_AGENT: &str = "main";`
  - `pub(crate) fn record_hash(record: &str) -> String`
  - `pub(crate) fn unix_now() -> i64`
  - `pub(crate) struct Ledger` with `epoch`, `agents`, `activity`, `errors`, `last_query_hash`, `last_turn_at`, `pending_subagents` and methods `reset`, `sent_for`, `mark_sent`, `scope_sent`, `set_scope_sent`, `push_activity`, `push_error`, `queue_subagent`, `take_subagent`, `activity`, `errors`, `activity_since`.
  - `pub(crate) struct Activity { pub(crate) tool: String, pub(crate) target: Option<String>, pub(crate) at: i64 }`
  - `pub(crate) struct ToolError { pub(crate) tool: String, pub(crate) head: String, pub(crate) at: i64 }`
  - `pub(crate) struct PendingSubagent { pub(crate) tool_use_id: String, pub(crate) subagent_type: String, pub(crate) task: String, pub(crate) at: i64 }`
  - `pub(crate) struct LedgerStore` with `new(state_dir: &Path) -> Self`, `load(&self, session_id: &str) -> Option<Ledger>`, `update<T>(&self, session_id: &str, f: impl FnOnce(&mut Ledger) -> T) -> Option<T>`.

- [ ] **Step 1: Write the failing tests**

Create `src/hook_run/session_ledger.rs` with only the test module below, and add the line `pub(crate) mod session_ledger;` in `mod.rs`.
The workspace lints deny `todo!()`, so the tests fail to compile until Step 3 adds the implementation.

```rust
#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, LedgerStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn record_hash_ignores_whitespace_layout() {
        assert_eq!(record_hash("[t] a  b\n c"), record_hash("[t] a b c"));
        assert_ne!(record_hash("[t] a"), record_hash("[u] a"));
    }

    #[test]
    fn missing_ledger_loads_as_default() {
        let (_dir, store) = store();
        assert_eq!(store.load("s1"), Some(Ledger::default()));
    }

    #[test]
    fn corrupt_ledger_loads_as_default() {
        let (dir, store) = store();
        let path = dir.path().join("recall_session").join("s1.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"epoch\": 3, \"agents\": ").unwrap();
        assert_eq!(store.load("s1"), Some(Ledger::default()));
    }

    #[test]
    fn invalid_session_id_is_refused() {
        let (dir, store) = store();
        for id in ["../x", "/abs", "..", "", "a/b"] {
            assert_eq!(store.load(id), None, "{id}");
            assert_eq!(store.update(id, |_| ()), None, "{id}");
        }
        assert!(!dir.path().join("x.json").exists());
    }

    #[test]
    fn update_persists_and_reset_clears_agents_only() {
        let (_dir, store) = store();
        store.update("s1", |l| {
            l.mark_sent(MAIN_AGENT, ["h1".to_string()]);
            l.set_scope_sent(MAIN_AGENT);
            l.push_activity(Activity { tool: "Read".into(), target: None, at: 1 });
        });
        let before = store.load("s1").unwrap();
        assert!(before.sent_for(MAIN_AGENT).contains("h1"));
        assert!(before.scope_sent(MAIN_AGENT));
        store.update("s1", Ledger::reset);
        let after = store.load("s1").unwrap();
        assert_eq!(after.epoch, before.epoch + 1);
        assert!(after.sent_for(MAIN_AGENT).is_empty());
        assert!(!after.scope_sent(MAIN_AGENT));
        assert_eq!(after.activity().len(), 1, "activity survives a reset");
    }

    #[test]
    fn sent_sets_are_kept_per_agent() {
        let mut l = Ledger::default();
        l.mark_sent("agent-1", ["h".to_string()]);
        assert!(l.sent_for("agent-1").contains("h"));
        assert!(l.sent_for(MAIN_AGENT).is_empty());
    }

    #[test]
    fn rings_keep_only_the_newest_entries() {
        let mut l = Ledger::default();
        for i in 0..25 {
            l.push_activity(Activity { tool: format!("t{i}"), target: None, at: i });
            l.push_error("Bash", &format!("Exit code 1\nerr {i}"), i);
        }
        assert_eq!(l.activity().len(), ACTIVITY_CAP);
        assert_eq!(l.activity().front().unwrap().tool, "t5");
        assert_eq!(l.errors().len(), ERRORS_CAP);
        assert_eq!(l.errors().back().unwrap().head, "Exit code 1\nerr 24");
    }

    #[test]
    fn error_head_is_capped_on_a_char_boundary() {
        let mut l = Ledger::default();
        l.push_error("Bash", &"é".repeat(400), 1);
        let head = &l.errors().back().unwrap().head;
        assert!(head.len() <= ERROR_HEAD_BYTES);
        assert!(head.chars().all(|c| c == 'é'));
    }

    #[test]
    fn subagent_queue_dedups_by_tool_use_id_and_takes_oldest_match() {
        let mut l = Ledger::default();
        l.queue_subagent("u1", "Explore", "find the parser", 100);
        l.queue_subagent("u1", "Explore", "find the parser", 101);
        l.queue_subagent("u2", "Plan", "plan it", 102);
        l.queue_subagent("u3", "Explore", "second task", 103);
        assert_eq!(l.take_subagent("Explore", 110).unwrap().task, "find the parser");
        assert_eq!(l.take_subagent("Explore", 110).unwrap().task, "second task");
        assert!(l.take_subagent("Explore", 110).is_none());
        assert!(l.take_subagent("Plan", 100 + PENDING_TTL_SECS + 5).is_none(), "expired");
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let (_dir, store) = store();
        let store = std::sync::Arc::new(store);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    store.update("s1", |l| l.mark_sent(MAIN_AGENT, [format!("h{i}")]))
                })
            })
            .collect();
        let written = handles.into_iter().filter_map(|h| h.join().unwrap()).count();
        let ledger = store.load("s1").unwrap();
        assert_eq!(ledger.sent_for(MAIN_AGENT).len(), written);
        assert!(written >= 1);
    }
}
```

`concurrent_updates_are_not_lost` accepts a skipped write (lock timeout returns `None`), but every write that reports success must be present.

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo nextest run -p llmenv session_ledger`
Expected: compile errors for the missing items.

- [ ] **Step 3: Write the implementation** (above the test module)

```rust
//! Per-session recall ledger for adaptive ICM recall (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The agent key for the parent conversation.
pub(crate) const MAIN_AGENT: &str = "main";
const ACTIVITY_CAP: usize = 20;
const ERRORS_CAP: usize = 5;
const PENDING_CAP: usize = 8;
/// A queued task that no `SubagentStart` took within this time is stale.
const PENDING_TTL_SECS: i64 = 300;
const ERROR_HEAD_BYTES: usize = 300;
const TASK_HEAD_CHARS: usize = 600;
/// A hook must not stall the agent on a busy ledger, so a write gives up after this wait.
const LOCK_WAIT: Duration = Duration::from_millis(200);
const LOCK_POLL: Duration = Duration::from_millis(10);
const STALE_DAYS: u64 = 7;
/// Hex characters kept from the SHA-256 digest; 64 bits is enough to key one session.
const HASH_HEX_CHARS: usize = 16;

/// One injected-record set per model context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct AgentState {
    sent: BTreeSet<String>,
    scope_sent: bool,
}

/// One tool call, reduced to the part that carries a topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Activity {
    pub(crate) tool: String,
    pub(crate) target: Option<String>,
    pub(crate) at: i64,
}

/// One failed tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolError {
    pub(crate) tool: String,
    pub(crate) head: String,
    pub(crate) at: i64,
}

/// A subagent task seen on the `Agent` tool call, waiting for its `SubagentStart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingSubagent {
    pub(crate) tool_use_id: String,
    pub(crate) subagent_type: String,
    pub(crate) task: String,
    pub(crate) at: i64,
}

/// The recall state of one Claude Code session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Ledger {
    pub(crate) epoch: u64,
    agents: BTreeMap<String, AgentState>,
    activity: VecDeque<Activity>,
    errors: VecDeque<ToolError>,
    pub(crate) last_query_hash: Option<String>,
    pub(crate) last_turn_at: i64,
    pending_subagents: VecDeque<PendingSubagent>,
}

impl Ledger {
    /// Start a new epoch: every context forgets what it was sent. Activity stays.
    pub(crate) fn reset(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.agents.clear();
    }

    /// The record hashes already injected into `agent`.
    pub(crate) fn sent_for(&self, agent: &str) -> BTreeSet<String> {
        self.agents.get(agent).map(|a| a.sent.clone()).unwrap_or_default()
    }

    pub(crate) fn mark_sent(&mut self, agent: &str, hashes: impl IntoIterator<Item = String>) {
        self.agents.entry(agent.to_string()).or_default().sent.extend(hashes);
    }

    pub(crate) fn scope_sent(&self, agent: &str) -> bool {
        self.agents.get(agent).is_some_and(|a| a.scope_sent)
    }

    pub(crate) fn set_scope_sent(&mut self, agent: &str) {
        self.agents.entry(agent.to_string()).or_default().scope_sent = true;
    }

    pub(crate) fn activity(&self) -> &VecDeque<Activity> {
        &self.activity
    }

    pub(crate) fn errors(&self) -> &VecDeque<ToolError> {
        &self.errors
    }

    /// Whether any activity arrived after `at`.
    pub(crate) fn activity_since(&self, at: i64) -> bool {
        self.activity.back().is_some_and(|a| a.at > at)
    }

    pub(crate) fn push_activity(&mut self, entry: Activity) {
        push_capped(&mut self.activity, entry, ACTIVITY_CAP);
    }

    pub(crate) fn push_error(&mut self, tool: &str, error: &str, now: i64) {
        let entry = ToolError { tool: tool.to_string(), head: head_bytes(error, ERROR_HEAD_BYTES), at: now };
        push_capped(&mut self.errors, entry, ERRORS_CAP);
    }

    /// Queue a subagent task. A repeat of the same `tool_use_id` is ignored.
    pub(crate) fn queue_subagent(&mut self, tool_use_id: &str, subagent_type: &str, prompt: &str, now: i64) {
        self.drop_expired(now);
        if self.pending_subagents.iter().any(|p| p.tool_use_id == tool_use_id) {
            return;
        }
        let entry = PendingSubagent {
            tool_use_id: tool_use_id.to_string(),
            subagent_type: subagent_type.to_string(),
            task: prompt.chars().take(TASK_HEAD_CHARS).collect(),
            at: now,
        };
        push_capped(&mut self.pending_subagents, entry, PENDING_CAP);
    }

    /// Remove and return the oldest queued task for `agent_type`.
    pub(crate) fn take_subagent(&mut self, agent_type: &str, now: i64) -> Option<PendingSubagent> {
        self.drop_expired(now);
        let index = self.pending_subagents.iter().position(|p| p.subagent_type == agent_type)?;
        self.pending_subagents.remove(index)
    }

    fn drop_expired(&mut self, now: i64) {
        self.pending_subagents.retain(|p| now.saturating_sub(p.at) <= PENDING_TTL_SECS);
    }
}

fn push_capped<T>(ring: &mut VecDeque<T>, entry: T, cap: usize) {
    ring.push_back(entry);
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// The longest prefix of `text` that fits in `max` bytes and ends on a char boundary.
fn head_bytes(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default().to_string()
}

/// A stable key for one recall record: whitespace layout does not change it.
pub(crate) fn record_hash(record: &str) -> String {
    let normalized = record.split_whitespace().collect::<Vec<_>>().join(" ");
    Sha256::digest(normalized.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(HASH_HEX_CHARS)
        .collect()
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// The ledger files under `state_dir()/recall_session/`.
#[derive(Debug, Clone)]
pub(crate) struct LedgerStore {
    dir: PathBuf,
}

impl LedgerStore {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self { dir: state_dir.join("recall_session") }
    }

    /// Read the ledger. `None` only for an unsafe `session_id` or a busy lock.
    pub(crate) fn load(&self, session_id: &str) -> Option<Ledger> {
        let _lock = self.lock(session_id)?;
        Some(self.read(session_id))
    }

    /// Apply `f` under the lock and save. `None` for an unsafe `session_id` or a busy lock.
    /// A failed save is logged; the result of `f` is still returned.
    pub(crate) fn update<T>(&self, session_id: &str, f: impl FnOnce(&mut Ledger) -> T) -> Option<T> {
        let _lock = self.lock(session_id)?;
        let mut ledger = self.read(session_id);
        let result = f(&mut ledger);
        self.write(session_id, &ledger);
        Some(result)
    }

    fn file(&self, session_id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.{ext}"))
    }

    fn lock(&self, session_id: &str) -> Option<File> {
        // The id comes from hook stdin; an unsafe value must never reach a path join.
        if !crate::paths::is_valid_short_name(session_id) {
            tracing::error!("session_id failed path-safety validation for recall ledger");
            return None;
        }
        if let Err(e) = crate::paths::create_dir_owner_only(&self.dir) {
            tracing::error!("cannot create {}: {e}", self.dir.display());
            return None;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.file(session_id, "lock"))
            .inspect_err(|e| tracing::error!("cannot open recall ledger lock: {e}"))
            .ok()?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Some(file),
                Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(LOCK_POLL);
                }
                Err(e) => {
                    tracing::warn!("recall ledger busy or unlockable, write skipped: {e}");
                    return None;
                }
            }
        }
    }

    fn read(&self, session_id: &str) -> Ledger {
        let path = self.file(session_id, "json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("recall ledger {} is corrupt, starting fresh: {e}", path.display());
                Ledger::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => {
                tracing::warn!("cannot read recall ledger {}: {e}", path.display());
                Ledger::default()
            }
        }
    }

    fn write(&self, session_id: &str, ledger: &Ledger) {
        super::session_state::prune_stale_json_files(&self.dir, STALE_DAYS);
        let path = self.file(session_id, "json");
        let result = serde_json::to_vec(ledger)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::paths::write_owner_only_atomic(&path, &bytes));
        if let Err(e) = result {
            tracing::error!("cannot save recall ledger {}: {e}", path.display());
        }
    }
}
```

`prune_stale_json_files` only removes `.json` files, so the `.lock` files of pruned sessions stay; add the removal of a `.lock` file whose `.json` is gone in the same function call site:

```rust
    fn prune_orphan_locks(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().and_then(|e| e.to_str()) == Some("lock")
                && !path.with_extension("json").exists()
            {
                let _ignored = std::fs::remove_file(&path);
            }
        }
    }
```

and call `self.prune_orphan_locks();` right after `prune_stale_json_files` in `write`.
A lock file whose session is still active is recreated on its next update, so the removal is safe.

Add the test:

```rust
    #[test]
    fn orphan_lock_files_are_removed_on_write() {
        let (dir, store) = store();
        let lock = dir.path().join("recall_session").join("gone.lock");
        store.update("s1", |_| ());
        std::fs::write(&lock, "").unwrap();
        store.update("s1", |_| ());
        assert!(!lock.exists());
        assert!(dir.path().join("recall_session").join("s1.lock").exists());
    }
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo nextest run -p llmenv session_ledger`
Expected: PASS (11 tests).

- [ ] **Step 5: Break the code to prove a test catches it, then restore**

Change `push_capped`'s `while ring.len() > cap` to `if ring.len() > cap + 1`.
Run: `cargo nextest run -p llmenv rings_keep_only_the_newest_entries` — expected FAIL.
Restore the line and run again — expected PASS.

- [ ] **Step 6: Commit**

```bash
git add src/hook_run/session_ledger.rs src/hook_run/mod.rs
git commit -m "feat(hook-run): add per-session recall ledger"
```

---

### Task 4: Relevance functions and the transcript tail

**Files:**
- Create: `src/hook_run/relevance.rs`
- Modify: `src/hook_run/transcript.rs` (add `last_assistant_text`)
- Modify: `src/hook_run/mod.rs` (add `mod relevance;`)

**Interfaces:**
- Consumes: `session_ledger::{Activity, ToolError}` (Task 3).
- Produces:
  - `pub(crate) struct TurnSignals<'a> { pub(crate) prompt: &'a str, pub(crate) activity: &'a VecDeque<Activity>, pub(crate) errors: &'a VecDeque<ToolError>, pub(crate) last_turn_at: i64, pub(crate) assistant_tail: Option<&'a str> }`
  - `pub(crate) fn turn_query(signals: &TurnSignals<'_>) -> String`
  - `pub(crate) fn activity_terms(activity: &VecDeque<Activity>) -> Vec<String>`
  - `pub(crate) fn fanout_keywords(activity: &VecDeque<Activity>) -> Vec<String>`
  - `pub(crate) fn sibling_topics(hit_topics: &[String]) -> Vec<String>`
  - `pub(crate) fn record_topic(record: &str) -> Option<&str>`
  - `pub(crate) fn activity_from_tool_call(tool_name: &str, tool_input: &serde_json::Value, at: i64) -> Activity`
  - `pub(crate) fn error_query(tool: &str, error: &str) -> String`
  - `pub(crate) fn subagent_query(task: Option<&str>, agent_type: &str, activity: &VecDeque<Activity>) -> String`
  - `pub(crate) const QUERY_CAP: usize = 600;`
  - `transcript::last_assistant_text(path: &Path, max_chars: usize) -> Option<String>`

- [ ] **Step 1: Write the failing tests** (test module at the bottom of `relevance.rs`; create the file with only `use` lines and the module, plus `mod relevance;` in `mod.rs`)

```rust
#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use std::collections::VecDeque;

    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::hook_run::session_ledger::{Activity, ToolError};

    fn act(tool: &str, target: Option<&str>, at: i64) -> Activity {
        Activity { tool: tool.into(), target: target.map(Into::into), at }
    }

    #[test]
    fn activity_from_tool_call_reads_paths_and_command_names() {
        let read = activity_from_tool_call("Read", &json!({"file_path": "/r/src/hook_run/recall.rs"}), 1);
        assert_eq!(read.target.as_deref(), Some("/r/src/hook_run/recall.rs"));
        let bash = activity_from_tool_call("Bash", &json!({"command": "RUST_LOG=x cargo nextest run"}), 1);
        assert_eq!(bash.target.as_deref(), Some("cargo"));
        let other = activity_from_tool_call("WebSearch", &json!({"query": "q"}), 1);
        assert_eq!(other.target, None);
    }

    #[test]
    fn activity_terms_are_newest_first_distinct_and_skip_generic_dirs() {
        let ring: VecDeque<_> = [
            act("Read", Some("/r/src/hook_run/recall.rs"), 1),
            act("Bash", Some("cargo"), 2),
            act("Edit", Some("/r/crates/llmenv-config/src/schema.rs"), 3),
        ]
        .into();
        assert_eq!(
            activity_terms(&ring),
            ["schema", "llmenv-config", "cargo", "recall", "hook_run"]
        );
        assert_eq!(fanout_keywords(&ring), ["schema", "llmenv-config"]);
    }

    #[test]
    fn turn_query_combines_prompt_terms_new_error_and_tail() {
        let activity: VecDeque<_> = [act("Bash", Some("git"), 5)].into();
        let errors: VecDeque<_> =
            [ToolError { tool: "Bash".into(), head: "Exit code 1\nmerge conflict".into(), at: 9 }].into();
        let q = turn_query(&TurnSignals {
            prompt: "continue",
            activity: &activity,
            errors: &errors,
            last_turn_at: 8,
            assistant_tail: Some("resolving the CI yaml"),
        });
        assert_eq!(q, "continue git merge conflict resolving the CI yaml");
        let old = turn_query(&TurnSignals { last_turn_at: 10, ..TurnSignals {
            prompt: "continue", activity: &activity, errors: &errors, last_turn_at: 0, assistant_tail: None,
        }});
        assert_eq!(old, "continue git", "an error older than the last turn is left out");
    }

    #[test]
    fn sibling_topics_map_canonical_names_and_skip_hits() {
        let hits = vec!["context-llmenv".to_string(), "preferences".to_string()];
        assert_eq!(sibling_topics(&hits), ["decisions-llmenv", "errors-resolved"]);
        let both = vec!["context-x".to_string(), "decisions-x".to_string()];
        assert_eq!(sibling_topics(&both), ["errors-resolved"]);
        assert!(sibling_topics(&["preferences".to_string()]).is_empty());
    }

    #[test]
    fn record_topic_reads_the_bracket_prefix() {
        assert_eq!(record_topic("[context-llmenv] text"), Some("context-llmenv"));
        assert_eq!(record_topic("no topic"), None);
    }

    #[test]
    fn error_query_drops_the_exit_code_line() {
        assert_eq!(
            error_query("Bash", "Exit code 101\nerror[E0063]: missing field `adaptive_recall`"),
            "Bash error[E0063]: missing field `adaptive_recall`"
        );
    }

    #[test]
    fn subagent_query_prefers_the_task_and_falls_back_to_type_and_terms() {
        let ring: VecDeque<_> = [act("Bash", Some("gh"), 1)].into();
        assert_eq!(subagent_query(Some("map the recall path"), "Explore", &ring), "map the recall path");
        assert_eq!(subagent_query(None, "Explore", &ring), "Explore gh");
    }

    proptest! {
        #[test]
        fn caps_never_split_a_char(prompt in "\\PC{0,900}", tail in "\\PC{0,900}") {
            let empty = VecDeque::new();
            let errors = VecDeque::new();
            let q = turn_query(&TurnSignals {
                prompt: &prompt, activity: &empty, errors: &errors, last_turn_at: 0,
                assistant_tail: Some(&tail),
            });
            prop_assert!(q.chars().count() <= QUERY_CAP);
            prop_assert_eq!(q.clone(), turn_query(&TurnSignals {
                prompt: &prompt, activity: &empty, errors: &errors, last_turn_at: 0,
                assistant_tail: Some(&tail),
            }));
        }
    }
}
```

In `transcript.rs` tests, add (reuse the existing `transcript` helper):

```rust
    #[test]
    fn last_assistant_text_returns_the_newest_visible_text_capped() {
        let file = transcript(&[
            serde_json::json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "old"}]}}),
            serde_json::json!({"message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "x"}]}}),
            serde_json::json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "newest reply"}]}}),
            serde_json::json!({"message": {"role": "user", "content": [{"type": "tool_result", "content": "r"}]}}),
        ]);
        assert_eq!(last_assistant_text(file.path(), 6).as_deref(), Some("newest"));
        assert_eq!(last_assistant_text(std::path::Path::new("/nonexistent"), 6), None);
    }
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo nextest run -p llmenv relevance last_assistant_text`
Expected: compile errors for the missing functions.

- [ ] **Step 3: Write `relevance.rs`**

```rust
//! Pure functions that turn session signals into ICM recall queries (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::VecDeque;
use std::path::Path;

use crate::hook_run::session_ledger::{Activity, ToolError};

pub(crate) const QUERY_CAP: usize = 600;
const PROMPT_CAP: usize = 500;
const TAIL_CAP: usize = 300;
const ACTIVITY_WINDOW: usize = 10;
const FANOUT_KEYWORDS: usize = 2;
const MAX_SIBLINGS: usize = 2;
/// Directory names too common to say anything about the topic of the work.
const GENERIC_DIRS: &[&str] = &["src", "tests", "test", "lib", "docs", "crates", "."];

/// What a `TurnStart` knows about the session.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnSignals<'a> {
    pub(crate) prompt: &'a str,
    pub(crate) activity: &'a VecDeque<Activity>,
    pub(crate) errors: &'a VecDeque<ToolError>,
    pub(crate) last_turn_at: i64,
    pub(crate) assistant_tail: Option<&'a str>,
}

fn cap_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect::<String>().trim().to_string()
}

fn join_capped(parts: &[String]) -> String {
    let joined = parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join(" ");
    cap_chars(&joined, QUERY_CAP)
}

/// Reduce a tool call to the part that names a topic: a file path or a command name.
pub(crate) fn activity_from_tool_call(tool_name: &str, tool_input: &serde_json::Value, at: i64) -> Activity {
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| tool_input.get(key).and_then(serde_json::Value::as_str));
    let command = tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .and_then(|c| c.split_whitespace().find(|word| !word.contains('=')))
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str());
    Activity { tool: tool_name.to_string(), target: path.or(command).map(str::to_string), at }
}

/// Topic terms from the newest activity: file stems, their directory, command names.
pub(crate) fn activity_terms(activity: &VecDeque<Activity>) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut add = |term: &str| {
        if !term.is_empty() && !GENERIC_DIRS.contains(&term) && !terms.iter().any(|t| t == term) {
            terms.push(term.to_string());
        }
    };
    for entry in activity.iter().rev().take(ACTIVITY_WINDOW) {
        let Some(target) = entry.target.as_deref() else { continue };
        let path = Path::new(target);
        match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) if target.contains('/') => {
                add(stem);
                let parent = path.parent().and_then(Path::file_name).and_then(|p| p.to_str());
                let parent = match parent {
                    Some(p) if GENERIC_DIRS.contains(&p) => path
                        .parent()
                        .and_then(Path::parent)
                        .and_then(Path::file_name)
                        .and_then(|p| p.to_str()),
                    other => other,
                };
                parent.into_iter().for_each(&mut add);
            }
            _ => add(target),
        }
    }
    terms
}

/// The activity terms used as exact-match `keyword` filters.
pub(crate) fn fanout_keywords(activity: &VecDeque<Activity>) -> Vec<String> {
    activity_terms(activity).into_iter().take(FANOUT_KEYWORDS).collect()
}

/// The per-turn relevance query.
pub(crate) fn turn_query(signals: &TurnSignals<'_>) -> String {
    let fresh_error = signals
        .errors
        .back()
        .filter(|e| e.at > signals.last_turn_at)
        .map(|e| strip_exit_code(&e.head))
        .unwrap_or_default();
    join_capped(&[
        cap_chars(signals.prompt, PROMPT_CAP),
        activity_terms(signals.activity).join(" "),
        fresh_error,
        cap_chars(signals.assistant_tail.unwrap_or_default(), TAIL_CAP),
    ])
}

/// The `[topic]` of one recall record.
pub(crate) fn record_topic(record: &str) -> Option<&str> {
    let (topic, _) = record.strip_prefix('[')?.split_once(']')?;
    (!topic.is_empty()).then_some(topic)
}

/// Sibling topics of the canonical `context-X` / `decisions-X` names, plus `errors-resolved`.
pub(crate) fn sibling_topics(hit_topics: &[String]) -> Vec<String> {
    let mut siblings: Vec<String> = Vec::new();
    let mut matched = false;
    for topic in hit_topics {
        let sibling = if let Some(project) = topic.strip_prefix("context-") {
            Some(format!("decisions-{project}"))
        } else {
            topic.strip_prefix("decisions-").map(|project| format!("context-{project}"))
        };
        matched |= sibling.is_some();
        siblings.extend(sibling);
    }
    if matched {
        siblings.push("errors-resolved".to_string());
    }
    let mut unique: Vec<String> = Vec::new();
    for topic in siblings {
        if !hit_topics.contains(&topic) && !unique.contains(&topic) {
            unique.push(topic);
        }
    }
    unique.into_iter().take(MAX_SIBLINGS).collect()
}

/// The `Exit code N` line names no topic, so it is dropped.
fn strip_exit_code(error: &str) -> String {
    error
        .lines()
        .filter(|line| !line.starts_with("Exit code "))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The recall query for one failed tool call.
pub(crate) fn error_query(tool: &str, error: &str) -> String {
    join_capped(&[tool.to_string(), strip_exit_code(error)])
}

/// The recall query for a new subagent.
pub(crate) fn subagent_query(task: Option<&str>, agent_type: &str, activity: &VecDeque<Activity>) -> String {
    match task {
        Some(task) => cap_chars(task, QUERY_CAP),
        None => join_capped(&[agent_type.to_string(), activity_terms(activity).join(" ")]),
    }
}
```

If `activity_terms` exceeds complexity 8 under `cargo clippy`, extract the parent-directory choice into `fn topic_dir(path: &Path) -> Option<&str>` with the same logic.

- [ ] **Step 4: Add `last_assistant_text` to `transcript.rs`**

```rust
/// The newest assistant text in the tail of `path`, capped at `max_chars`.
///
/// Returns `None` when the transcript can't be read or holds no assistant text.
pub(crate) fn last_assistant_text(path: &Path, max_chars: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines.len().saturating_sub(TAIL_LINES);
    lines.get(tail..)?.iter().rev().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        let message = entry.get("message")?;
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            return None;
        }
        user_text(message).map(|t| t.chars().take(max_chars).collect())
    })
}
```

`user_text` already extracts the `text` blocks of a message, whatever its role; its name refers to the original caller.

- [ ] **Step 5: Run the tests and confirm they pass**

Run: `cargo nextest run -p llmenv relevance last_assistant_text`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/hook_run/relevance.rs src/hook_run/transcript.rs src/hook_run/mod.rs
git commit -m "feat(hook-run): build recall queries from session signals"
```

---

### Task 5: Budget filter, kept hashes, and the query action

**Files:**
- Modify: `src/hook_run/action.rs` (new `RecallQuery` type and `Action::RecallQuery` variant)
- Modify: `src/hook_run/recall.rs` (limit, `sent` filter, hashes, counter rename, `run_with_budget_filtered`)
- Modify: `src/hook_run/mod.rs:231-254` (extract `scope_recall_actions`)

**Interfaces:**
- Consumes: `session_ledger::record_hash` (Task 3).
- Produces:
  - `pub struct RecallQuery { pub query: String, pub topic: Option<String>, pub keyword: Option<String>, pub project: Option<String>, pub limit: u8 }` and `Action::RecallQuery(RecallQuery)` (tool `icm_memory_recall`).
  - `RecallBudget::new(limit: usize, sent: BTreeSet<String>) -> Self`, `add_text(&mut self, text: &str)`, `kept_hashes(&self) -> Vec<String>`, `render(&self, passthrough: Vec<String>) -> String`, `is_full(&self) -> bool` (now `pub(super)`).
  - `pub(super) const RECALL_BUDGET_BYTES: usize = 8_000;` (now `pub(super)`).
  - `pub(super) async fn run_with_budget_filtered(actions, budget: RecallBudget, run) -> anyhow::Result<(String, RecallBudget)>`.
  - `fn scope_recall_actions(tag_queries, bundle_queries, ranks) -> Vec<Action>` in `mod.rs`.

- [ ] **Step 1: Write the failing tests**

In `action.rs` tests:

```rust
    #[test]
    fn recall_query_sends_only_the_fields_it_has() {
        let full = Action::RecallQuery(RecallQuery {
            query: "q".into(),
            topic: Some("errors-resolved".into()),
            keyword: Some("cargo".into()),
            project: Some(String::new()),
            limit: 3,
        });
        assert_eq!(full.tool_name(), "icm_memory_recall");
        assert_eq!(
            full.arguments("ignored", "ignored"),
            json!({"query": "q", "topic": "errors-resolved", "keyword": "cargo", "project": "", "limit": 3})
        );
        let bare = Action::RecallQuery(RecallQuery {
            query: "q".into(), topic: None, keyword: None, project: None, limit: 10,
        });
        assert_eq!(bare.arguments("", ""), json!({"query": "q", "limit": 10}));
    }
```

In `recall.rs` tests:

```rust
    #[test]
    fn sent_records_are_skipped_and_counted() {
        let sent: BTreeSet<String> =
            [crate::hook_run::session_ledger::record_hash("[t] old")].into();
        let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, sent);
        budget.add_text("[t] old\n[t] new");
        assert_eq!(budget.kept(), ["[t] new"]);
        assert_eq!(budget.already_sent, 1);
        assert_eq!(
            budget.kept_hashes(),
            [crate::hook_run::session_ledger::record_hash("[t] new")]
        );
    }

    #[test]
    fn a_smaller_limit_is_respected() {
        let mut budget = RecallBudget::new(2_000, BTreeSet::new());
        budget.add_records(vec![record_of(1_500), record_of(1_500)]);
        assert_eq!(budget.kept().len(), 1);
        assert_eq!(budget.omitted, 1);
    }
```

Change the expected string in `trace_line_reports_every_counter` to:

```rust
            Some(
                "[LLMENV_CONTEXT] recall_entries=3 recall_bytes=16 injected_entries=2 \
                 injected_bytes=11 duplicate_records=1 already_sent=0 omitted=0 skipped_actions=0"
            )
```

In `mod.rs` tests:

```rust
    #[test]
    fn scope_recall_actions_is_turn_start_without_the_final_recall() {
        let tags = tag_recall_queries(&["a".to_string()]).unwrap();
        let turn = dispatch(HookEvent::TurnStart, &tags, &[], &BTreeMap::new(), None);
        let scope = scope_recall_actions(&tags, &[], &BTreeMap::new());
        assert_eq!(turn.last(), Some(&Action::Recall));
        assert_eq!(scope, turn[..turn.len() - 1]);
    }
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo nextest run -p llmenv recall_query_sends sent_records_are_skipped a_smaller_limit trace_line_reports scope_recall_actions`
Expected: compile errors, then the trace-line assertion failure.

- [ ] **Step 3: Add the action**

In `action.rs`, above `Action`:

```rust
/// One adaptive recall call (#2249). `None` fields are left out of the tool call, so ICM
/// applies its own default (for `project`, the server's cwd project filter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallQuery {
    pub query: String,
    pub topic: Option<String>,
    pub keyword: Option<String>,
    pub project: Option<String>,
    pub limit: u8,
}
```

Add the variant after `RecallBundle`:

```rust
    /// Adaptive recall with an explicit query and filters (#2249).
    RecallQuery(RecallQuery),
```

In `tool_name`, add `| Action::RecallQuery(_)` to the `icm_memory_recall` arm.
In `arguments`, add:

```rust
            Action::RecallQuery(q) => {
                let mut args = json!({ "query": q.query, "limit": q.limit });
                for (key, value) in [("topic", &q.topic), ("keyword", &q.keyword), ("project", &q.project)] {
                    if let Some(value) = value {
                        args[key] = json!(value);
                    }
                }
                args
            }
```

- [ ] **Step 4: Change the budget**

In `recall.rs`:
- Make `RECALL_BUDGET_BYTES` `pub(super)`.
- Replace `#[derive(Debug, Default, PartialEq, Eq)]` on `RecallBudget` with `#[derive(Debug, PartialEq, Eq)]`, and add fields `limit: usize`, `sent: BTreeSet<String>`, `pub(super) already_sent: usize`.
- Add:

```rust
impl Default for RecallBudget {
    fn default() -> Self {
        Self::new(RECALL_BUDGET_BYTES, BTreeSet::new())
    }
}
```

- In `impl RecallBudget` add:

```rust
    /// A budget of `limit` bytes that skips every record whose hash is in `sent`.
    pub(super) fn new(limit: usize, sent: BTreeSet<String>) -> Self {
        Self {
            kept: Vec::new(),
            seen: HashSet::new(),
            kept_bytes: 0,
            records: 0,
            record_bytes: 0,
            duplicates: 0,
            omitted: 0,
            skipped_actions: 0,
            limit,
            sent,
            already_sent: 0,
        }
    }

    pub(super) fn add_text(&mut self, text: &str) {
        self.add_records(split_recall_records(text));
    }

    pub(super) fn kept_hashes(&self) -> Vec<String> {
        self.kept.iter().map(|r| record_hash(r)).collect()
    }

    /// The passthrough text, then the kept records, then the omission notice.
    pub(super) fn render(&self, passthrough: Vec<String>) -> String {
        let mut parts = passthrough;
        parts.extend(self.kept.iter().cloned());
        parts.extend(self.notice());
        parts.join("\n\n")
    }
```

- In `is_full` and `add_records`, use `self.limit` instead of `RECALL_BUDGET_BYTES`, and make `is_full` `pub(super)`.
- In `add_records`, check the sent set first:

```rust
            if self.sent.contains(&record_hash(&record)) {
                self.already_sent += 1;
            } else if self.seen.contains(&record) {
```

- In `trace_line`, rename `advisory_stripped={}` to `duplicate_records={}`, add `already_sent={}` after it with `self.already_sent`, and update the doc comment: "`duplicate_records` counts records identical to one kept earlier in this call, `already_sent` counts records skipped because the context already holds them".
- Import `use std::collections::BTreeSet;` and `use crate::hook_run::session_ledger::record_hash;`.
- Replace `run_with_budget`'s body with a call to a new filtered version:

```rust
pub(super) async fn run_with_budget<F, Fut>(actions: Vec<Action>, run: F) -> anyhow::Result<(String, RecallBudget)>
where
    F: FnMut(Action) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    run_with_budget_filtered(actions, RecallBudget::default(), run).await
}

/// [`run_with_budget`] with a caller-supplied budget, so a limit and a sent set apply.
///
/// # Errors
/// Returns the first error from `run`.
pub(super) async fn run_with_budget_filtered<F, Fut>(
    actions: Vec<Action>,
    mut budget: RecallBudget,
    mut run: F,
) -> anyhow::Result<(String, RecallBudget)>
where
    F: FnMut(Action) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let mut passthrough: Vec<String> = Vec::new();
    for action in actions {
        if !is_recall(&action) {
            let text = run(action).await?;
            if !text.is_empty() && !passthrough.contains(&text) {
                passthrough.push(text);
            }
        } else if budget.is_full() {
            budget.skipped_actions += 1;
        } else {
            budget.add_text(&run(action).await?);
        }
    }
    let text = budget.render(passthrough);
    Ok((text, budget))
}
```

- Add `| Action::RecallQuery(_)` to `is_recall`.

- [ ] **Step 5: Extract `scope_recall_actions` in `mod.rs`**

```rust
/// The scope-tag recalls in specificity order (#2159): rank-1 tags, bundles, broader tags.
fn scope_recall_actions(
    tag_queries: &[TagRecallQuery],
    bundle_queries: &[BundleRecallQuery],
    ranks: &BTreeMap<String, u8>,
) -> Vec<Action> {
    let rank_of = |q: &TagRecallQuery| ranks.get(&q.tag).copied().unwrap_or(recall::UNSCOPED_RANK);
    let mut tags: Vec<&TagRecallQuery> = tag_queries.iter().collect();
    // Stable, so tags of one rank keep the caller's (alphabetical) order.
    tags.sort_by_key(|q| rank_of(q));
    let split = tags.partition_point(|q| rank_of(q) <= recall::MOST_SPECIFIC_RANK);
    let (specific, broader) = tags.split_at(split);
    let mut actions: Vec<Action> = Vec::new();
    actions.extend(specific.iter().map(|q| Action::RecallTag((*q).clone())));
    actions.extend(bundle_queries.iter().cloned().map(Action::RecallBundle));
    actions.extend(broader.iter().map(|q| Action::RecallTag((*q).clone())));
    actions
}
```

and the `TurnStart` arm of `dispatch` becomes:

```rust
        HookEvent::TurnStart => {
            let mut actions = scope_recall_actions(tag_queries, bundle_queries, ranks);
            actions.push(Action::Recall);
            actions
        }
```

- [ ] **Step 6: Run the whole hook_run suite**

Run: `cargo nextest run -p llmenv hook_run`
Expected: PASS; every existing budget and dispatch test still passes, because the default budget keeps the 8,000-byte limit and an empty sent set.

- [ ] **Step 7: Commit**

```bash
git add src/hook_run/action.rs src/hook_run/recall.rs src/hook_run/mod.rs
git commit -m "feat(hook-run): filter recall budget by sent records"
```

---

### Task 6: Claude Code `SessionStart` envelope (#2251)

**Files:**
- Modify: `src/adapter/claude_code.rs:552-554` (`emit_hook_context`), tests near line 7021

**Interfaces:**
- Produces: `ClaudeCodeAdapter::emit_hook_context("SessionStart", text)` returns the `hookSpecificOutput.additionalContext` envelope; `"SessionEnd"` still returns `""`.

- [ ] **Step 1: Write the failing test and update the old one**

Replace the body of `emit_hook_context_store_only_events_return_empty_string` so that it checks `SessionEnd` only, and add:

```rust
    #[test]
    fn emit_hook_context_session_start_injects_for_claude_code() {
        // #2251: Claude Code accepts additionalContext on SessionStart; the shared
        // suppression came from #558, which was about SessionEnd only.
        let output = ClaudeCodeAdapter.emit_hook_context("SessionStart", "wake data");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("must be valid JSON");
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert!(
            parsed["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .expect("must have additionalContext")
                .contains("wake data")
        );
        assert_eq!(ClaudeCodeAdapter.emit_hook_context("SessionStart", "  "), "");
        assert_eq!(super::super::emit_hook_context("SessionStart", "x"), "", "shared path unchanged");
    }
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `cargo nextest run -p llmenv emit_hook_context_session_start_injects`
Expected: FAIL, output is `""`.

- [ ] **Step 3: Implement**

```rust
    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        // Claude Code accepts additionalContext on SessionStart (#2251). The shared helper
        // suppresses it because other engines are not verified.
        if hook_event_name == "SessionStart" && !text.trim().is_empty() {
            return serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": format!("[ICM MEMORY CONTEXT (auto-injected)]\n{text}"),
                }
            })
            .to_string();
        }
        super::emit_hook_context(hook_event_name, text)
    }
```

To keep one copy of the header string, move `"[ICM MEMORY CONTEXT (auto-injected)]"` in `src/adapter/mod.rs:550` into `pub(crate) const MEMORY_CONTEXT_HEADER: &str` and use it in both places.

- [ ] **Step 4: Run the adapter tests**

Run: `cargo nextest run -p llmenv emit_hook_context`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapter/claude_code.rs src/adapter/mod.rs
git commit -m "fix(adapter): deliver SessionStart context to Claude Code"
```

---

### Task 7: Adaptive flows and concurrent waves

**Files:**
- Create: `src/hook_run/adaptive.rs`
- Modify: `src/hook_run/mod.rs` (add `mod adaptive;`)

**Interfaces:**
- Consumes: `LedgerStore`, `Ledger`, `MAIN_AGENT`, `unix_now`, `record_hash` (Task 3); every `relevance` function (Task 4); `RecallQuery`, `RecallBudget`, `run_with_budget_filtered`, `RECALL_BUDGET_BYTES` (Task 5); `transcript::last_assistant_text` (Task 4); `McpHttpClient` (existing).
- Produces:
  - `pub(super) struct AdaptiveCtx<'a> { pub(super) client: &'a McpHttpClient, pub(super) store: &'a LedgerStore, pub(super) session_id: &'a str, pub(super) payload: &'a serde_json::Value }`
  - `pub(super) async fn session_start(ctx: &AdaptiveCtx<'_>, wake: Action, scope: Vec<Action>) -> anyhow::Result<String>`
  - `pub(super) async fn turn_start(ctx: &AdaptiveCtx<'_>, scope: Vec<Action>) -> anyhow::Result<String>`
  - `pub(super) async fn tool_failure(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<String>`
  - `pub(super) async fn subagent_start(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<String>`
  - `pub(super) fn record_local(event: HookEvent, store: &LedgerStore, session_id: &str, payload: &serde_json::Value)`

- [ ] **Step 1: Write the failing tests** (bottom of `adaptive.rs`)

The tests use a wiremock server that answers every `icm_memory_recall` with a body chosen by a matcher on the request text.

```rust
#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn text(body: &str) -> serde_json::Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": body}]}})
    }

    async fn server_with(pairs: &[(&str, &str)]) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text("")))
            .mount(&server)
            .await;
        for (needle, body) in pairs {
            Mock::given(method("POST"))
                .and(body_string_contains(*needle))
                .respond_with(ResponseTemplate::new(200).set_body_json(text(body)))
                .mount(&server)
                .await;
        }
        server
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: LedgerStore,
        client: McpHttpClient,
    }

    fn fixture(server: &MockServer) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        Fixture { _dir: dir, store, client }
    }

    fn ctx<'a>(f: &'a Fixture, payload: &'a serde_json::Value) -> AdaptiveCtx<'a> {
        AdaptiveCtx { client: &f.client, store: &f.store, session_id: "s1", payload }
    }

    fn tag_action(tag: &str) -> Action {
        Action::RecallTag(crate::hook_run::tag_recall_queries(&[tag.to_string()]).unwrap().remove(0))
    }

    #[tokio::test]
    async fn session_start_sends_scope_once_and_turn_start_does_not_repeat_it() {
        let server = server_with(&[
            ("icm_wake_up", "wake pack"),
            ("llmenv-tag:proj", "[context-p] scope fact"),
            ("\"limit\":10", "[context-p] scope fact\n[context-p] relevant fact"),
        ])
        .await;
        let f = fixture(&server);
        let start = json!({"source": "startup"});
        let out = session_start(&ctx(&f, &start), Action::WakeUp(None), vec![tag_action("proj")])
            .await
            .unwrap();
        assert!(out.contains("wake pack") && out.contains("scope fact"), "{out}");
        let turn = json!({"prompt": "work on recall"});
        let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")]).await.unwrap();
        assert!(out.contains("relevant fact"), "{out}");
        assert!(!out.contains("scope fact"), "already sent: {out}");
    }

    #[tokio::test]
    async fn compact_resets_and_resume_keeps_the_ledger() {
        let server = server_with(&[("icm_wake_up", ""), ("llmenv-tag:proj", "[context-p] scope fact")]).await;
        let f = fixture(&server);
        let run = |source: &'static str| {
            let payload = json!({"source": source});
            let f = &f;
            async move {
                session_start(&ctx(f, &payload), Action::WakeUp(None), vec![tag_action("proj")])
                    .await
                    .unwrap()
            }
        };
        assert!(run("startup").await.contains("scope fact"));
        assert!(!run("resume").await.contains("scope fact"), "resume keeps sent");
        assert!(run("compact").await.contains("scope fact"), "compact resets");
    }

    #[tokio::test]
    async fn turn_start_falls_back_to_the_scope_set_without_session_start() {
        let server = server_with(&[("llmenv-tag:proj", "[context-p] scope fact")]).await;
        let f = fixture(&server);
        let turn = json!({"prompt": "hi"});
        let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")]).await.unwrap();
        assert!(out.contains("scope fact"), "{out}");
        assert!(f.store.load("s1").unwrap().scope_sent(MAIN_AGENT));
    }

    #[tokio::test]
    async fn a_repeated_query_with_no_new_activity_makes_no_relevance_calls() {
        let server = server_with(&[("\"limit\":10", "[context-p] fact")]).await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "same"});
        turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        let calls_before = server.received_requests().await.unwrap().len();
        turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), calls_before);
    }

    #[tokio::test]
    async fn topic_fanout_recalls_sibling_topics() {
        let server = server_with(&[
            ("\"limit\":10", "[context-llmenv] main fact"),
            ("decisions-llmenv", "[decisions-llmenv] sibling fact"),
        ])
        .await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "why"});
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert!(out.contains("main fact") && out.contains("sibling fact"), "{out}");
    }

    #[tokio::test]
    async fn one_failed_call_keeps_the_others() {
        let server = server_with(&[("\"limit\":10", "[context-p] kept fact")]).await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"project\":\"\""))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "x"});
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert!(out.contains("kept fact"), "{out}");
    }

    #[tokio::test]
    async fn tool_failure_injects_errors_once() {
        let server = server_with(&[("errors-resolved", "[errors-resolved] fix: pin mcp<2")]).await;
        let f = fixture(&server);
        let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
        let first = tool_failure(&ctx(&f, &payload)).await.unwrap();
        assert!(first.contains("pin mcp<2"), "{first}");
        assert_eq!(f.store.load("s1").unwrap().errors().len(), 1);
        let second = tool_failure(&ctx(&f, &payload)).await.unwrap();
        assert!(!second.contains("pin mcp<2"), "already sent: {second}");
    }

    #[tokio::test]
    async fn invalid_agent_id_records_nothing() {
        let server = server_with(&[("errors-resolved", "[errors-resolved] fix")]).await;
        let f = fixture(&server);
        let payload = json!({"tool_name": "Bash", "error": "boom", "agent_id": "../evil"});
        let out = tool_failure(&ctx(&f, &payload)).await.unwrap();
        assert!(out.contains("fix"));
        assert!(f.store.load("s1").unwrap().sent_for(MAIN_AGENT).is_empty());
    }

    #[tokio::test]
    async fn subagent_gets_its_task_memories_without_touching_the_parent() {
        let server = server_with(&[("map the recall path", "[context-p] recall lives in hook_run")]).await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.mark_sent(MAIN_AGENT, [record_hash("[context-p] recall lives in hook_run")]));
        record_local(
            HookEvent::PreToolUse,
            &f.store,
            "s1",
            &json!({"tool_name": "Agent", "tool_use_id": "u1",
                    "tool_input": {"subagent_type": "Explore", "prompt": "map the recall path"}}),
        );
        let payload = json!({"agent_id": "agent-1", "agent_type": "Explore"});
        let out = subagent_start(&ctx(&f, &payload)).await.unwrap();
        assert!(out.contains("recall lives in hook_run"), "{out}");
        let ledger = f.store.load("s1").unwrap();
        assert_eq!(ledger.sent_for(MAIN_AGENT).len(), 1, "parent unchanged");
        assert_eq!(ledger.sent_for("agent-1").len(), 1);
    }

    #[test]
    fn record_local_appends_batch_activity() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        record_local(
            HookEvent::PostToolBatch,
            &store,
            "s1",
            &json!({"tool_calls": [
                {"tool_name": "Read", "tool_input": {"file_path": "/r/src/a.rs"}},
                {"tool_name": "Bash", "tool_input": {"command": "cargo test"}}
            ]}),
        );
        let targets: Vec<_> = store.load("s1").unwrap().activity().iter()
            .map(|a| a.target.clone().unwrap()).collect();
        assert_eq!(targets, ["/r/src/a.rs", "cargo"]);
    }
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo nextest run -p llmenv adaptive::`
Expected: compile errors for the missing items.

- [ ] **Step 3: Write the module**

```rust
//! Adaptive recall flows for the lifecycle hooks (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::hook_run::HookEvent;
use crate::hook_run::action::{Action, RecallQuery, split_recall_records};
use crate::hook_run::mcp_client::McpHttpClient;
use crate::hook_run::recall::{RECALL_BUDGET_BYTES, RecallBudget, run_with_budget_filtered};
use crate::hook_run::relevance::{self, TurnSignals};
use crate::hook_run::session_ledger::{Ledger, LedgerStore, MAIN_AGENT, record_hash, unix_now};
use crate::hook_run::transcript;

const FAILURE_BUDGET_BYTES: usize = 2_000;
const SUBAGENT_BUDGET_BYTES: usize = 4_000;
const MAIN_LIMIT: u8 = 10;
const FANOUT_LIMIT: u8 = 3;
const FAILURE_LIMIT: u8 = 5;
const TAIL_CHARS: usize = 300;
/// Wave 2 would push a slow backend past the latency that a prompt tolerates.
const WAVE2_CUTOFF: Duration = Duration::from_millis(1_500);

/// What every adaptive flow needs.
pub(super) struct AdaptiveCtx<'a> {
    pub(super) client: &'a McpHttpClient,
    pub(super) store: &'a LedgerStore,
    pub(super) session_id: &'a str,
    pub(super) payload: &'a Value,
}

fn query(text: &str, limit: u8) -> RecallQuery {
    RecallQuery { query: text.to_string(), topic: None, keyword: None, project: None, limit }
}

/// Run one recall; a failure costs its records only.
async fn recall(client: &McpHttpClient, q: Option<RecallQuery>) -> String {
    let Some(q) = q else { return String::new() };
    Action::RecallQuery(q).run(client, "", "").await.unwrap_or_else(|e| {
        tracing::warn!("adaptive recall call failed, its records are skipped: {e}");
        String::new()
    })
}

/// The recall texts for `text`, in budget order: main, keyword fanout, topic fanout,
/// cross-project.
async fn waves(client: &McpHttpClient, text: &str, keywords: &[String]) -> Vec<String> {
    let keyword = |i: usize| {
        keywords.get(i).map(|k| RecallQuery {
            keyword: Some(k.clone()),
            project: Some(String::new()),
            ..query(text, FANOUT_LIMIT)
        })
    };
    let cross = RecallQuery { project: Some(String::new()), ..query(text, FANOUT_LIMIT) };
    let start = Instant::now();
    let (main, kw0, kw1, cross) = tokio::join!(
        recall(client, Some(query(text, MAIN_LIMIT))),
        recall(client, keyword(0)),
        recall(client, keyword(1)),
        recall(client, Some(cross)),
    );
    let mut texts = vec![main, kw0, kw1];
    if start.elapsed() < WAVE2_CUTOFF {
        let topics: Vec<String> = texts
            .iter()
            .flat_map(|t| split_recall_records(t))
            .filter_map(|r| relevance::record_topic(&r).map(str::to_string))
            .collect();
        let siblings = relevance::sibling_topics(&topics);
        let topic = |i: usize| {
            siblings.get(i).map(|t| RecallQuery { topic: Some(t.clone()), ..query(text, FANOUT_LIMIT) })
        };
        let (t0, t1) = tokio::join!(recall(client, topic(0)), recall(client, topic(1)));
        texts.extend([t0, t1]);
    }
    texts.push(cross);
    texts
}

fn resets_ledger(source: Option<&str>) -> bool {
    matches!(source, Some("startup" | "clear" | "compact"))
}

/// `SessionStart`: reset per `source`, then send the wake-up pack and the scope set.
pub(super) async fn session_start(ctx: &AdaptiveCtx<'_>, wake: Action, scope: Vec<Action>) -> anyhow::Result<String> {
    if resets_ledger(ctx.payload["source"].as_str()) {
        ctx.store.update(ctx.session_id, Ledger::reset);
    }
    let sent = ctx.store.load(ctx.session_id).unwrap_or_default().sent_for(MAIN_AGENT);
    let mut actions = vec![wake];
    actions.extend(scope);
    let (text, budget) = run_with_budget_filtered(actions, RecallBudget::new(RECALL_BUDGET_BYTES, sent), |a| async move {
        a.run(ctx.client, "", "").await
    })
    .await?;
    let kept = budget.kept_hashes();
    ctx.store.update(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        l.set_scope_sent(MAIN_AGENT);
    });
    Ok(text)
}

/// `TurnStart`: relevance recall, with the scope set as a fallback when no
/// `SessionStart` ran in this epoch.
pub(super) async fn turn_start(ctx: &AdaptiveCtx<'_>, scope: Vec<Action>) -> anyhow::Result<String> {
    let ledger = ctx.store.load(ctx.session_id).unwrap_or_default();
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, ledger.sent_for(MAIN_AGENT));
    let fallback = !ledger.scope_sent(MAIN_AGENT);
    if fallback {
        for action in scope {
            if budget.is_full() {
                break;
            }
            budget.add_text(&action.run(ctx.client, "", "").await.unwrap_or_default());
        }
    }
    let tail = ctx.payload["transcript_path"]
        .as_str()
        .and_then(|p| transcript::last_assistant_text(Path::new(p), TAIL_CHARS));
    let text = relevance::turn_query(&TurnSignals {
        prompt: ctx.payload["prompt"].as_str().unwrap_or_default(),
        activity: ledger.activity(),
        errors: ledger.errors(),
        last_turn_at: ledger.last_turn_at,
        assistant_tail: tail.as_deref(),
    });
    let query_hash = record_hash(&text);
    let repeated = ledger.last_query_hash.as_deref() == Some(query_hash.as_str())
        && !ledger.activity_since(ledger.last_turn_at);
    if !repeated && !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        for wave_text in waves(ctx.client, &text, &keywords).await {
            budget.add_text(&wave_text);
        }
    }
    let kept = budget.kept_hashes();
    let now = unix_now();
    ctx.store.update(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        l.last_query_hash = Some(query_hash);
        l.last_turn_at = now;
        if fallback {
            l.set_scope_sent(MAIN_AGENT);
        }
    });
    Ok(budget.render(Vec::new()))
}

/// The ledger key for the context that fired the hook, and whether hashes may be
/// recorded under it. An unsafe `agent_id` filters as `main` and records nothing.
fn agent_key(payload: &Value) -> (String, bool) {
    match payload["agent_id"].as_str() {
        None => (MAIN_AGENT.to_string(), true),
        Some(id) if crate::paths::is_valid_short_name(id) => (id.to_string(), true),
        Some(_) => (MAIN_AGENT.to_string(), false),
    }
}

/// `PostToolUseFailure`: record the error, then inject related memories once.
pub(super) async fn tool_failure(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<String> {
    let tool = ctx.payload["tool_name"].as_str().unwrap_or_default();
    let error = ctx.payload["error"].as_str().unwrap_or_default();
    let now = unix_now();
    ctx.store.update(ctx.session_id, |l| l.push_error(tool, error, now));
    let (key, record) = agent_key(ctx.payload);
    let sent = ctx.store.load(ctx.session_id).unwrap_or_default().sent_for(&key);
    let text = relevance::error_query(tool, error);
    let resolved = RecallQuery {
        topic: Some("errors-resolved".to_string()),
        project: Some(String::new()),
        ..query(&text, FANOUT_LIMIT)
    };
    let (main, fixes) = tokio::join!(
        recall(ctx.client, Some(query(&text, FAILURE_LIMIT))),
        recall(ctx.client, Some(resolved)),
    );
    let mut budget = RecallBudget::new(FAILURE_BUDGET_BYTES, sent);
    budget.add_text(&fixes);
    budget.add_text(&main);
    if record {
        let kept = budget.kept_hashes();
        ctx.store.update(ctx.session_id, |l| l.mark_sent(&key, kept));
    }
    Ok(budget.render(Vec::new()))
}

/// `SubagentStart`: task-relevant memories for a fresh subagent context.
pub(super) async fn subagent_start(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<String> {
    let (key, record) = agent_key(ctx.payload);
    if !record || key == MAIN_AGENT {
        tracing::warn!("SubagentStart without a usable agent_id, injection skipped");
        return Ok(String::new());
    }
    let agent_type = ctx.payload["agent_type"].as_str().unwrap_or_default();
    let now = unix_now();
    let task = ctx.store.update(ctx.session_id, |l| l.take_subagent(agent_type, now)).flatten();
    let ledger = ctx.store.load(ctx.session_id).unwrap_or_default();
    let text = relevance::subagent_query(task.as_ref().map(|t| t.task.as_str()), agent_type, ledger.activity());
    let mut budget = RecallBudget::new(SUBAGENT_BUDGET_BYTES, ledger.sent_for(&key));
    if !text.is_empty() {
        for wave_text in waves(ctx.client, &text, &relevance::fanout_keywords(ledger.activity())).await {
            budget.add_text(&wave_text);
        }
    }
    let kept = budget.kept_hashes();
    ctx.store.update(ctx.session_id, |l| l.mark_sent(&key, kept));
    Ok(budget.render(Vec::new()))
}

/// Local ledger writes with no MCP call: batch activity and queued subagent tasks.
pub(super) fn record_local(event: HookEvent, store: &LedgerStore, session_id: &str, payload: &Value) {
    let now = unix_now();
    match event {
        HookEvent::PostToolBatch => {
            let calls = payload["tool_calls"].as_array().cloned().unwrap_or_default();
            store.update(session_id, |l| {
                for call in &calls {
                    let tool = call["tool_name"].as_str().unwrap_or_default();
                    l.push_activity(relevance::activity_from_tool_call(tool, &call["tool_input"], now));
                }
            });
        }
        HookEvent::PreToolUse if payload["tool_name"].as_str() == Some("Agent") => {
            let input = &payload["tool_input"];
            let id = payload["tool_use_id"].as_str().unwrap_or_default();
            let kind = input["subagent_type"].as_str().unwrap_or("general-purpose");
            let prompt = input["prompt"].as_str().unwrap_or_default();
            store.update(session_id, |l| l.queue_subagent(id, kind, prompt, now));
        }
        _ => {}
    }
}
```

Notes for the implementer:
- `BTreeSet` is imported for the signatures the compiler may ask for; remove the import if clippy flags it as unused.
- `Action::run(client, "", "")` is correct for `RecallTag`, `RecallBundle`, `RecallQuery`, and `WakeUp`: none of them reads the `query` or `chunk` argument.
- The wiremock matchers in the tests depend on the JSON the client sends. If `"limit":10` does not match because the client pretty-prints or reorders, match on a string the request always holds, such as the query text, and keep the assertion the same.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo nextest run -p llmenv adaptive::`
Expected: PASS (10 tests).

- [ ] **Step 5: Break the dedup to prove the tests catch it, then restore**

In `turn_start`, change `RecallBudget::new(RECALL_BUDGET_BYTES, ledger.sent_for(MAIN_AGENT))` to `RecallBudget::new(RECALL_BUDGET_BYTES, BTreeSet::new())`.
Run: `cargo nextest run -p llmenv session_start_sends_scope_once` — expected FAIL.
Restore and run again — expected PASS.

- [ ] **Step 6: Commit**

```bash
git add src/hook_run/adaptive.rs src/hook_run/mod.rs
git commit -m "feat(hook-run): add adaptive recall flows"
```

---

### Task 8: Wire the flows into `run_inner`

**Files:**
- Modify: `src/hook_run/mod.rs` (`run_inner` near lines 977-1045 and 1220-1232; new helper `run_event_memory`)

**Interfaces:**
- Consumes: everything from Tasks 1-7.
- Produces: `async fn run_event_memory(call: MemoryCall<'_>) -> anyhow::Result<String>` where

```rust
struct MemoryCall<'a> {
    event: HookEvent,
    client: &'a McpHttpClient,
    settings: Option<MemoryHookSettings>,
    session_id: Option<&'a str>,
    payload: &'a serde_json::Value,
    actions: Vec<Action>,
    scope: Vec<Action>,
    query: &'a str,
    store_content: &'a str,
}
```

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn adaptive_applies_only_to_its_events_with_a_valid_session() {
        let on = Some(MemoryHookSettings { wakeup_max_tokens: None, adaptive_recall: true });
        let off = Some(MemoryHookSettings { wakeup_max_tokens: None, adaptive_recall: false });
        for event in [
            HookEvent::SessionStart,
            HookEvent::TurnStart,
            HookEvent::PostToolUseFailure,
            HookEvent::SubagentStart,
        ] {
            assert!(uses_adaptive(event, on, Some("s1")), "{event}");
            assert!(!uses_adaptive(event, off, Some("s1")), "{event}");
            assert!(!uses_adaptive(event, on, None), "{event}");
            assert!(!uses_adaptive(event, on, Some("../x")), "unsafe id: {event}");
        }
        assert!(!uses_adaptive(HookEvent::SessionEnd, on, Some("s1")));
    }

    #[test]
    fn local_recording_events_skip_the_memory_pipeline() {
        assert!(records_locally(HookEvent::PostToolBatch));
        assert!(records_locally(HookEvent::PreToolUse));
        assert!(!records_locally(HookEvent::TurnStart));
    }
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo nextest run -p llmenv adaptive_applies_only local_recording_events`
Expected: compile errors.

- [ ] **Step 3: Implement the predicates and the helper** (beside `counts_tool_use`)

```rust
/// Whether `event` writes to the recall ledger without any MCP call (#2249).
fn records_locally(event: HookEvent) -> bool {
    matches!(event, HookEvent::PostToolBatch | HookEvent::PreToolUse)
}

/// Whether `event` takes the adaptive recall flow instead of the stateless actions.
/// An unsafe `session_id` cannot key a ledger file, so it takes the stateless actions.
fn uses_adaptive(event: HookEvent, settings: Option<MemoryHookSettings>, session_id: Option<&str>) -> bool {
    settings.is_some_and(|s| s.adaptive_recall)
        && session_id.is_some_and(crate::paths::is_valid_short_name)
        && matches!(
            event,
            HookEvent::SessionStart
                | HookEvent::TurnStart
                | HookEvent::PostToolUseFailure
                | HookEvent::SubagentStart
        )
}

/// Run the event's memory work: the adaptive flow when it applies, else the stateless
/// actions. A missing state dir degrades to the stateless actions.
async fn run_event_memory(call: MemoryCall<'_>) -> anyhow::Result<String> {
    let state_dir = crate::paths::state_dir().ok();
    let (Some(session_id), Some(state_dir)) = (call.session_id, state_dir) else {
        return run_memory_actions(call.client, call.actions, call.query, call.store_content).await;
    };
    if !uses_adaptive(call.event, call.settings, Some(session_id)) {
        return run_memory_actions(call.client, call.actions, call.query, call.store_content).await;
    }
    let store = session_ledger::LedgerStore::new(&state_dir);
    let ctx = adaptive::AdaptiveCtx { client: call.client, store: &store, session_id, payload: call.payload };
    match call.event {
        HookEvent::SessionStart => {
            let wake = Action::WakeUp(call.settings.and_then(|s| s.wakeup_max_tokens));
            adaptive::session_start(&ctx, wake, call.scope).await
        }
        HookEvent::TurnStart => adaptive::turn_start(&ctx, call.scope).await,
        HookEvent::PostToolUseFailure => adaptive::tool_failure(&ctx).await,
        HookEvent::SubagentStart => adaptive::subagent_start(&ctx).await,
        _ => run_memory_actions(call.client, call.actions, call.query, call.store_content).await,
    }
}
```

- [ ] **Step 4: Call them from `run_inner`**

Directly after the `task_tracker_enabled` binding (before `let pre_tool_text` at line 940), add the block below.
It must come before `pre_tool_text`, because that path can return early at line 958 for a `PreToolUse`, and the `Agent` task would then never be queued.

```rust
    // #2249: local ledger writes need no scope or MCP work, so they run before every
    // early return in this function.
    if records_locally(event)
        && let (Some(session_id), Ok(state_dir)) = (claude_session_id, crate::paths::state_dir())
    {
        adaptive::record_local(
            event,
            &session_ledger::LedgerStore::new(&state_dir),
            session_id,
            stdin_payload,
        );
    }
```

In the #702 early-exit `matches!` list, add `| HookEvent::PostToolUseFailure | HookEvent::SubagentStart`.

At line 1144, keep `settings` from Task 2.
Replace line 1232 (`out = run_memory_actions(client, actions, &query, store_content).await?;`) with:

```rust
                out = run_event_memory(MemoryCall {
                    event,
                    client,
                    settings,
                    session_id: claude_session_id,
                    payload: stdin_payload,
                    actions,
                    scope: scope_recall_actions(&tag_queries, &bundle_queries, &tag_ranks),
                    query: &query,
                    store_content,
                })
                .await?;
```

`settings` must be in scope inside the `async` block; it is `Copy`, so capture it by value.

- [ ] **Step 5: Add an end-to-end fail-soft test**

In `tests/hook_run_failsoft.rs`, copy the existing test that runs `hook-run turn_start` with an unreachable backend, and add a variant that sends `{"hook_event_name":"PostToolUseFailure","session_id":"s1","tool_name":"Bash","error":"x"}` to `hook-run post_tool_use_failure`.
Assert exit status 0 and an empty stdout, the same as the existing test.

- [ ] **Step 6: Run the full suite**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/hook_run/mod.rs tests/hook_run_failsoft.rs
git commit -m "feat(hook-run): route lifecycle recall through adaptive flows"
```

---

### Task 9: Register the new hooks for Claude Code

**Files:**
- Modify: `src/adapter/claude_code.rs:88-125` (`lifecycle_hook_registrations`), `:259-269` (`CLAUDE_CODE_HOOK_EVENTS`), `:1558-1570` (registration after `turn_start`)
- Test: `src/adapter/claude_code.rs` tests near lines 3659-3731

**Interfaces:**
- Produces: `const ADAPTIVE_RECALL_HOOK_EVENTS: &[(&str, &str)] = &[("post_tool_batch", "PostToolBatch"), ("post_tool_use_failure", "PostToolUseFailure"), ("subagent_start", "SubagentStart")];`
- Produces: registrations `PostToolBatch`, `PostToolUseFailure`, `SubagentStart` with `hook-run <neutral>`, and `PreToolUse` with matcher `^Agent$` and `hook-run pre_tool_use`, all only when the memory backend is active.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn adaptive_recall_hooks_register_only_with_a_memory_backend() {
        let settings = render_settings_for_test(&manifest_with_icm());
        for event in ["PostToolBatch", "PostToolUseFailure", "SubagentStart"] {
            assert_eq!(hook_commands_for(&settings, event).len(), 1, "{event}");
        }
        let agent_matcher = settings["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["matcher"] == "^Agent$");
        assert!(agent_matcher);
        let bare = render_settings_for_test(&manifest_without_icm());
        for event in ["PostToolBatch", "PostToolUseFailure", "SubagentStart"] {
            assert!(hook_commands_for(&bare, event).is_empty(), "{event}");
        }
    }
```

Use the fixture builders that the `turn_start` gate tests at lines 3677-3709 already use; if their names differ from `manifest_with_icm` and `manifest_without_icm`, use theirs.

- [ ] **Step 2: Run it and confirm it fails**

Run: `cargo nextest run -p llmenv adaptive_recall_hooks_register`
Expected: FAIL, no commands for `PostToolBatch`.

- [ ] **Step 3: Implement**

Add the three native names to `CLAUDE_CODE_HOOK_EVENTS`.
Add the constant `ADAPTIVE_RECALL_HOOK_EVENTS` beside `SESSION_LOG_HOOK_EVENTS`.
In `lifecycle_hook_registrations`, add one row per neutral event:

```rust
        ("post_tool_batch", icm_active, "needs a memory backend (features.memory)"),
        ("post_tool_use_failure", icm_active, "needs a memory backend (features.memory)"),
        ("subagent_start", icm_active, "needs a memory backend (features.memory)"),
```

After the `turn_start` registration block:

```rust
    // #2249: adaptive recall records tool activity, injects on failures and subagent
    // starts, and queues subagent tasks. Same gate as turn_start: each hook fires often.
    let registered = lifecycle_hook_registrations(manifest);
    for (neutral_event, native_event) in ADAPTIVE_RECALL_HOOK_EVENTS {
        if registered.iter().any(|(event, on, _)| event == neutral_event && *on) {
            hooks_by_event
                .entry((*native_event).to_string())
                .or_default()
                .push(json!({
                    "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} {neutral_event}") }],
                }));
        }
    }
    if registered.iter().any(|(event, on, _)| *event == "subagent_start" && *on) {
        hooks_by_event
            .entry("PreToolUse".to_string())
            .or_default()
            .push(json!({
                "matcher": "^Agent$",
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} pre_tool_use") }],
            }));
    }
```

Run `cargo nextest run -p llmenv adapter` and update any test that pins the exact event set or the exact `lifecycle_hook_registrations` rows (for example the pair table near line 3731 and `doctor` tests in `src/cli/doctor.rs`) so that it lists the new rows.

- [ ] **Step 4: Run the full suite**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapter/claude_code.rs src/cli/doctor.rs
git commit -m "feat(adapter): register adaptive recall hooks"
```

---

### Task 10: Docs, changelog, design doc, and the follow-up issue

**Files:**
- Modify: `website/docs/configuration.md:553-560` (`features.memory` table and text)
- Modify: `website/docs/commands.md:260-275` (`hook-run` events)
- Modify: `docs/design/issue-2159-2141-icm-recall-prioritization.md`
- Modify: `CHANGELOG.md` (`## [Unreleased]`)

- [ ] **Step 1: Configuration docs**

Add a table row after `wakeup_max_tokens`:

```markdown
| `adaptive_recall` | no | Per-session adaptive recall, `true` or `false`, default `true` (added in v3.12.0) |
```

Add a section after the `wakeup_max_tokens` paragraph, one sentence per line:

```markdown
`adaptive_recall` (added in v3.12.0) controls how the lifecycle hooks pick memories.
With the default `true`, llmenv keeps a small state file per session and sends each memory one time per model context.
The session start sends the wake-up pack and the scope-tagged memories.
Each prompt then recalls memories that match the prompt, the files and commands in recent tool calls, the newest tool error, and the last assistant reply, plus memories from related topics.
A failed tool call injects memories about that error, and a new subagent gets memories that match its task.
After a compaction or `/clear`, the state resets and the scope-tagged memories go out again.
Set `adaptive_recall: false` to go back to the stateless recall, which sends the same scope-tagged memories on every prompt.
```

- [ ] **Step 2: Commands docs**

In the `hook-run` event list, update `session_start` and `turn_start`, and add the new events:

```markdown
- `session_start` — injects the session wake-up pack (`icm_wake_up`) and, with `adaptive_recall`, the scope-tagged memories; resets the per-session recall state after a compaction or `/clear` (context delivery to Claude Code fixed in v3.12.0)
- `turn_start` — with `adaptive_recall`, injects memories that match the prompt and recent session activity, skipping memories already sent in this context (changed in v3.12.0); without it, injects the scope-tagged recall on every prompt
- `post_tool_batch` — records the tools, files, and commands of a batch in the per-session recall state; no output (added in v3.12.0)
- `post_tool_use_failure` — records the error and injects memories about it (added in v3.12.0)
- `subagent_start` — injects memories that match the subagent's task (added in v3.12.0)
```

Keep the surrounding wrap style of the file.

- [ ] **Step 3: Design doc**

In `docs/design/issue-2159-2141-icm-recall-prioritization.md`, add a section at the end:

```markdown
## Update for #2249 (v3.12.0)

With `adaptive_recall` on, the ranked scope-tag recall described here runs at `SessionStart`, one time per model context, not on every prompt.
`TurnStart` runs a relevance recall instead.
The byte budget and the specificity ranking in this document still apply to the scope set.
Design: `docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md`.
```

- [ ] **Step 4: Changelog**

Invoke the `keepachangelog` skill (and `bens-voice` for the wording).
Entries under `## [Unreleased]`:

- `### Added`: adaptive ICM recall, one or two sentences, linking `https://phaedrus1992.github.io/llmenv/docs/configuration#featuresmemory`, with `(#2249)`.
- `### Fixed`: the Claude Code session start now delivers the wake-up pack, which was fetched and discarded, `(#2251)`.

Then run the forward-merge reconciliation from `AGENTS.md`: `git log --no-merges $(git describe --tags --abbrev=0)..HEAD --oneline` and check the older release line's changelog for inherited fixes missing here.
Run `scripts/sync-changelog-doc.sh` so that `website/docs/changelog.md` matches.

- [ ] **Step 5: File the upstream follow-up issue**

The upstream is ICM, not llmenv.
Find its repo with `git grep -n "icm" -- Cargo.toml README.md website/docs | rg -i "github.com"`; if the repo is not found, ask the user for it.
File one issue: "Expose record ids and links in icm_memory_recall output", with the reason: llmenv #2249 dedups by content hash and cannot use ICM's stored links for related-topic expansion.
Record the new issue URL in the `## Follow-up` section of the spec.

- [ ] **Step 6: Lint and commit**

Run: `npx --no-install markdownlint-cli2 website/docs/configuration.md website/docs/commands.md CHANGELOG.md`
Expected: 0 issues.

```bash
git add website/docs CHANGELOG.md docs/design/issue-2159-2141-icm-recall-prioritization.md docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md
git commit -m "docs: document adaptive ICM recall"
```

---

## Final verification (before handing back to ship-issue)

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-features --tests -- -D warnings`
- [ ] `cargo nextest run --workspace`
- [ ] `bash scripts/hawk-check.sh -D warnings`
- [ ] Manual check with a real session: run `llmenv regenerate`, start a Claude Code session, and confirm the first prompt's ICM block differs from the session-start block, and that a second prompt on a new topic brings new memories.
