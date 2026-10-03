# Issue #1539 — required code-scanning check can go stale before merge

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/1539
- **Milestone:** `v3.12.0`
- **Base branches:** `release/3.x` for `codeql.yml` (forward-merges); `release/4.x` for `zizmor.yml`, which does not exist on `release/3.x`. Two PRs, the second after the first forward-merges.
- **Type:** CI fix, with an experiment first

This is a spec, not a plan.

## Problem

Two PRs showed all checks green but merge was blocked with "Code scanning is waiting for results from zizmor for the commits `<sha>`".
The workflow triggers on `pull_request` and checks out GitHub's ephemeral test-merge ref (`refs/pull/N/merge`).
GitHub recomputes that merge commit's SHA over time (observed drifting between two API calls minutes apart with no push to either branch).
The SARIF upload is tied to `github.sha` at run time (the old merge SHA), while the `code_scanning` ruleset rule checks the PR's current `merge_commit_sha`.
Once those diverge, the required check waits forever for a result that will never arrive.
Pushing an empty commit retriggers the scan and unblocks the PR.

## Verified facts

| Fact | Location |
| --- | --- |
| `zizmor.yml` exists on `main` and `release/4.x` (triggers: `push` to `main` and `release/**`, and `pull_request`; `actions/checkout` with `persist-credentials: false`; `zizmorcore/zizmor-action` uploads SARIF). It is absent on `release/3.x` | `.github/workflows/zizmor.yml` on `main` |
| `codeql.yml` on `release/3.x` triggers on `push`, `pull_request`, and a weekly schedule; a `changes` job gates four languages; `codeql-action/analyze` uploads per language; an unchanged language uploads an empty SARIF so the required rule still sees a result (#2411) | `.github/workflows/codeql.yml` |
| Both upload with the ambient `github.sha` and `github.ref`, which on `pull_request` are the merge ref and its commit | workflow defaults |
| The repo ruleset covering `main` and `release/**` requires `test`, `deny`, `hawk`, CodeQL, and zizmor, with an admin bypass actor | comment in `.github/workflows/forward-merge-release.yml` |
| `github/codeql-action/upload-sarif` and `analyze` accept `ref` and `sha` inputs; GitHub's code-scanning upload API requires `ref` to be `refs/pull/N/merge` or `refs/pull/N/head` for a PR, with `sha` the commit that ref pointed at when the analysis ran | codeql-action README, GitHub REST docs for code-scanning SARIF upload |
| The confirmed mismatch: the uploaded SARIF's `commit_sha` matched neither the PR head SHA nor the current `merge_commit_sha` | issue text |

## Decisions

1. **Run an experiment before changing the required workflows.**
   On a throwaway branch, add a copy of the zizmor job (on `release/4.x`) that checks out `github.event.pull_request.head.sha` and uploads with `ref: refs/pull/${{ github.event.pull_request.number }}/head` and `sha: ${{ github.event.pull_request.head.sha }}`.
   Open a PR, let both the original and the copy upload, then force the merge ref to recompute (push a commit to the base branch, or wait and poll `gh api repos/…/pulls/N --jq .merge_commit_sha`).
   Record whether the copy's check stays satisfied while the original goes stale.
2. **If the head-ref upload holds, apply it to both workflows.**
   `codeql.yml` on `release/3.x`: the `analyze` and `upload-sarif` steps get `ref` and `sha` inputs on `pull_request` events, and `actions/checkout` gets `ref: ${{ github.event.pull_request.head.sha }}` so the analyzed tree matches the uploaded SHA.
   `zizmor.yml` on `release/4.x`: the same, through `zizmor-action`'s inputs if it exposes them; if it does not, run `zizmor --format sarif` directly and upload with `codeql-action/upload-sarif` (pinned to the same SHA the repo already uses).
   `push` events are unchanged.
3. **Scanning the head instead of the merge result is an accepted trade.**
   The merge-ref scan catches a conflict between the PR and the base that neither side has alone.
   For zizmor (workflow files) and CodeQL (code) that case is rare, and the `push` scan on the base after merge still catches it.
   Say this in the workflow comment.
4. **If the experiment fails, document the retrigger.**
   `website/docs/maintainers.md` gets a "code scanning is waiting for results" entry with the empty-commit command, and the issue closes with that.
5. **Keep the #2411 empty-SARIF step** and give it the same `ref`/`sha`, or the unchanged-language result will be the stale one.

## Design

### Experiment (throwaway, on `release/4.x`)

A second job in `zizmor.yml` named `zizmor-head-experiment`, present for one PR only, removed before the real change merges.

### `codeql.yml` (release/3.x)

```yaml
- uses: actions/checkout@<pinned>
  with:
    persist-credentials: false
    ref: ${{ github.event.pull_request.head.sha || github.sha }}
…
- uses: github/codeql-action/analyze@<pinned>
  with:
    category: /language:${{ matrix.language }}
    ref: ${{ github.event.pull_request.number && format('refs/pull/{0}/head', github.event.pull_request.number) || github.ref }}
    sha: ${{ github.event.pull_request.head.sha || github.sha }}
```

The same two inputs on the empty-results upload step.
A comment above the first one explains the merge-ref drift and links this issue.

### `zizmor.yml` (release/4.x, after forward-merge)

Same pattern; the exact step shape depends on `zizmor-action`'s inputs at the pinned version (read its action.yml).

### Docs

- `website/docs/maintainers.md`: the stale-check symptom, why it happened, and the retrigger command as a fallback. No changelog entry (CI only).

## Tests

1. The experiment PR shows the head-ref check staying green after the merge ref changes, with the API output pasted into the issue.
2. `actionlint` and `zizmor` pass on both changed workflows.
3. A PR after the change: `gh api repos/…/code-scanning/analyses?tool_name=CodeQL` shows `commit_sha` equal to the PR head SHA and `ref` equal to `refs/pull/N/head`.
4. A PR that touches no scanned language still satisfies the required check (the #2411 path).

## Acceptance criteria

1. The experiment result is recorded in the issue.
2. Both workflows upload against the PR head on `pull_request`, or the docs record the fallback and why.
3. No PR in the following week needs an empty commit to unblock code scanning (check before closing).

## Out of scope

- Changing the ruleset's required checks.
- The forward-merge workflow's own approval polling.
