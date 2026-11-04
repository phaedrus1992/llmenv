# Issue #3218 — tell the agent once per session about a bundle with no content

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/3218
- **Milestone:** `v3.14.0`
- **Base branch:** `release/3.x`
- **Code:** `src/hook_run/memory_endpoint.rs`, a new `src/hook_run/bundle_notice.rs`, `src/hook_run/mod.rs`
- **Type:** new agent notice with per-session dedupe, tests, docs

## Problem

A bundle can fire while its content folder is missing.
llmenv then cannot read the bundle's `bundle.yaml`, so it cannot tell whether the bundle declares `features.memory`.
`memory_url` returns `MemoryEndpoint::NotDeclared` with a non-empty `skipped_bundles` list (`src/hook_run/memory_target.rs:31`; the list is built in `classify_missing_memory`, `src/hook_run/memory_resolve.rs:151-158`).

`resolve_memory_client` (`src/hook_run/memory_endpoint.rs:29`) writes that case to stderr only (line 55).
The engine drops a hook's stderr, so the agent sees no memory and no reason.
A notice on every event would repeat on every hook call, because each hook event runs as its own process.

## Decision

Owner decision on #3218 (2026-11-03):

> Notify once per session, keyed by bundle name, through the existing recall-health or state files. Acceptance: `resolve_memory_client_adds_no_notice_for_a_bundle_without_content` changes to assert one notice on the first event and none on the second.

## Where the "once" is stored

`recall_health.json` is one global file, not one for each session, so it cannot hold a per-session record.
The design uses the per-session state file pattern that `stop_dedupe`, `read_once`, and `repeat_detect` use: `<state_dir>/<feature>/<session_id>.json`, locked with `session_state::lock_state_file`.

The new feature folder is `<state_dir>/bundle_notice/`.
Each file holds the set of bundle names that this session was already told about.

## Design

### 1. New module `src/hook_run/bundle_notice.rs`

Declare it in `src/hook_run/mod.rs` with `mod bundle_notice;`.

```rust
//! Remember which content-less bundles a session was already told about (#3218).

const DIR: &str = "bundle_notice";

/// The bundle names one session was told about.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Noticed {
    bundles: std::collections::BTreeSet<String>,
}

/// The names in `names` that this session was not yet told about. Records them as told.
///
/// With no state folder, or no usable session id, nothing can be stored, so every name is
/// returned on every call: a repeated notice is better than a silent one.
pub(super) fn unnoticed(state_dir: Option<&Path>, session_id: Option<&str>, names: &[String]) -> Vec<String>
```

Steps of `unnoticed`:

1. If `names` is empty, return an empty `Vec`.
2. If `state_dir` is `None`, or `session_id` is `None`, return `names.to_vec()`.
3. If the session id is not a safe file name, return `names.to_vec()`. Use the same check as `stop_dedupe::state_path` (`src/hook_run/stop_dedupe.rs:80`), `is_valid_short_name`.
4. Let `dir = state_dir.join(DIR)`. Do not create the folder yourself: `lock_state_file` in step 5 creates it owner-only (`src/hook_run/session_state.rs`, around line 62).
5. Take the lock with `session_state::lock_state_file(&dir, id, "bundle notice")`. If it returns `None`, return `names.to_vec()`.
6. Read `<dir>/<id>.json`:
   - missing file: start from `Noticed::default()`;
   - a file that does not parse: log it with `tracing::error!` (name the file), and start from `Noticed::default()`. The write in step 9 replaces it.
   - any other read error: log it with `tracing::error!`, and return `names.to_vec()` without a write.
7. Let `new` be the names from `names` that are not in `noticed.bundles`, in input order, without repeats.
8. If `new` is empty, return it. Do not write.
9. Insert `new` into the set. Call `session_state::prune_stale_json_files(&dir, 7)` and `session_state::prune_orphan_locks(&dir, 7)`, as `save_stats` does in `src/hook_run/slippage.rs`. Then write the JSON with `crate::paths::write_owner_only_atomic`. On a write error, log it with `tracing::error!` and still return `new`.
10. Return `new`.

### 2. A context argument for `resolve_memory_client`

`resolve_memory_client` already has five positional parameters, the project cap.
Put the event and the two new values in one struct:

```rust
/// What a memory call needs to know about the hook event that makes it.
pub(super) struct MemoryCallContext<'a, E> {
    /// The hook event, for notice text.
    pub event: E,
    /// The llmenv state folder, if known.
    pub state_dir: Option<&'a Path>,
    /// The engine conversation id from the hook input, if any.
    pub session_id: Option<&'a str>,
}
```

Do not put a trait bound on the struct.
Put `E: std::fmt::Display` on `resolve_memory_client`.

New signature.
The function stays `pub(super)`, as it is today, and the struct matches it:

```rust
pub(super) fn resolve_memory_client<E: Display>(
    config: &Config,
    config_dir: &Path,
    active: &ActiveScopes,
    call: MemoryCallContext<'_, E>,
    cache: &'static OnceLock<Mutex<HashMap<String, McpHttpClient>>>,
) -> Option<ResolvedMemoryClient>
```

Replace each use of `event` in the body with `call.event`.

