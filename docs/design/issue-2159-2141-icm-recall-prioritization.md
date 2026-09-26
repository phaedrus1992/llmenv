# Issues #2159 and #2141 — ICM recall: byte budget and most-specific-first order

- **Issues:** https://github.com/phaedrus1992/llmenv/issues/2159 (part A), https://github.com/phaedrus1992/llmenv/issues/2141 (part B)
- **Milestones:** part A is `v3.11.2`; part B is `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** part A is a bug fix; part B is a feature

This is a spec, not a plan.
It says what the code must do and resolves every decision.
Part B builds on part A and must not start before part A merges.

## Problem

On every prompt, the `TurnStart` hook injects ICM recall text into Claude Code as `hookSpecificOutput.additionalContext`.
Claude Code saves any hook output above an inline limit to a file and shows the model only a preview of about 2 KB.
Measured on a real machine: 1,202 hook outputs were saved to a file, minimum 10,039 bytes, median 15,045 bytes, maximum 35,303 bytes.
The model therefore sees the first 2 KB of recall and loses the rest.

The first 2 KB is also the least useful part.
The recall order today is:

1. `Action::Recall` with `{"query": "<sorted tags joined by \", \">"}` and no `project` argument.
2. One `Action::RecallTag` per active tag, in alphabetical order.
3. One `Action::RecallBundle` per project-enabled bundle, in alphabetical order.

ICM defaults a missing `project` argument to the ICM server's own working-directory name.
For a remote `icm serve`, that name has no relation to the user's repository.
So step 1 returns memories from unrelated projects (observed: a Time Machine note, notes from other repositories), and those land at the top.

## Verified facts (release/3.x)

Implementers must not re-derive these.

| Fact | Location |
| --- | --- |
| Action order for `TurnStart` is `Recall`, then tags, then bundles | `dispatch()`, `src/hook_run/mod.rs` (the `HookEvent::TurnStart` arm near line 236) |
| Recall query is `tags.join(", ")` | `src/hook_run/mod.rs` near line 1090 |
| Recall arguments per action | `Action::arguments`, `src/hook_run/action.rs` near line 113 |
| `RecallTag`/`RecallBundle` pass `project: ""` and `keyword: llmenv-tag:<tag>` / `llmenv-bundle:<bundle>` | same |
| `Store` passes no `project`, so stored memories get the ICM server's default project | same |
| Actions run in order, results are exact-block deduped (first wins), then joined with `"\n\n"` | `run_memory_actions`, `dedup_and_count_action_results`, `src/hook_run/mod.rs` near lines 1324 and 1371 |
| Advisory lines (`no memories found…`, `[icm:…`) are stripped per action | `strip_advisory`, `src/hook_run/action.rs` near line 179 |
| The whole text is wrapped as `[ICM MEMORY CONTEXT (auto-injected)]\n<text>` | `emit_hook_context`, `src/adapter/mod.rs` near line 537 |
| Scope kinds are `project`, `content`, `network`, `user`, `host` | `src/scope/mod.rs` |
| `ActiveScope` has `kind`, `tags`, `project_root` (only for `project`), `enable_bundles` (only for `project`) | `src/scope/mod.rs` near line 38 |
| `ActiveScopes.extra_tags` holds `$LLMENV_EXTRA_TAGS`; some tags in `ActiveScopes.tags` come from no scope (for example the OS tag) | `src/scope/mod.rs` near line 66 and `non_project_tags()` |
| `recall_bundle_names()` returns only bundles from scopes' `enable_bundles`, minus disabled ones | `src/hook_run/mod.rs` near line 1723 |
| The `TurnStart` payload is Claude Code's `UserPromptSubmit` stdin JSON; the prompt text is `payload["prompt"]` | `event_content()`, `src/hook_run/mod.rs` near line 301 |
| ICM `icm_memory_recall` takes `query` (required), `keyword`, `topic`, `project`, `limit` (default 5, max 20) | ICM MCP tool schema |
| ICM recall output is one memory per record; a record starts with a line `[<topic>] <text>` | observed in saved hook output |
| ICM topic convention: `decisions-<project>`, `context-<project>`, `preferences`, `errors-resolved` | ICM server instructions |

## Part A (#2159, v3.11.2) — budget and order

Part A changes only the order of existing recalls and the amount of text kept.
It adds no new recall input.

### A1. Specificity rank

Give each tag a rank.
Lower rank means more specific, and it is injected first.

| Rank | Source of the tag |
| --- | --- |
| 1 | a `project` scope's `tags`, or `ActiveScopes.extra_tags` |
| 2 | a `content` scope's `tags` |
| 3 | a `network` scope's `tags` |
| 4 | a `user` scope's `tags` |
| 5 | a `host` scope's `tags` |
| 6 | a tag in `ActiveScopes.tags` that no scope contributed and that is not in `extra_tags` |

A tag that several scopes contribute takes the lowest rank among them.
Every bundle from `recall_bundle_names()` has rank 1, because only project scopes enable bundles.
`extra_tags` has rank 1 because it is the documented stand-in for a project's `.llmenv.yaml`.

Put the rank computation in one pure function in `src/hook_run/mod.rs`:
`fn tag_specificity(active: &ActiveScopes) -> BTreeMap<String, u8>`.
Do not add the rank to `ActiveScope` or `ActiveScopes`; the rank is a recall concern only.

### A2. New action order for `TurnStart`

1. Rank-1 recalls: all rank-1 `RecallTag` actions and all `RecallBundle` actions.
2. Rank-2 `RecallTag` actions.
3. Rank-3 `RecallTag` actions.
4. Rank-4 `RecallTag` actions.
5. Rank-5 `RecallTag` actions.
6. Rank-6 `RecallTag` actions.
7. `Action::Recall` (the query without a project filter) last, because it is the least specific.

Within one rank, sort by tag or bundle name, as today, so the output is deterministic.
Inside rank 1, the tag recalls come before the bundle recalls.

`dispatch()` keeps its signature shape, but it receives the ranked, ordered queries.
The `SessionStart` and `SessionEnd` arms do not change.

### A3. Split recall text into records

Add a pure function in `src/hook_run/action.rs`: `fn split_recall_records(text: &str) -> Vec<String>`.

- Input is one action's text after `strip_advisory`.
- A record starts at a line that matches `^\[[^\]\n]{1,200}\] ` (a `[`, 1 to 200 characters that are not `]` or a newline, `]`, and a space).
- Every following line that does not match belongs to the current record.
- Lines before the first matching line form one record of their own.
- If no line matches, the whole text is one record.
- Remove trailing whitespace from each record. Drop empty records.

Use the `regex` crate for the shape match.
`regex` 1.13.1 is already in `Cargo.lock` as a transitive dependency, so add it to the root `Cargo.toml` `[dependencies]` as `regex = "=1.13.1"` (exact pin, the same style as the other entries).
This adds no new crate to the build.
Compile each pattern once in a `static` `std::sync::LazyLock<regex::Regex>`.
Use `is_match` on each line; a line-anchored pattern with `^` is enough because the input is split into lines first.

This couples llmenv to ICM's text format.
The fallback rule (no match means one record) keeps the coupling safe: a format change degrades to today's per-action behavior and never loses text.

### A4. Budget

Add constants in `src/hook_run/mod.rs`:

```rust
/// Claude Code saves hook output over about 10 KB to a file and shows the
/// model a 2 KB preview. Stay well under that limit.
const RECALL_BUDGET_BYTES: usize = 8_000;
```

Do not make the budget configurable in part A.

`run_memory_actions` changes as follows:

1. Run recall actions in the A2 order, one at a time, as today.
2. Split each action's text into records (A3).
3. For each record, in order: skip it if an identical record (after trim) was already kept; else keep it if `kept_bytes + record.len() + 2 <= RECALL_BUDGET_BYTES`; else count it as omitted.
4. Once a record does not fit, keep going through the rest of this action's records, because a smaller one can still fit.
5. After an action, if `RECALL_BUDGET_BYTES - kept_bytes < 200`, do not run the remaining recall actions. Count their records as unknown, not as omitted.
6. Non-recall actions (`WakeUp`, `Store`) run as today and are not counted in the budget.
7. Join kept records with `"\n\n"`.
8. If any record was omitted, append one line: `[llmenv] <N> lower-priority memories omitted to stay under the context limit.`
   If recall actions were skipped in step 5, use: `[llmenv] <N> lower-priority memories omitted; <M> recall queries skipped to stay under the context limit.`

The budget covers only recall text.
The wrapper line and `append_pending_notice` text are small, so they stay outside it.
The whole `additionalContext` value must stay at or below 9,000 bytes; see acceptance criteria.

Replace `dedup_and_count_action_results` with record-level dedup.
Update `RecallStats` and `emit_context_trace` to count records, and add `omitted` and `skipped_actions` counters to the trace line.
Update the doc comment that says per-record parsing is avoided; it no longer holds.

### A5. Not changed in part A

- `emit_hook_context` stays as it is. Do not truncate in the emitter; a cut there would split a record.
- `throttle` output (`src/throttle/mod.rs`) is a short note and needs no budget.
- The `Action::Recall` query text stays `tags.join(", ")`. Part B changes it.
- `Store` arguments do not change.

### A6. Tests (part A)

All tests are unit tests with no live ICM.
Use the existing `dispatch` and action-argument test style in `src/hook_run/mod.rs` and `action.rs`.

1. `tag_specificity`: a tag in both a `host` scope and a `project` scope gets rank 1; an `extra_tags` tag gets rank 1; a scope-less tag gets rank 6; a `content` tag gets rank 2.
2. `dispatch(TurnStart, …)` order: rank-1 tags, then bundles, then ranks 2 to 6, then `Recall`.
3. `split_recall_records`: multi-line records; text with no `[` line gives one record; leading text before the first record; a `[` line without `"] "` is a continuation.
4. Budget: 30 records of 500 bytes give kept bytes at or below 8,000, whole records only, and an omission line with the right count.
5. Dedup: an identical record from a rank-1 action and from `Recall` is kept once, at the rank-1 position.
6. Skip: when the budget fills after the first action, later recall actions are not called (use a counting fake client) and the skip line appears.
7. Property test (`proptest`, already used in `action.rs`): for any list of records, output bytes are at or below 8,000 plus the omission line, and every kept record is byte-identical to an input record.

### A7. Acceptance criteria (part A)

1. On a real session with the maintainer's ICM store, the `additionalContext` value is at or below 9,000 bytes on every prompt.
   Check it with `LLMENV_TRACE_TIMING=1` and the trace line.
2. Claude Code no longer writes `*additionalContext.txt` files for llmenv hook output.
3. With a project scope active, the first injected record comes from a rank-1 recall, not from `Action::Recall`.
4. Changelog entry under `## [Unreleased]` in the 3.x changelog, section `Fixed`.
5. `Cargo.lock` changes because `regex` becomes a direct dependency. Run `scripts/gen-attribution.sh` and commit its output in the same change (AGENTS.md rule). The package set does not change, so expect no new license entries.
6. `website/docs/` memory page says that recall is capped and ordered by scope specificity, tagged `(changed in v3.11.2)`.

## Part B (#2141, v3.12.0) — recall on the work at hand

Part B adds two new, more specific recall inputs and a project-topic recall.
It reuses part A's records, dedup and budget unchanged.

### B1. New tier order

| Tier | Recalls |
| --- | --- |
| 0 | one "work" recall: `query` = work keywords (B2, B3), `project: ""`, `limit: 10` |
| 1 | two project-topic recalls: `topic: "context-<project>"` and `topic: "decisions-<project>"`, `project: ""`, `query` = work keywords or, if none, `<project>` |
| 2 | part A ranks 1 to 6, in part A order |
| 3 | `Action::Recall`, now with `project: ""` and `query` = the tag list, as today |

`<project>` is the file name of the `project_root` of the `project` scope with the longest `project_root` path (the innermost project).
If no `project` scope is active, skip tier 1.
If `<project>` is not valid UTF-8 or is empty, skip tier 1.

Pass `project: ""` on every recall in part B.
ICM's default project filter uses the server's own working directory, which is wrong for a remote server, and `Store` does not set a project either.

Add new `Action` variants `RecallWork(String)` and `RecallTopic { topic: String, query: String }`.
Do not overload `Action::Recall`.

### B2. Prompt keywords

Source: `stdin_payload["prompt"]` on `TurnStart`.

1. If the prompt is missing, empty, or starts with `/` (a slash command), there are no prompt keywords.
2. Take the first 2,000 characters.
3. Remove fenced code blocks (text between lines that start with three backticks).
4. Use the rest as-is as the query text; ICM does its own natural-language matching.
5. Cut the query to 500 characters at a character boundary.

Do not store the prompt anywhere new.
The prompt text goes to the ICM endpoint, which can be a remote server on the LAN.
Turn captures already go to the same endpoint through `Store`, so this adds no new destination.
State this in the docs (B6).

### B3. Branch keywords

Source: the current git branch of the innermost project root.

1. Find the git directory: if `<project_root>/.git` is a directory, use it; if it is a file (worktree), read `gitdir: <path>` from it and resolve a relative path against `project_root`.
2. Read `<gitdir>/HEAD`. If it starts with `ref: refs/heads/`, the rest (trimmed) is the branch. Otherwise (detached HEAD) there are no branch keywords.
3. No branch keywords for `main`, `master`, `develop`, `trunk`, or any branch that starts with `release/` or `forward-merge/`.
4. Match the shape with the regex `^[A-Za-z0-9._/-]{1,200}$` (the `regex` crate from part A). If it does not match, no branch keywords. Steps 5 to 8 then check the meaning of the parts in code.
5. Remove one leading prefix segment if it is one of: `feat`, `feature`, `fix`, `bugfix`, `hotfix`, `chore`, `docs`, `doc`, `refactor`, `perf`, `test`, `tests`, `build`, `ci`, `style`, `deps`, `renovate`, `dependabot`, followed by `/`.
6. Split the rest on `/`, `-`, `_`, `.`.
7. A part that is all ASCII digits, 1 to 7 digits long, is an issue number. Keep it as `#<n>` and as `<n>`.
8. Drop parts shorter than 3 characters, except issue numbers.
9. Drop duplicates. Keep order.
10. Join with spaces.

Do not spawn `git`.
Reading `HEAD` directly costs one small file read per prompt.

Cache the result per session the same way `read_once` and `repeat_detect` keep state: a JSON file at `<state_dir>/recall/<session_id>.json` holding `{"head": "<raw HEAD content>", "branch_keywords": "<string>"}`.
Recompute when the raw `HEAD` content differs.
Prune old files with the shared `prune_stale_json_files` helper in `src/hook_run/session_state.rs`, with the same age limit `read_once` uses.
If there is no session id, do not cache; compute every time.
A cache read or write error is not fatal: compute the keywords and continue.

### B4. Work keywords

Work keywords = branch keywords, then a space, then prompt keywords.
If both are empty, skip tier 0.
Tier 1 uses the same string as its `query`; if it is empty, tier 1 uses `<project>`.

### B5. Not in part B

- No config knob for tiers or budget. Add one only when a user asks for it.
- No change to `Store`. Setting `project` on `Store` is a separate change with a data-migration question; it is out of scope.
- No SessionStart injection; that is #2142.

### B6. Tests and acceptance (part B)

1. Branch parsing table test: `feat/2141-icm-overflow` gives `#2141 2141 icm overflow`; `main` gives nothing; `release/3.x` gives nothing; detached HEAD gives nothing; `fix/a-b` gives nothing (parts too short); a `.git` file with a relative `gitdir:` resolves.
2. Prompt keyword test: slash command gives nothing; fenced code is removed; a 5,000-character prompt gives a 500-character query at a character boundary with multi-byte text.
3. `dispatch` order test for tiers 0 to 3.
4. `RecallWork` and `RecallTopic` arguments carry `project: ""`.
5. Session-state cache test: same `HEAD` content does not re-read keywords; changed content does.
6. Acceptance: on a branch named for an issue, the first injected record mentions that issue when ICM holds a memory for it.
7. Changelog entry under `Added`; docs page describes the tiers and the prompt-to-ICM data flow, tagged `(added in v3.12.0)`.
