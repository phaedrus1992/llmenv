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
  A lock file next to the cache entry keeps two renders from starting two refreshes.
  A lock older than `REFRESH_LOCK_STALE_SECS` belongs to a child that died, so it is replaced.
- The cache entry keeps the last PR that a lookup returned.
  While that PR is younger than `STALE_PR_MAX_AGE_SECS`, a failed or due lookup shows it in the stale grey.
  The review-decision color is dropped, because the decision may have changed.
- A lookup that returns no PR clears the kept PR, so a merged or closed PR does not stay on the bar.
- With no cache at all, the render shows nothing for that branch until the refresh lands.
- With no state folder to hold a cache, the render runs `gh` itself with the short limit, as before.
- A failure with no kept PR shows `#?` and the cause, such as `#? auth` or `#? timeout`.

## Not in this change

The `gh` shim lives in the user's own llmenv config repository, not here.
The shim spends about 2 seconds, and a cache of the repo owner per directory would remove most of that.
That belongs to the shim.
