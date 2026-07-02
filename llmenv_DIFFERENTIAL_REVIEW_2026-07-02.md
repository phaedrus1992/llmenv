# Differential Security Review

**Branch:** `fix/541-prerelease-handling` → `main`
**Commit:** `953ea5f` — _fix: handle prerelease tags (rc/beta/alpha) in release pipeline_
**Date:** 2026-07-02
**Reviewer:** Automated differential review (nbl-dev/differential-review)

---

## Executive Summary

| Severity | Count |
|----------|-------|
| 🔴 CRITICAL | 0 |
| 🟠 HIGH | 0 |
| 🟡 MEDIUM | 2 |
| 🟢 LOW | 1 |

**Overall Risk:** MEDIUM
**Recommendation:** CONDITIONAL APPROVE

**Key Metrics:**
- Files analyzed: 2/2 (100%)
- Test coverage gaps: N/A (CI workflow + docs — no unit tests applicable)
- High blast radius changes: 0
- Security regressions detected: 0

---

## What Changed

**Commit Range:** `main..fix/541-prerelease-handling`
**Commits:** 1
**File:** `.github/workflows/release.yml` (+22 / -3), `RELEASING.md` (+42 / -0)

| File | +Lines | -Lines | Risk | Blast Radius |
|------|--------|--------|------|--------------|
| `.github/workflows/release.yml` | +22 | -3 | MEDIUM | LOW |
| `RELEASING.md` | +42 | -0 | LOW | N/A |

**Summary:** The PR adds prerelease-tag detection to the release pipeline. A regex match on the version string sets `IS_PRERELEASE`, which controls `--prerelease` / `--latest` flags on the `gh release` commands and skips the Homebrew formula update. `RELEASING.md` gains a new `## Pre-releases` section documenting the workflow.

---

## Findings

### 🟡 MEDIUM — Unquoted flag variables allow word-splitting with `set -u`

**File:** `.github/workflows/release.yml` lines 406 and 419
**Commit:** 953ea5f

**Description:**

```bash
gh release edit "${TAG}" --notes-file release-body.md ${LATEST_FLAG} ${PRERELEASE_FLAG}
# ...
              ${LATEST_FLAG} ${PRERELEASE_FLAG}
```

`LATEST_FLAG` and `PRERELEASE_FLAG` are initialised to `""` and then conditionally set to a flag string. Unquoted empty strings in a `set -euo pipefail` shell are fine under `bash` (empty words are dropped), but the pattern is non-idiomatic and fragile: if either variable is ever multi-word (e.g., `--flag value`), word splitting breaks argument passing silently. The conventional bash idiom for optional-flag accumulation is an array:

```bash
FLAGS=()
if [[ "${IS_PRERELEASE}" == "true" ]]; then
  FLAGS+=(--prerelease)
else
  FLAGS+=(--latest)
fi
gh release edit "${TAG}" --notes-file release-body.md "${FLAGS[@]}"
```

This is not a correctness bug today (single-word flags, controlled values), but the current pattern is a latent risk if the flag set grows.

**Risk rationale:** MEDIUM — not exploitable in current form, but a contributor adding a flag with a space (e.g., `--label "Prerelease RC"`) would introduce silent argument splitting.

---

### 🟡 MEDIUM — `publish-crate` job publishes prerelease versions to crates.io without documentation of intent

**File:** `.github/workflows/release.yml` — `publish-crate` job (no change in this diff)
**Related context:** `RELEASING.md` lines 235–237

**Description:**

The new `update-homebrew` job is correctly gated with `needs.create-release.outputs.is_prerelease == 'false'`. However, `publish-crate` has no corresponding guard — it runs unconditionally on any `refs/tags/v*` push, including prerelease tags. This is documented in `RELEASING.md` as intentional:

> _"crates.io — prerelease versions publish normally (semver.org support is native)."_

The finding is not a bug, but the intent is only in documentation. There is no in-workflow comment on `publish-crate` noting that it is expected to fire on prerelease tags. A future maintainer auditing the `update-homebrew` guard will notice the asymmetry without context. The `publish-crate` job also does not depend on `create-release`, so it cannot read `is_prerelease` output anyway — the asymmetry is structural.

**Recommendation:**

