# Status line PR lookup

The `pr` widget shows the open PR of the current branch.
It reads `gh pr view`, which takes about 2 seconds on a machine with the account-selecting `gh` shim.
This file records the problem, the options, and the choice (#3889).

## Problem

The status line runs again on every prompt, and a render must not wait on the network.
The lookup had a hard limit of 3 seconds.
A load spike pushed it past the limit, and the widget then showed `#?` for a branch that has a PR.
The failure was cached for the same 60 seconds as an answer, so `#?` stayed on screen for a minute.

## Options

1. Keep the lookup in the render, and raise the limit.
   A slow `gh` then holds up the prompt for the whole limit.
2. Run the lookup in a detached child process, and let the render read the cache.
   The render never waits.
   The first render of a branch with no cache shows no PR, and the next prompt shows it.
3. Run the lookup in a thread of the render process.
   The process exits when the render ends, so the thread cannot outlive it.

## Choice

Option 2.
It is the only one where the limit can be longer than the time a person tolerates at the prompt.
The detached child is the pattern that `session_log::detached` and `hook_run::detached_store` already use.

## Behavior

- An answer is fresh for `PR_CACHE_TTL_SECS`, and a failure is fresh for `PR_FAILURE_TTL_SECS`.
  A timeout is transient, so a failure is retried sooner than an answer.
- A cache entry that is not fresh starts one refresh, and the render uses what it has.
  The refresh holds a system file lock on a file next to the cache entry for as long as it lives.
  The system frees the lock when the process ends, so a child that dies leaves no lock, and no age rule is needed.
  Two renders that both see no lock start two children, and the second child finds the lock held and ends.
  A lost refresh has no checkpoint, because the next render starts a new one.
  A child that finds the lock held ends with a debug line only, because that is the designed dedupe.
  The lock file stays after the refresh, and a clean-up of the state folder must keep it while a refresh runs.
- The cache entry keeps the last PR that a lookup returned.
  While that PR is younger than `STALE_PR_MAX_AGE_SECS`, a failed or due lookup shows it in the stale grey.
  The review-decision color is dropped, because the decision may have changed.
- A lookup that returns no PR clears the kept PR, so a merged or closed PR does not stay on the bar.
- An answer with a PR that is due and older than a day shows a failure, not the PR and not a blank, because a refresh that has not landed for a day is a fault.
- A time in the future reads as old, so a clock that stepped back keeps nothing alive.
- Output of `gh` that is not a PR is a failure, and it does not clear the kept PR. A PR view that exits 0 holds a PR, and "no PR" is a non-zero exit.
- A refresh that cannot start shows a failure when there is nothing cached, so it does not look like "no PR".
- The refresh child takes the cache folder from its own environment, and not from its input.
- With no cache at all, the render shows nothing for that branch until the refresh lands.
- With no state folder to hold a cache (`CLAUDE_CONFIG_DIR` is not set), the render runs `gh` itself with the short limit, and it waits.
  This is the behavior from before this change. The engine sets `CLAUDE_CONFIG_DIR`, so it only shows for a manual run.
- A failure with no kept PR shows `#?` and the cause, such as `#? auth` or `#? timeout`.

## Not in this change

The `gh` shim lives in the user's own llmenv config repository, not here.
The shim spends about 2 seconds, and a cache of the repo owner per directory would remove most of that.
That belongs to the shim.
