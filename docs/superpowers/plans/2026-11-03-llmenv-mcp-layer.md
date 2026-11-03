# llmenv MCP Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan inline, task by task. Do not use superpowers:subagent-driven-development (project rule).
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put a small llmenv-owned tool set in front of ICM, tag stored memories with a kind, and remove the Python `mcp-proxy` from the ICM path.

**Architecture:** One internal ICM adapter holds the transport. Hooks and a new stdio MCP server share it. The server exposes five tools to the model. Phase 1 replaces the proxy with `icm serve --http`.

**Tech Stack:** Rust, `rmcp` (official MCP SDK, pinned by #2160), `icm` 0.11.5 or later, existing `McpHttpClient`.

**Spec:** `docs/superpowers/specs/2026-11-03-llmenv-mcp-layer-design.md`. Read it first. It holds the reasons and the owner decisions.

## How this plan runs

- Base branch `release/3.x`. Milestone v3.15.0. Issue #3104 is the parent.
- File one issue per phase before you start it. Link each to #3104. Run each phase as a dev-sprint.
- Phase 1 is gated by Task 1.1. Phase 2 does not depend on phase 1. Phase 3 needs phase 2. Phase 4 needs phase 3.
- Expect forward-merge conflicts from `release/3.x` into 4.x. The 4.x line has a separate `llmenv-mcp` crate, and earlier sprints needed hand merges there.
- #2160 (rmcp client) and #2161 (rmcp bridge) are on v3.15.0. Phase 1 may supersede #2161. Phase 2 works over today's `McpHttpClient`, so it does not wait for #2160.

## Global Constraints

- Tool names the model sees: `remember`, `recall`, `list`, `update`, `forget`. No other model-facing memory tool.
- Kinds: `log`, `decision`, `design`, `lesson`, `rule`. No kind means unknown.
- Kinds and tags ride in ICM `keywords` as `kind:<kind>` and `tag:<tag>`. The `kind:` prefix is reserved.
- Only memories tagged `kind:log` are skipped by injection. A memory with no kind is injected as today.
- The raw `mcp__icm__*` entry leaves the model's MCP config only. Hooks and doctor keep the full surface through the adapter.
- Latency targets, to be replaced by measurement: a warm call adds under 20 ms over a direct call, and session start adds under 150 ms.
- ICM reference: tag `icm-v0.11.5`. `POST /mcp` is served by `icm serve --http <ADDR> --token <T>`. A non-loopback bind needs a token.
- Runtime ICM access goes through MCP or HTTP. Never call the `icm` CLI at runtime. Launching `icm serve` is the one allowed use.
- The layer keeps no state to catch duplicate stores. It relies on the Claude Code version floor that doctor reports (#2551).
- Do not touch `MemoryType` (`crates/llmenv-config/src/schema.rs`). It classifies llmenv context chunks and is a different axis from `kind`.
- Every user-visible change needs a `CHANGELOG-3.md` entry under `[Unreleased]` and a docs update in `website/docs/` with a version tag.
- Module budget: a Rust file stays under its cap. Extract a module when a task would grow a file past it.
- `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --check` pass before each commit.
- Start the `llmenv task` for a step in its own Bash call before the commit. Write commit message files with the Write tool.

## Review Focus

Each line has its test in the task named at the end.

1. The token file is missing or unreadable on the ICM host: export must report the path and the fix, and must not start an unauthenticated non-loopback daemon. (Task 1.2, 1.3)
2. The token must not land in a generated engine config file. The header uses env expansion. (Task 1.4)
3. A model-supplied tag that starts with `kind:` or holds a bad character must be rejected, so the model cannot forge a kind. (Task 2.1)
4. `recall` with an empty query, a huge `limit`, or no match returns a clear result. It never panics and never returns an unbounded list. (Task 3.2)
5. `update` or `forget` with an unknown id returns an error naming the id, not a silent success. (Task 2.2)
6. ICM without `--http`, or older than the floor, gives a doctor message with the fix, and the old proxy path still works until the cut-over. (Task 1.5)

---

## Phase 1: drop the Python proxy

### Task 1.1: Probe `/mcp` with Claude Code (gate)

**Files:**
- Create: `docs/design/issue-3104-phase1-probe.md` (the recorded result)

**Interfaces:**
- Produces: a pass or fail verdict that gates Tasks 1.2 to 1.5. On fail, stop and do #2161 instead.

- [ ] **Step 1:** Run `icm serve --http 127.0.0.1:<free port> --token <random>` against a scratch database (`--db .tmp/probe.db`).
- [ ] **Step 2:** In a scratch `CLAUDE_CONFIG_DIR`, add a remote MCP entry for `http://127.0.0.1:<port>/mcp` with header `Authorization: Bearer ${LLMENV_ICM_TOKEN}`, and export the variable. Run `claude -p` to list tools, store one memory, and recall it.
- [ ] **Step 3:** Run llmenv's own `McpHttpClient` against the same URL: `probe`, `list_tools`, and `call_tool("icm_memory_recall", ...)`. Use a small test in `src/hook_run/mcp_client/tests.rs` that is `#[ignore]` and documents the manual run.
- [ ] **Step 4:** Check whether `icm serve` reads the token from an environment variable or a file. If it takes `--token` only, record that the token shows in the process list.
- [ ] **Step 5:** Write the verdict, the Claude Code version, and the ICM version in the probe document. Commit.

### Task 1.2: Token file

**Files:**
- Create: `src/mcp/icm_token.rs`
- Modify: `src/mcp/mod.rs` (declare the module)

**Interfaces:**
- Produces:
  - `pub(crate) fn token_path(state_dir: &Path) -> PathBuf` returns `<state_dir>/icm-token`.
  - `pub(crate) fn load_or_create(state_dir: &Path) -> anyhow::Result<String>` for the ICM host.
  - `pub(crate) fn load(state_dir: &Path) -> anyhow::Result<Option<String>>` returns `None` when the file is absent.

- [ ] **Step 1:** Write tests: `load_or_create_makes_an_owner_only_file`, `load_or_create_returns_the_same_token_twice`, `load_returns_none_when_absent`, `load_names_the_path_when_unreadable`.
- [ ] **Step 2:** Run `cargo test --lib icm_token`. Expect FAIL: module missing.
- [ ] **Step 3:** Implement with `crate::paths::write_owner_only_atomic` and a CSPRNG crate already in `Cargo.lock`. Do not add a dependency without checking the lockfile first.
- [ ] **Step 4:** Run the tests. Expect PASS. Commit `feat(mcp): store the ICM daemon token`.

### Task 1.3: Spawn `icm serve --http`

**Files:**
- Modify: `src/mcp/proxy.rs` (`spawn_mcp_proxy` at line 813 becomes `spawn_icm_http`)
- Delete: `src/mcp/proxy/launcher.rs`
- Modify: `src/cli/mod.rs` (`ensure_local_memory_proxy`, line 1231)

**Interfaces:**
- Consumes: `icm_token::load_or_create` (Task 1.2), `ensure_running(bind, pid_path, spawn)`.
- Produces: `pub(crate) fn spawn_icm_http(bind: &str, pid_path: &Path, source: SpawnSource, token: &str) -> anyhow::Result<Child>`. The command is `<icm> serve --http <bind> --token <token>`, with `pin_icm_serve_dir` and the same detached stdio as today.

- [ ] **Step 1:** Write tests: `spawn_icm_http_builds_the_expected_command` (assert the argument list with a stub program path), `ensure_local_memory_proxy_fails_with_a_notice_when_the_token_cannot_be_written` (the error names the path).
- [ ] **Step 2:** Run them. Expect FAIL.
- [ ] **Step 3:** Implement. Refuse a non-loopback `listen_host` when no token exists. Remove `mcp_proxy_command`, `uvx` handling, and their tests, since nothing else uses them.
- [ ] **Step 4:** Run `cargo test --lib proxy` and `cargo test --lib ensure_local_memory_proxy`. Expect PASS. Commit.

### Task 1.4: Clients send the token

**Files:**
- Modify: `src/mcp/resolve.rs` (`resolve_memory`, line 281)
- Modify: `src/hook_run/memory_endpoint.rs` (`resolve_memory_client`, line 29)

**Interfaces:**
- Consumes: `icm_token::load`.
- Produces: `resolve_memory(mem, hosts, token: Option<&str>)`. When a token exists, the engine entry gets header `Authorization: Bearer ${LLMENV_ICM_TOKEN}`, and `llmenv export` exports `LLMENV_ICM_TOKEN` on a host that holds the token. The hook client reads the token file directly and sends the literal value.

- [ ] **Step 1:** Write tests: `resolve_memory_renders_a_bearer_header_with_env_expansion`, `resolve_memory_adds_no_header_without_a_token`, `the_token_value_never_appears_in_a_rendered_config`.
- [ ] **Step 2:** Run them. Expect FAIL.
- [ ] **Step 3:** Implement. A host without the token file behaves as today, so remote hosts with an old proxy keep working.
- [ ] **Step 4:** Run `cargo test --lib resolve`. Expect PASS. Commit.

### Task 1.5: Doctor, cut-over notice, docs

**Files:**
- Modify: the memory section of `src/cli/doctor/` (find it with `rg "memory" src/cli/doctor`)
- Modify: `website/docs/` memory page, `CHANGELOG-3.md`

**Interfaces:**
- Consumes: ICM `GET /health` (no token needed).

- [ ] **Step 1:** Write tests: `doctor_warns_when_the_listener_is_an_old_mcp_proxy` (health route missing), `doctor_names_the_icm_version_floor`.
- [ ] **Step 2:** Implement the checks. The warning for an old proxy gives the exact command: `pkill -f 'mcp-proxy .*-- icm serve'`. Never kill by pidfile.
- [ ] **Step 3:** Update the docs: no Python is needed. Add the version tag. Add the changelog entry `[feat:mcp]`. Run `scripts/sync-changelog-doc.sh`.
- [ ] **Step 4:** Run `cargo test --lib doctor`. Commit. If #2161 is now superseded, comment on it.

---

## Phase 2: one ICM adapter

### Task 2.1: Kinds and tags

**Files:**
- Create: `src/memory/kind.rs`
- Modify: `src/memory/mod.rs` (declare the module)

**Interfaces:**
- Produces:
  - `pub(crate) enum Kind { Log, Decision, Design, Lesson, Rule }`
  - `Kind::keyword(self) -> String` returns `kind:<name>`.
  - `Kind::from_keywords(words: &[String]) -> Option<Kind>`.
  - `pub(crate) fn tag_keyword(tag: &str) -> Result<String, KindError>` returns `tag:<tag>`. Reuse `is_valid_tag_charset` and `MAX_TAG_LEN` from `crate::scope::tags`. Reject a tag that starts with `kind:`.

- [ ] **Step 1:** Write tests: `kind_round_trips_through_its_keyword`, `from_keywords_reads_the_first_kind`, `unknown_words_give_no_kind`, `a_tag_with_the_kind_prefix_is_rejected`, `a_tag_with_a_bad_character_is_rejected`.
- [ ] **Step 2:** Run `cargo test --lib kind`. Expect FAIL.
- [ ] **Step 3:** Implement. Run the tests. Expect PASS. Commit.

### Task 2.2: The adapter

**Files:**
- Create: `src/memory/backend.rs` (types and trait), `src/memory/icm_adapter.rs`

**Interfaces:**
- Produces in `backend.rs`:
  - `pub(crate) struct Remember { content: String, kind: Kind, topic: Option<String>, tags: Vec<String>, importance: ImportanceLevel }`
  - `pub(crate) struct RecallQuery { query: String, kind: Option<Kind>, tags: Vec<String>, project: Option<String>, limit: usize }`
  - `pub(crate) struct ListQuery { kind: Option<Kind>, tags: Vec<String>, topic: Option<String>, project: Option<String>, limit: usize }`
  - `pub(crate) struct Hit { id: String, topic: String, summary: String, kind: Option<Kind>, tags: Vec<String> }`
  - `pub(crate) trait MemoryBackend { async fn remember(&self, r: Remember) -> anyhow::Result<String>; async fn recall(&self, q: RecallQuery) -> anyhow::Result<Vec<Hit>>; async fn list(&self, q: ListQuery) -> anyhow::Result<Vec<Hit>>; async fn update(&self, id: &str, content: Option<String>, kind: Option<Kind>, tags: Option<Vec<String>>) -> anyhow::Result<()>; async fn forget(&self, id: &str) -> anyhow::Result<()>; }`
- Produces in `icm_adapter.rs`: `pub(crate) struct IcmAdapter { client: McpHttpClient }`, `IcmAdapter::new(client: McpHttpClient) -> Self`, `impl MemoryBackend for IcmAdapter`.
- Behavior: `recall` calls `icm_memory_recall` in JSON format and reads `keywords` from each result. It leaves out `kind:log` hits unless `q.kind == Some(Kind::Log)`. It filters by tags after the call and over-fetches to make up the limit. `list` approximates a listing with recall and the `keyword` filter. Document the limits in the type's doc comment.

- [ ] **Step 1:** Capture one real recall JSON reply from `icm` 0.11.5 as `src/memory/testdata/recall.json`. Assert in a test that each result has `keywords`. If it does not, stop and use the fallback: store logs under a `logs-<project>` topic, and tell the owner.
- [ ] **Step 2:** Write tests with a stub MCP server (the one in `src/hook_run/mcp_client/tests.rs`): `remember_sends_kind_and_tags_as_keywords`, `recall_leaves_out_logs_by_default`, `recall_returns_logs_when_the_kind_is_log`, `recall_filters_by_two_tags`, `update_with_an_unknown_id_errors_naming_the_id`, `forget_with_an_unknown_id_errors_naming_the_id`.
- [ ] **Step 3:** Run them. Expect FAIL.
- [ ] **Step 4:** Implement. Run `cargo test --lib icm_adapter`. Expect PASS. Commit.

### Task 2.3: Move the hook call sites

**Files:**
- Modify: `src/session_log/dispatch.rs` (3 calls), `src/hook_run/action.rs`, `src/hook_run/adaptive/failure.rs`, `src/hook_run/detached_store.rs`, `src/consolidation/dedup.rs`, `src/consolidation/mod.rs` (2), `src/memory/mod.rs`, `src/memory/prune.rs`

**Interfaces:**
- Consumes: `IcmAdapter`. Add the hook-only calls to `IcmAdapter` as inherent methods, not to the trait: `wake_up`, the `icm_transcript_*` family, `stats`, `list_topics`, `health`.
- Behavior must not change.

- [ ] **Step 1:** Move one file at a time, in the order of the Files list. Replace `client.call_tool(...)` with the adapter method. Keep the argument JSON identical.
- [ ] **Step 2:** After each file, run `cargo test --lib` for that module. The existing tests are the check. Do not change their assertions.
- [ ] **Step 3:** After the last file, run `rg "call_tool\(" src -g '!**/*test*'`. Expect hits only inside `icm_adapter.rs`.
- [ ] **Step 4:** Run the full `cargo test --lib`. Commit per file group with `refactor(memory): route <area> through the ICM adapter`.

---

## Phase 3: the model-facing server

### Task 3.1: The `rmcp` dependency

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`, `THIRD-PARTY-LICENSES.md`, `website/docs/third-party-licenses.md`

- [ ] **Step 1:** If #2160 landed, reuse its pinned version and features. Otherwise look up the current stable `rmcp` release and pin it exactly (no `^` or `~`). Enable the server and stdio-transport features only.
- [ ] **Step 2:** Run `cargo deny check`. Run `scripts/gen-attribution.sh` and commit the regenerated files with the dependency.
- [ ] **Step 3:** Run `cargo build`. Commit `chore(deps): add rmcp`.

### Task 3.2: The five tools

**Files:**
- Create: `src/mcp_server/mod.rs`, `src/mcp_server/tools.rs`

**Interfaces:**
- Consumes: `MemoryBackend`, `Kind`, `tag_keyword`.
- Produces: `pub(crate) async fn serve<B: MemoryBackend + Send + Sync + 'static>(backend: B) -> anyhow::Result<()>` runs a stdio MCP server with the tools `remember`, `recall`, `list`, `update`, `forget`. Each description is at most two sentences and says when to use the tool. `remember` says to pick one kind. `const TOOL_SCHEMA_BUDGET_CHARS: usize = 4000` caps the serialized `tools/list`.
- Limits: `recall` and `list` cap `limit` at a named constant and default it to a named constant.

- [ ] **Step 1:** Write tests with a fake `MemoryBackend`: `tools_list_returns_exactly_the_five_names`, `tools_list_stays_under_the_schema_budget`, `remember_passes_kind_tags_and_importance`, `recall_with_an_empty_query_returns_a_clear_error`, `recall_caps_a_huge_limit`, `a_backend_error_reaches_the_model_with_cause_and_fix`.
- [ ] **Step 2:** Run `cargo test --lib mcp_server`. Expect FAIL.
- [ ] **Step 3:** Implement with `rmcp`. Run the tests. Expect PASS. Commit.

### Task 3.3: The `llmenv mcp serve` command

**Files:**
- Create: `src/cli/mcp_serve.rs`
- Modify: `src/cli/mod.rs` (add a `Command::Mcp` variant next to `Command::HookRun`, line 305)

**Interfaces:**
- Consumes: `resolve_memory_client`, `IcmAdapter`, `mcp_server::serve`.
- Behavior: `llmenv mcp serve` resolves the memory endpoint as a hook does, builds the adapter, and serves on stdio. With no memory backend it exits with an error that names the config key to set.

- [ ] **Step 1:** Write an integration test in `tests/mcp_serve.rs`: start a stub ICM HTTP server, run the built binary, send `initialize`, `tools/list`, and a `remember` call over stdio. Assert that the stub received `icm_memory_store` with `kind:decision` in the keywords.
- [ ] **Step 2:** Run `cargo test --test mcp_serve`. Expect FAIL.
- [ ] **Step 3:** Implement. Run the test. Expect PASS. Commit.

### Task 3.4: Render the entry, with the raw toggle

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs` (`Memory`: add `expose_raw_tools: bool`, default `false`), `src/mcp/resolve.rs`, `src/adapter/claude_code/mod.rs` (line 642 area)

**Interfaces:**
- Produces: a resolved stdio entry named `llmenv` with `command` set to the same absolute llmenv path that the hook table uses, and `args` `["mcp", "serve"]`. Reuse the existing function that writes the hook command path. Do not write a second one. When `expose_raw_tools` is `false`, the `icm` entry is not rendered for the model. Hooks do not read the engine entry, so they are unaffected.

- [ ] **Step 1:** Write tests: `memory_renders_the_llmenv_entry_and_no_icm_entry_by_default`, `expose_raw_tools_true_renders_both`, `the_llmenv_entry_uses_the_hook_command_path`.
- [ ] **Step 2:** Run them. Expect FAIL. Implement. Expect PASS.
- [ ] **Step 3:** Add `expose_raw_tools` to the known-keys check (`unknown_keys.rs`) and to the docs with the version tag. Add the changelog entry, and say that `mcp__icm__*` permission rules stop applying to the model.
- [ ] **Step 4:** Commit.

### Task 3.5: Tiers and engine parity

**Files:**
- Modify: `src/adapter/mcp_tiers.rs`, `src/adapter/claude_code/mod.rs` (`apply_mcp_tier_permissions`, line 2394), the opencode and Crush adapters (find the ICM tier use with `rg "ICM_READ_ONLY" src/adapter`)

**Interfaces:**
- Produces: `LLMENV_READ_ONLY = ["recall", "list"]`, `LLMENV_MUTATION = ["remember", "update"]`, `LLMENV_DESTRUCTIVE = ["forget"]`. The Claude Code prefix is `mcp__llmenv__`.

- [ ] **Step 1:** Write tests per adapter: `forget_asks_and_recall_is_allowed` for the new names, with the default policy of `tier_action`.
- [ ] **Step 2:** Run them. Expect FAIL. Implement. Expect PASS. Commit.
- [ ] **Step 3:** If an engine cannot render a stdio MCP entry, record that in the docs and in the phase issue. Do not fake it.

### Task 3.6: Doctor, audit, measurement

**Files:**
- Modify: `src/cli/doctor/` (memory section)
- Create: `docs/design/issue-3104-latency.md`

- [ ] **Step 1:** Doctor spawns `llmenv mcp serve`, sends `initialize` and `tools/list`, and reports a cause and fix on failure or a timeout. Test with a stub.
- [ ] **Step 2:** Audit: run `rg "icm_memory|mcp__icm__" bundles website/docs docs examples`. Update every skill, rule, and doc that names a raw tool the model no longer sees.
- [ ] **Step 3:** Measure session start and a warm `recall` against a direct ICM call. Record the numbers and the machine in the latency document. Compare with the targets in Global Constraints. If a target fails, say so in the phase issue and ask the owner.
- [ ] **Step 4:** Commit.

---

## Phase 4: kind-aware injection

### Task 4.1: Skip logs in injection

**Files:**
- Modify: the hook recall path (`src/hook_run/adaptive/waves.rs` and `src/hook_run/action.rs`; find the place that turns recall results into injected text)

**Interfaces:**
- Consumes: `IcmAdapter::recall` and `Kind::from_keywords`.
- Produces: `fn injectable(hits: Vec<Hit>) -> Vec<Hit>` drops hits whose kind is `Some(Kind::Log)` and keeps the rest.

- [ ] **Step 1:** Write tests: `a_log_is_not_injected`, `a_memory_with_no_kind_is_injected`, `a_decision_is_injected`.
- [ ] **Step 2:** Run them. Expect FAIL. Implement. Expect PASS.
- [ ] **Step 3:** `icm_wake_up` text is built by ICM, so kinds cannot filter it. Record this limit in the docs.
- [ ] **Step 4:** Commit.

### Task 4.2: Docs and wrap-up

- [ ] **Step 1:** Document kinds, tags, and what injection skips, in `website/docs/` with the version tag. Add the changelog entry.
- [ ] **Step 2:** Run the full `cargo test --lib`, clippy, and fmt. Run the mutation check on the changed lines if CI asks.
- [ ] **Step 3:** Comment on #3104 and on each phase issue with what shipped and what is deferred. File an issue for every deferral, with a milestone and labels.
- [ ] **Step 4:** Commit and open the PR through `ship-issue`.
