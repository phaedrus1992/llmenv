# Issue #2165 — scan release branches for new advisories

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2165
- **Milestone:** `v3.11.2`
- **Branch:** the workflow goes on `main` (GitHub runs scheduled workflows from the default branch only); the Renovate change goes in `main`'s `.github/renovate.json5`
- **Type:** CI

This is a spec, not a plan.

## Problem

No check looks for new advisories on `release/*` branches:

1. Dependabot alerts are computed for the default branch (`main`) only.
2. Renovate's `vulnerabilityAlerts` reads those same Dependabot alerts, so it has no data for release branches either.
3. `cargo deny` (the `deny` check) runs only on pull requests, and it is skipped when a PR changes no Rust code.

Result, found 2026-09-26: `release/3.x` still shipped `serialize-javascript` (high) and other findings that `release/4.x` had fixed a month earlier, and nothing reported it (#2164).

Renovate could not fix those findings even with the data: they are transitive npm packages pinned by their parents, which need `overrides`, and Renovate does not write `overrides`.
So the gap to close is detection, not remediation.

## Verified facts

| Fact | Location |
| --- | --- |
| Renovate config is read from the default branch: `.github/renovate.json5` on `main` | repo |
| `baseBranchPatterns: ["main", "/^release\\/.+$/"]`; `vulnerabilityAlerts.enabled: true`; no `osvVulnerabilityAlerts` | `main:.github/renovate.json5` |
| Existing action pins: `EmbarkStudios/cargo-deny-action@3c6349835b2b7b196a839186cb8b78e02f7b5f25` (v2.1.1), `actions/setup-node@820762786026740c76f36085b0efc47a31fe502…` | `.github/workflows/ci.yml` line 105, `.github/workflows/docs.yml` line 26 |
| Maintained release branches today: `release/1.x`, `release/2.x`, `release/3.x`, `release/4.x` | `git ls-remote --heads origin 'release/*'` |
| Website lockfile: `website/package-lock.json`; Node `>=24.21.0` | `website/package.json` |

## Design

### 1. Scheduled workflow: `.github/workflows/release-advisories.yml` on `main`

Triggers: `schedule` daily at 06:17 UTC (off the hour), and `workflow_dispatch`.
Permissions: `contents: read`, `issues: write`. Nothing else.

Jobs:

1. **`branches`**: list `release/*` heads with `git ls-remote --heads origin 'release/*'`, keep names matching `^release/[0-9]+\.x$` (the shape check), and output them as a JSON array for the matrix.
2. **`scan`** (matrix over the list, `fail-fast: false`), for each branch:
   1. `actions/checkout` at that branch, `persist-credentials: false`.
   2. `cargo deny check advisories` with `EmbarkStudios/cargo-deny-action`, pinned to the same SHA as `ci.yml`, `command: check advisories`, `continue-on-error: true`; record the outcome.
   3. If `website/package-lock.json` exists: `actions/setup-node` (same pin as `docs.yml`, Node 24), then `npm audit --package-lock-only --audit-level=moderate --json` in `website/`, saved to a file. No `npm install`, no scripts.
   4. Run `scripts/release-advisory-report.sh <branch> <deny-outcome> <npm-audit.json>`, which prints a markdown report on stdout and exits 0 when clean, 1 when there are findings. Put the parsing in `scripts/release_advisory_report.py` beside it (Python standard library only), per the repo's script rule.
   5. The repo has no `security` label yet. Before the first `gh issue` call, run `gh label create security --color B60205 --description "Security advisory on a release branch" --force` (idempotent).
   6. On findings: find an open issue titled exactly `security: advisories on <branch>` with label `security`; update its body with the report, or create it with labels `security`, `dependencies`, `P1`. Use `gh issue list/create/edit` with `GH_TOKEN: ${{ github.token }}`.
   7. When clean: close an open issue with that title, with the comment `No advisories on <branch> as of <date>.`

Rules:

- Pin every action to a full SHA with a version comment (project rule). Run `actionlint` and `zizmor` on the file; fix all findings.
- Never interpolate `${{ matrix.branch }}` into a `run:` script directly; pass it through `env:` (zizmor template-injection rule).
- One issue per branch, not per advisory, so a noisy day does not flood the tracker.

### 2. Renovate: add OSV data

In `main:.github/renovate.json5`, add `"osvVulnerabilityAlerts": true` next to `vulnerabilityAlerts`, with a comment: `Dependabot data covers only the default branch; OSV lets Renovate raise security PRs for direct dependencies on release/* too.`
This helps only for direct dependencies; the scheduled scan covers the rest.

## Tests

1. `release_advisory_report.py` unit tests in `scripts/tests/test_release_advisory_report.py`, run with `python3 -m unittest` (standard library; the repo has no Python test runner dependency): clean audit JSON gives exit 0 and no report; audit JSON with a high finding lists package, severity and range; a failed deny outcome adds a Rust section; a malformed JSON file gives an error naming the file.
2. `actionlint` and `zizmor` pass on the workflow.
3. A manual `workflow_dispatch` run.

## Acceptance criteria

1. A manual run on today's branches opens `security: advisories on release/3.x` listing the #2164 findings (if #2164 is not merged yet), and opens nothing for a clean branch.
2. After #2164 merges, the next run closes that issue.
3. No changelog entry (CI only).

## Out of scope

- Fixing findings automatically.
- Scanning `main` (Dependabot already does).
