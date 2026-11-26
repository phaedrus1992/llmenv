# Unattended run report: 2026-11-25 (milestone v3.14.2)

Branch `unattended/local-main`, based on `origin/release/3.x` (9366439f).
It holds 35 commits, and 3 of them are sprint merge commits.
The commits are not signed yet.
The run disabled signing, and `scripts/resign.sh` signs every commit again before the push.
Nothing was pushed, and no PR is open.

## Rules of the run

1. Each sprint had its own branch, cut from the integration branch.
2. Each sprint passed the repo gate (`prek run --hook-stage pre-push --all-files`)
   and the full test suite before its local merge.
3. Each sprint had a strict `nbl-dev:pre-pr-review`, with no push and no PR.
4. The last step was a strict review of the whole branch against `origin/release/3.x`.
5. A question for the owner went to a comment on the issue, and the issue went on the blocked list.
6. No issue was closed. The PR closes them.

## Work merged

Sprint 1, composite #4049, scratch files in `TMPDIR`:

- #4046: `cache.tmp_retention_hours` (default 168) is the keep time of `state/tmp`.
  `llmenv clean --tmp-older-than DUR` replaces it for one run, and `--dry-run` lists each entry with its age.
  A zero value is refused.
  A null `cache_retention_hours` still turns off every age sweep.
- #4045: `llmenv tmp-guard` denies a write under `/tmp` and names `$TMPDIR` as the fix.
  It denies a call that it cannot check.

Sprint 2, composite #4056, follow-ups of the tmp guard:

- #4052: the guard also runs on opencode and Crush.
- #4053: the guard covers `/var/tmp`, `/dev/shm`, and more writers.
  The writers are `git clone`, `tar`, `curl`, `wget`, `rsync`, and the opencode `patch` tool.
- #4054: the state gate and the sweep share one age source, and a dry run carries the age of each entry.

Sprint 3, #4055, refactor:

- `cli::run` is a table of 8 group functions in `src/cli/dispatch.rs`.
- `generate_settings_json` calls section modules `settings_hooks.rs` and `settings_permissions.rs`.
- The Crush `materialize` is a set of section functions in `src/adapter/crush/assemble.rs`.
- No function that the sprint added or changed has a complexity above 8.
  The tests and the settings snapshot show that behavior did not change.
  The refactor is not user-visible, so it has no changelog entry.

## Work not done

| Issue | State | Why |
| --- | --- | --- |
| #4057 | blocked | The Crush `download` argument name is unknown. The clone of the Crush source needs the owner. |
| #4035 | blocked | The owner picks the fix for the serialized stdio pipe. The recommendation is #2909 first. |
| #3911 | blocked | The owner decides the sample window of the usage delta. The recommendation is the zone width. |
| #3633 | blocked | It needs the `icm` reply format from its source. The clone needs the owner. |
| #4058 | filed | 21 functions in `cli/mod.rs` and `claude_code/mod.rs` still have a complexity above 8. |
| #4059 | filed | `every_subcommand_has_a_group` counts subcommands, but does not check each group. |
| #4060 | filed | `run_memory` in `cli/dispatch.rs` has no test. |
| #4061 | filed | The tmp guard misses `find -exec`, `sed -i`, `git worktree add`, `$'..'`, and variable targets. |
| #4062 | filed | The tmp guard fails open on Claude Code and Crush when `llmenv` cannot start. |

## Review of the whole branch

Six analyzers and crap-score ran:
`semgrep-scanner`, `overengineering-reviewer`, `silent-failure-hunter`,
`security-audit`, `property-test-gap-finder`, `variant-bug-hunter`, and `crap-score`.
Each sprint also had its own review before this one.

Findings of the whole-branch run: P0 0, P1 3, P2 22, plus 7 from the silent-failure hunt and 1 from crap-score.

Fixed:

- P1: the guard allowed a write after a shell reserved word, such as `for f in a b; do cp $f /tmp/; done`.
- `unzip -d` into `/tmp` was allowed.
- An ordinary deny was logged at `error` level.
- Property tests for the parsers and writers of the guard, the retention window, and the order of the Claude Code hook entries.
- The opencode test filter did not remove the built-in hooks, and two timeout lines were copied in `hook_bridge.rs`.
- The changelog and the docs now state the null window, all six guarded tools, and the fail-open case.
- Sprint 3 review: the Crush hook filter ran after the skill writes, so its warnings came late or not at all.

Dismissed, with the reason:

- Semgrep `rust-obfuscation-anonymous-nesting` at `settings_permissions.rs`: an ordinary closure over a slice.
- A `catch_unwind` fail-open for Claude Code: the release profile aborts on panic, and the crate denies `panic` and `unwrap`.
- A floor for `--tmp-older-than`: an explicit flag with `--dry-run`, as `--older-than` is. A zero value is refused.
- A deny for any `$` target: it would block `"$HOME/x"`. The gap is tracked in #4061.
- The `Routed` enum in `dispatch.rs`: the uniform result type matches the arms.

The semgrep run was incomplete: it skipped 3 to 5 Rust files for each ruleset, and it did not scan markdown.
The other analyzers cover those files.

## Lessons

- A refactor must keep the order of the code.
  The Crush split moved the hook filter after the skill writes.
  Only a line-by-line read of the old function caught it.
- A shell guard must skip the reserved words (`do`, `then`, `else`) before it reads the command.
- A complexity counter counts each `?` as a branch.
  An arm table and `result.map(|()| Done)` keep a dispatch function flat.
- The task hook blocks a commit until a task is wip.
  A subagent needs `llmenv task start --force` behind a parent task.
- `prek` stashes unstaged files at commit, so a commit must not run while a suite reads the tree.

## What the user must do

1. Unlock the signer, then run `scripts/signing.sh restore` and `scripts/resign.sh`.
2. Say go to push `unattended/local-main` and open one PR against `release/3.x`.
3. Answer #4035, #3911, #4057, and #3633 on the issues.
4. Confirm that the next release number is 3.14.2, because the docs tag the new features `(added in v3.14.2)`.
