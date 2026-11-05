# Issues #3330 and #3083 — an unreadable working directory for content scopes

- **Issues:** https://github.com/phaedrus1992/llmenv/issues/3330 and https://github.com/phaedrus1992/llmenv/issues/3083
- **Milestone:** `v3.14.0`
- **Base branch:** `release/3.x`
- **Code:** `report_walk_error` and `matches_content_all` in `src/scope/content_walk.rs`
- **Type:** small behavior change (notice text), one test, docs

## Problem

Content scopes match on files in the working directory.
`matches_content_all` (`src/scope/content_walk.rs:30`) walks the directory once for every content scope.
When the walk cannot read the working directory itself, the function returns an empty set.
Each content scope then reads as unmatched, which is different from "llmenv does not know".

The agent already gets a notice for that case (#3070).
`report_walk_error` (`src/scope/content_walk.rs:7-17`) records it at walk depth 0 with `crate::agent_notice::record_logged`.
No test covers it, and the notice does not say that the match state is unknown.

## Decision

Owner decision on #3330 (2026-11-03), option 3:

> Keep the set of matched scopes. An unreadable working directory shows as unknown only in the agent notice that #3070 added. Implementation: document this in `website/docs/`, and add a test that an unreadable working directory records the notice. Then close this issue and #3083. The `ActiveScopes` contract does not change.

#3083 closes together with #3330, per the owner comment on #3083.

## What does not change

- The signature and the return value of `matches_content_all`. It still returns an empty set for an unreadable working directory.
- `ActiveScopes`, `ActiveScope`, and every consumer: the ICM header and tags (`src/icm.rs`), plugin resolution (`src/plugins/resolve.rs`), and the `LLMENV_ACTIVE_SCOPES` family of environment variables.
- The only production caller, the call to `matcher::matches_content_all` in `src/scope/mod.rs`, and its guard `cwd.exists() && !cfg.scope.content.is_empty()`.
- The deeper-level walk errors (depth greater than 0). They stay at `tracing::warn!`.

## Change 1: the notice says the state is unknown

In `report_walk_error`, replace the depth-0 notice text.

Today:

```text
llmenv: content scopes cannot read the working directory {cwd}, so none of them match ({err}). Fix: check the permissions of the working directory.
```

New text:

```text
llmenv: content scopes cannot read the working directory {cwd} ({err}), so their match state is unknown. llmenv treats each content scope as unmatched. Fix: check the permissions of the working directory.
```

Keep `{cwd}` as `cwd.display()` and `{err}` as the walkdir error, as today.
Keep the call to `crate::agent_notice::record_logged`.

Also update the doc comment on `matches_content_all` (`src/scope/content_walk.rs:27-28`) to this text:

```rust
/// A working directory that cannot be read gives no matches. The match state is then unknown,
/// and the agent notice says so (#3070, #3330). The returned set does not mark the unknown state.
```

Spec-writer call: the decision says the unknown state shows "only in the agent notice", and today's notice does not use the word.
The text change is the smallest change that makes the notice carry that state.

## Change 2: a test for the notice

Add one test to the `tests` module of `src/scope/content_walk.rs`.

Name: `an_unreadable_working_directory_records_the_unknown_notice`.

Rules for the test:

1. Mark it `#[cfg(unix)]`, and use `std::os::unix::fs::PermissionsExt`.
2. Create a `tempfile::tempdir()`, then a subfolder `work` in it, then one file `work/marker.txt`.
3. Build one content scope whose glob matches `marker.txt`. Build it the same way as the test `matches_content_all_evaluates_every_scope_in_one_walk` in `src/scope/matcher.rs` (around line 685).
4. Set `work` to mode `0o000` with `std::fs::set_permissions`.
5. If `std::fs::read_dir(&work).is_ok()` still succeeds, the test runs as root. Restore mode `0o755`, then fail with the message `Run the test as a non-root user` (#3086). Copy this pattern from `propagates_permission_error_instead_of_masking_absent` in `src/memory/mod.rs:267-292`.
6. Call `matches_content_all(&scopes, &work)`.
7. Restore mode `0o755` on `work` before any assertion, so the temp folder can be removed.
8. Assert that the returned set is empty.
9. Read `crate::agent_notice::current().unwrap_or_default()`. Assert that it contains all three of these:
   - the text `work.display().to_string()`;
   - the text `match state is unknown`;
   - the text `Fix:`.

The notice list is process-wide and shared by every test in the binary.
Assert only `contains` on the text above, never equality.
The temp path is unique, so the path check cannot match a notice from another test.

## Change 3: docs

Edit `website/docs/configuration.md`, section `## scope:`, at the **Content** bullet (around lines 172-180).
Add these sentences after the existing description of `glob` and `depth`:

```markdown
If llmenv cannot read the working directory, the match state of each content scope is unknown.
llmenv treats each content scope as unmatched, and the agent gets a notice that names the directory (changed in v3.14.0).
```

Edit `website/docs/troubleshooting.md`, section `## The agent sees an llmenv notice`.
Add one bullet for the notice text from Change 1, with its cause (the working directory is not readable) and its fix (correct the folder permissions).
Use the same bullet format as the other entries in that section.

## Changelog

Add one entry under `## [Unreleased]` → `### Changed` in `CHANGELOG-3.md`:

```markdown
- Say in the content-scope notice that an unreadable working directory leaves each content scope's match state unknown. The scopes still count as unmatched. [fix:scope] [#3330]
```

Take the `[type:scope]` tag from the commit that changes `report_walk_error`, as the `keepachangelog` rules say.
Then run `scripts/sync-changelog-doc.sh` and commit `website/docs/changelog.md` with it.

## Closing

The PR closes #3330 and #3083.
The base is `release/3.x`, so GitHub does not close them on merge.
Close both by hand after the merge, with `close-merged-refs.sh`.

## Out of scope

- An `unknown` kind in `ActiveScope`, or a tri-state result from `matches_content_all`. The owner rejected both (options 1 and 2).
- A notice for a deeper walk error.