### 3. The notice (`src/hook_run/memory_endpoint.rs`)

In the branch that handles the `memory_url` result before `into_url_and_settings` (lines 50-57):

1. If the endpoint is `MemoryEndpoint::NotDeclared { skipped_bundles }` and `skipped_bundles` is not empty, call `bundle_notice::unnoticed(call.state_dir, call.session_id, &skipped_bundles)`. Clone the list first if the next step consumes the endpoint.
2. If the result is not empty, call `crate::agent_notice::record_logged` with this text, where `{names}` is the new names joined by `, `:

   ```text
   llmenv: memory {event} skipped: bundle(s) {names} fired, but each has no content folder under the config dir's bundles/, or its name was rejected. llmenv cannot read whether it declares a memory backend. This notice shows once per session for each bundle. Fix: add the bundle content folder, or remove the bundle from config.yaml.
   ```

3. Keep the existing stderr line for every error case, unchanged.

The other cases (`NoBundlesFired`, `Suppressed`, `TagInactive`, and `NotDeclared` with an empty list) stay on stderr only, as today.

### 4. The caller (`src/hook_run/mod.rs:1410`)

The caller is inside `run_inner`, where `claude_session_id: Option<&str>` is in scope.
Get the state folder as an `Option<PathBuf>`:

- If the value that lines 1390-1392 get from `state_dir_or_log` is still in scope at the call, reuse it.
- Otherwise call `crate::paths::state_dir().ok()`. Do not call `state_dir_or_log` a second time: it records a notice on failure, and the earlier call already did.

Pass:

```rust
MemoryCallContext {
    event,
    state_dir: state_dir.as_deref(),
    session_id: claude_session_id,
}
```

Update every test that calls `resolve_memory_client` to pass a `MemoryCallContext`, with `state_dir: None` and `session_id: None` unless the test needs them.

## Tests

### Unit tests in `src/hook_run/bundle_notice.rs`

Each test uses a `tempfile::tempdir()` as the state folder.

1. `a_new_bundle_is_returned_once_per_session`
   - The first call with `["b1"]` and session `s1` returns `["b1"]`. The second identical call returns an empty list.
2. `a_later_new_bundle_is_returned_alone`
   - After `["b1"]` was returned, a call with `["b1", "b2"]` returns `["b2"]`.
3. `another_session_is_told_again`
   - After `["b1"]` was returned for `s1`, a call for `s2` returns `["b1"]`.
4. `no_session_id_returns_every_name_every_time`
   - Two calls with `session_id: None` each return `["b1"]`.
5. `a_corrupt_record_is_replaced`
   - Write `not json` to `<state>/bundle_notice/s1.json`. A call with `["b1"]` returns `["b1"]`. A second call returns an empty list.

### The acceptance test in `src/hook_run/mod.rs`

Change `resolve_memory_client_adds_no_notice_for_a_bundle_without_content` (around line 4001), and rename it to `resolve_memory_client_notices_a_bundle_without_content_once_per_session`.

1. Keep the existing setup: a bundle with no content folder that fires on tag `mytag`.
2. Give the bundle a name that no other test uses, for example `contentless-3218`.
3. Make a temp state folder, and use session id `s-3218`.
4. Run the first call inside `crate::test_log_capture::capture_logs`. Assert the captured text contains `This notice shows once per session` exactly once.
5. Run a second, identical call inside a new `capture_logs`. Assert the captured text does not contain `This notice shows once per session`.
6. Assert both calls return `None`, as today.

Count the text `This notice shows once per session`, not the bundle name.
`capture_logs` captures every level, and `build_bundle_refs` (`src/cli/mod.rs`, around line 2599) logs the bundle name at debug level on every call.
Count the log lines from `record_logged`, not the notice list: `agent_notice` drops a repeated text within one process, so the list cannot show a second notice.

## Docs

Edit `website/docs/troubleshooting.md`, section `## The agent sees an llmenv notice`, at the bullet for "memory <event> skipped: the memory backend failed to load" (around lines 377-380).
Remove the sentence "A bundle with no content directory is not reported here, because its declaration cannot be read."
Add a new bullet for the notice from Design step 3, with its cause and its fix, and this note:

```markdown
The notice shows once per session for each bundle (added in v3.14.0).
```

## Changelog

Add one entry under `## [Unreleased]` → `### Fixed` in `CHANGELOG-3.md`:

```markdown
- Tell the agent once per session when a bundle fires with no content folder, so a memory backend that it may declare is not silently missing. [fix:hook] [#3218]
```

Take the tag from the commit that adds the notice.
Then run `scripts/sync-changelog-doc.sh` and commit `website/docs/changelog.md` with it.

## Out of scope

- Reading a bundle's memory declaration from a manifest or a cache (option 1 in the issue).
- Notices for the other `MemoryEndpoint` cases.

## Spec-writer calls

- A new per-session folder `bundle_notice/`, not `recall_health.json`, because the recall-health file is global.
- With no session id, every event gives the notice. A repeat is better than silence, and Claude Code always sends a session id.
- The notice fires for any content-less bundle, including one that declares no memory, because llmenv cannot read the declaration. Once per session keeps that cost low.
- The new state folder is pruned after 7 days, like the slippage stats.
