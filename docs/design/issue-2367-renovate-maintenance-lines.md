# Issue #2367 — Renovate: 1.x and 2.x get no lockfile regen, so no transitive CVE fixes

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2367
- **Milestone:** `v3.12.0`
- **Base branch:** `main` (Renovate reads its config from the default branch only; this is the one exception to the "oldest release branch first" rule for this milestone)
- **Type:** CI fix (dependency policy)
- **Decision owner:** ranger confirmed on 2026-10-02 that `release/1.x` and `release/2.x` still promise transitive security fixes

This is a spec, not a plan.

## Problem

`.github/renovate.json5` has a rule "Maintenance release lines get security fixes only" that sets `enabled: false` for `matchBaseBranches: ["release/1.x", "release/2.x"]`.
It matches every update type, `lockFileMaintenance` included, so those branches never get a lockfile regen.
Its comment says they still get security fixes through `vulnerabilityAlerts`.
The `lockFileMaintenance` comment in the same file says `vulnerabilityAlerts` cannot fix a purely transitive dependency, and that most of this repo's alerts are that kind (postcss, svgo, brace-expansion through Docusaurus; quinn-proto through quinn).
So transitive CVEs on the maintenance lines are never raised, and nothing reports it.

## Verified facts (`main`)

| Fact | Location |
| --- | --- |
| `lockFileMaintenance.enabled: true` with a daily `schedule` (`before 6am`, since #2364) | `.github/renovate.json5` |
| The disable rule for 1.x and 2.x has no `matchUpdateTypes`, so it covers everything | `.github/renovate.json5` |
| A later rule automerges `minor`, `patch`, `pin`, `digest`, and `lockFileMaintenance` once the branch is green | `.github/renovate.json5` |
| Renovate applies `packageRules` in order; a later matching rule overrides an earlier one for the keys it sets | Renovate docs |
| Renovate reads configuration from the default branch only; branch-specific behavior comes from `matchBaseBranches` | Renovate docs, and the #2364 memory note |
| `release/1.x` and `release/2.x` exist on origin | `git ls-remote --heads origin 'release/*'` |

## Decisions

1. **Re-enable lock maintenance on the two lines with a later rule.**
   `{ description: "Maintenance lines still get transitive security fixes through lock maintenance", matchBaseBranches: ["release/1.x", "release/2.x"], matchUpdateTypes: ["lockFileMaintenance"], enabled: true }`, placed after the disable rule.
   Cost: at most one daily lock PR per line, automerged by the existing rule when CI is green.
2. **Fix both comments.**
   The disable rule's description says "version bumps are off; lock maintenance stays on for transitive fixes".
   The `lockFileMaintenance` comment names the two lines as covered.
3. **No new groups or schedules.** #2364 already slowed lock maintenance to daily; more granularity was rejected there.
4. **Verify from the Renovate side, not by guessing.**
   After merge, the dependency dashboard issue lists a lock-maintenance entry for each line within a day.
   If the lines' CI is red for reasons unrelated to the lock, that is a separate issue to file, not a reason to revert.

## Design

Edit `.github/renovate.json5` only:

1. Update the description of the existing disable rule.
2. Add the new rule directly after it.
3. Update the `lockFileMaintenance` comment.

Run the Renovate config validator before opening the PR (`npx --yes --package renovate -- renovate-config-validator .github/renovate.json5`, or the repo's existing way if one exists).

## Tests

1. The config validator passes.
2. Manual: after merge, the dependency dashboard shows `Lock file maintenance` entries for `release/1.x` and `release/2.x`.
3. Manual: the first lock PR on each line is automerged when green; if either line's CI is red, file an issue naming the failure.

## Acceptance criteria

1. A lock-maintenance PR appears on each of `release/1.x` and `release/2.x` within a day of merge.
2. The two comments state the real behavior.
3. No changelog entry (repository policy, not a user-facing change). If a lock PR on an old line pulls in a CVE fix, that line's own release notes cover it.

## Out of scope

- Re-enabling minor, patch, or major bumps on the maintenance lines.
- Changing the `vulnerabilityAlerts` handling.