Add a comment at the top of the `publish-crate` `run:` block (or as a `name:` annotation):

```yaml
# Intentionally runs for prerelease tags — crates.io supports semver prerelease
# natively. See RELEASING.md §Pre-releases for the full policy.
```

This costs one line and eliminates the asymmetry question for future readers.

---

### 🟢 LOW — `is_prerelease` output is a string `"true"`/`"false"`, not a boolean; expression comparison is correct but fragile

**File:** `.github/workflows/release.yml` line 426

```yaml
if: startsWith(github.ref, 'refs/tags/v') && needs.create-release.outputs.is_prerelease == 'false'
```

GitHub Actions job outputs are always strings. The string `'false'` comparison is correct here. However, if the `extract_version` step were ever refactored to output a boolean or to omit the output on failure, the expression would silently evaluate to `'' == 'false'` → `false`, skipping Homebrew updates for all releases. This is documented behaviour in GitHub Actions but worth noting.

No action required; the current code is correct. Document the string-comparison expectation in a comment if the workflow is frequently edited.

---

## Test Coverage Analysis

**Coverage:** N/A — changes are CI workflow YAML and documentation. No unit tests are applicable. The logic under test is `gh release` invocation correctness, which is only verifiable by a live release run.

**Risk Assessment:** The prerelease regex (`^[0-9]+\.[0-9]+\.[0-9]+-`) correctly matches `3.0.0-rc.1`, `3.0.0-beta.1`, `3.0.0-alpha.1`, and does not match `3.0.0`. Edge cases: `3.0.0-0` (pre-release zero) — matches correctly. `3.0.0+build` (build metadata, no dash) — does not match (correct; build metadata is not a prerelease). `3.0.0-` (trailing dash with no label) — matches (unusual but harmless).

---

## Blast Radius Analysis

The changed code runs inside the `create-release` job, which is a leaf job with no downstream job depending on its `bash` internals. The `is_prerelease` output propagates only to `update-homebrew` via the `needs` context — blast radius is LOW.

---

## Historical Context

**Security-related removals:**

None. The only removal is the unconditional `--latest` flag being replaced by a conditional:

```yaml
# BEFORE
gh release edit "${TAG}" --notes-file release-body.md --latest
# ...
              --latest

# AFTER
gh release edit "${TAG}" --notes-file release-body.md ${LATEST_FLAG} ${PRERELEASE_FLAG}
```

`--latest` was added in `main` to pin the stable release as the GitHub "latest" release. The removal is intentional — it is now passed conditionally (`LATEST_FLAG="--latest"` for stable releases only).

**Regression check:** The original `--latest` flag is correctly preserved for stable releases (non-prerelease path sets `LATEST_FLAG="--latest"`). No regression.

---

## Recommendations

### Immediate (Blocking)

None. The implementation is functionally correct.

### Before Merge (Non-Blocking)

- [ ] Quote flag variables via an array pattern (`FLAGS=()`) or accept the current form with a comment explaining the single-word-only contract: `.github/workflows/release.yml` lines 397–419.
- [ ] Add one-line comment to `publish-crate` job explaining prerelease publish is intentional: `.github/workflows/release.yml` (publish-crate `run:` block).

### Technical Debt

- [ ] If the flag set grows, migrate to array-based flag accumulation to avoid word-splitting risk.

---

## Analysis Methodology

**Strategy:** DEEP (2 files, SMALL change)

**Analysis Scope:**
- Files reviewed: 2/2 (100%)
- HIGH RISK: N/A (no auth/crypto/value-transfer changes)
- MEDIUM RISK: 100% — CI workflow changes reviewed in full
- LOW RISK: Documentation section reviewed

**Techniques:**
- Full diff analysis of both changed files
- Git blame on removed lines (`--latest` flag)
- Regression check on stable-release path
- Bash idiom review (word splitting, `set -u` interaction)
- Job dependency graph tracing (`publish-crate` → no `create-release` dep)
- GitHub Actions expression type audit (`is_prerelease` string vs boolean)

**Limitations:**
- No live workflow execution to verify `gh release` flag behaviour end-to-end.
- `actionlint` static analysis not run (would catch the unquoted variable).

**Confidence:** HIGH for analyzed scope.
