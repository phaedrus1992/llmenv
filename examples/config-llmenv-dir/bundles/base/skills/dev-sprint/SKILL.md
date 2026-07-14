<!-- markdownlint-disable MD003 MD013 MD022 MD041 -->
---
name: dev-sprint
description: Use when user asks "start sprint", "pick up work", "what should I work on". Selects 1-5 related issues from active milestone, synthesizes into composite work item (or picks single issue), hands off to ship-issue.
model: haiku
resources:

- scripts/list-issues.sh
- scripts/create-composite.sh
- scripts/resolve-base-branch.sh
- references/plugin-augmentation.md

---

# Dev Sprint

Pick 1-5 related issues, prioritizing lowest active milestone. Run a single issue by choice, or group related issues: same subsystem, shared code paths, blocking order.
Prioritize bug fixes over new features.
Pick one issue deliberately, or as many as five. Either way, skip composite creation only if it's a single issue (Phase 3) — but **still run the full Phase 4 handoff**, not a stripped-down one. "Directly" means "no composite," NOT "skip the augmentation."
Synthesize into composite.
Hand off to `ship-issue`. `ship-issue` owns branching, TDD, CI, pre-pr-review, merging. `dev-sprint` owns selection + synthesis.

**Single-issue runs are not a fast path.** Whether the sprint is one issue or five, Phase 4's handoff augmentation is mandatory: the required `pre-pr-review` scans (slop-scan, etc.), the `ponytail:ponytail-review` pass after `pre-pr-review`, the propagated directives (subagent/executing-plans, review autonomy), and Phases 5–8 (docs, semver, changelog, deferred-work filing, verification). The only thing a single issue skips is building a composite issue (Phase 3) and the multi-issue closing refs. Everything else runs exactly as it does for a multi-issue sprint.

**Hard rule — no orphan sprints:** Once a sprint composite is created (Phase 3), it owns the run. If scope must shrink, update the composite or close it with justification (see Phase 3.5). Never create a second composite while the first is open.

## Setup

**Create `.tmp/` at project root before starting:**

```bash
mkdir -p .tmp
```

All temp files in dev-sprint + sub-skills (`ship-issue`, `pre-pr-review`, etc.) use `.tmp/` at project root, not `/tmp`. Keeps artifacts local, no system temp interference.

## Phase 1: Triage Unmilestoned Issues

**Before selecting work, every open issue must have a milestone.** List the orphans directly:

```bash
"${CLAUDE_CONFIG_DIR}/skills/dev-sprint/scripts/list-issues.sh" --no-milestone
```

(Add `--json` for machine-readable output to pipe through `jq`/`python` — full bodies included, so no follow-up `gh issue view` needed.)

Assign each unmilestoned issue to the right milestone:

1. **Survey existing milestones first.** List with due dates + themes to place each orphan accurately:

   ```bash
   gh api repos/:owner/:repo/milestones --jq '.[] | "\(.title) — \(.description // "no description") (due \(.due_on // "none"))"'
   ```

2. **Match each orphan to best-fitting milestone** by topic/subsystem + ordering (which release it belongs to). Bugs in shipped behavior → earliest open milestone; new features → milestone owning that area.

   ```bash
   gh issue edit <issue> --milestone "<milestone-title>"
   ```

3. **No existing milestone fits? OK to create one** — or rename existing milestones for better ordering or topic coverage. Prefer reuse/rename over proliferating near-duplicate milestones.

   ```bash
   # create
   gh api repos/:owner/:repo/milestones -f title="vX.Y" -f description="<theme>" -f due_on="<ISO8601 or omit>"
   # rename / re-describe (milestones are addressed by number, not title)
   gh api -X PATCH repos/:owner/:repo/milestones/<number> -f title="<new title>" -f description="<new theme>"
   ```

**Goal:** after this phase, `No Milestone` is empty (or holds only issues you've deliberately, with justification, decided are out of scope for any milestone). Re-run `list-issues.sh` to confirm before selection.

## Phase 2: Select Related Issues

```bash
"${CLAUDE_CONFIG_DIR}/skills/dev-sprint/scripts/list-issues.sh"
```

Pick lowest milestone with open issues. Choose 1-5 related issues from that milestone: one issue alone (deliberately), or multiple related issues (same subsystem, shared code paths, blocking order).

Prefer: shared files, unblock each other. Avoid: mixed domains.

## Phase 3: Create Composite Issue

Composite body structure:

```markdown
## Issues
- #N — <title>
- #N — <title>

## Scope
<Shared subsystem/data flow/behavior. Why together.>

## Acceptance Criteria
<Merged & deduplicated criteria from all issues>

## Implementation Notes
<Ordering constraints. Known shared files.>
```

**CRITICAL:** List all sub-issues in composite body. Source of truth for what PR must close.

```bash
"${CLAUDE_CONFIG_DIR}/skills/dev-sprint/scripts/create-composite.sh" "Sprint: <theme>" "<body>" "<milestone>" "<labels>"
```

Milestone required; **labels optional** (upstream `ship-issue`/`pr-review` no longer enforce labels, label sets vary per repo). Labels = 4th arg, **comma-separated** for multiple — e.g. `"bug,area-core"`. Use labels that exist in repo (`gh label list`); script validates each, attaches the existing ones, skips missing ones with a warning rather than failing. Script prints new issue number on its final line—capture it, pass to `ship-issue`.

### Phase 3.5: Composite is Immutable (READ BEFORE PROCEEDING)

**Once the composite issue is created, it IS the sprint. You may not silently abandon it and create a new one.**

This is the orphan-sprint failure mode: composite #A is created with sub-issues #X, #Y, #Z; mid-run you decide #Y and #Z are too much; you create composite #B covering only #X and ship that; #A is left OPEN with #Y and #Z dangling, milestone view corrupted, planned work silently dropped.

**Forbidden:** creating a second composite during the same dev-sprint run while the first is still OPEN. Tempted to? Stop, apply one resolution below instead.

**If scope must shrink after Phase 3 (e.g. one bug harder than expected, another turns out blocked):**

1. **Preferred — keep the composite, shrink the PR:** Pick the subset you'll actually ship. Update composite body to mark deferred sub-issues:

```markdown
   ## Issues
   - #X — <title>  ✅ (this PR)
   - #Y — <title>  ⏸ deferred → #<followup>
   - #Z — <title>  ⏸ deferred → #<followup>
   ```

   For each deferred sub-issue, post a comment explaining the defer + linking the follow-up plan (or leave it open with no comment if follow-up is just "next sprint"). Composite stays open until its PR merges; on merge, `Closes #<composite>` + `Closes #X` only. #Y and #Z stay open for a future sprint.

   ```bash
   gh issue edit <composite> --body "$(updated body)"
   gh issue comment <Y> --body "Deferred from sprint #<composite>. Will be picked up in a follow-up sprint."
   ```

1. **Alternative — abandon the composite cleanly:** Original framing wrong and you want a fresh composite? You must close the old one first with a justification comment:

   ```bash
   gh issue comment <composite> --body "Closing as superseded. Original scope (#X #Y #Z) was too broad for one sprint. Replacing with #<new-composite> covering #X only. #Y and #Z return to milestone backlog."
   gh issue close <composite>
   ```

   Only after the old composite is CLOSED may you create a new one.

**Self-check before running `create-composite.sh` a second time in one run:** Has a composite already been created in this run? If yes, you are in the orphan path. STOP and apply resolution 1 or 2 above. Never create a second composite without closing or updating the first.

## Phase 4: Hand Off to ship-issue

**Applies to single-issue runs too.** Where this phase says "composite-id," use the single issue's number instead. The base-branch resolution, scans, ponytail pass, propagated directives, and PR verification all run regardless of issue count.

### Phase 4a: Select the base branch (milestone → release branch)

**The composite's milestone decides the base branch — not `main` by default.** A milestone titled `X.Y — <theme>` (or `vX.Y`) is a patch line: if a `release/X.Y.x` branch exists on the remote, branch from it so the work doesn't drag in unreleased feature work that lives only on `main` (and doesn't duplicate commits already on the release branch). Only fall back to the default branch when **no** matching `release/X.Y.x` exists.

**Use the script — don't eyeball it.** `resolve-base-branch.sh` performs this evaluation deterministically (same result every run) and prints the base branch on stdout:

```bash
# By composite issue number (looks up its milestone via gh):
BASE_BRANCH="$("${CLAUDE_CONFIG_DIR}/skills/dev-sprint/scripts/resolve-base-branch.sh" --issue <composite-id>)"
# …or by milestone title directly:
BASE_BRANCH="$("${CLAUDE_CONFIG_DIR}/skills/dev-sprint/scripts/resolve-base-branch.sh" --milestone "<milestone>")"
echo "Base branch for this sprint: $BASE_BRANCH"
```

The script falls back to the repo default branch when the milestone has no matching `release/X.Y.x` (a milestone with no release line is not an error). Diagnostics go to stderr; only the branch name is on stdout, safe for `$(...)` capture.

Pull latest of `$BASE_BRANCH` before invoking. **Pass `$BASE_BRANCH` into the `ship-issue` handoff** (see example below) so it branches from the right place instead of assuming `main`. Check `references/plugin-augmentation.md` for plugin, include in message.

**IMPORTANT: All temp files in dev-sprint + sub-skills use `.tmp/` at project root. Ensure `.tmp/` exists before invoking sub-skills. Sub-skills must not use `/tmp`.**

**CRITICAL: Include sub-issue list in `ship-issue` invocation.** Propagates to `pre-pr-review` so PR has all closing refs.

Example message:

```text
Sprint: Validation & Error Handling

Base branch: release/1.0.x   ← branch from this, NOT main (from Phase 4a)

Sub-issues to close on PR merge:
- #315 — Code quality: socket validation
- #325 — P0: Add error handling and validation
- #330 — Sprint composite issue

[Continue with ship-issue instructions...]
```

**For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

Applies to any sub-skill that launches agents to do the work (`ship-issue`, `pre-pr-review`). Include this directive in the handoff message so it propagates.

**Review phase autonomy:** `ship-issue` → `pre-pr-review` fixes apply automatically, no user prompts. Fix or defer, then create PR.

**Required scans during `pre-pr-review`:** Include in `ship-issue` handoff so it propagates to `pre-pr-review`:

- `/Users/phaedrus/git/my-llmenv/bundles/base/scripts/slop-scan.sh scan . --lint`

  Always invoke slop-scan through this pinned wrapper (it runs `slop-scan@0.3.0`
  and is on the permission allowlist). Do **not** call `npx slop-scan` directly —
  the unpinned, agent-chosen form is blocked by the supply-chain auto-policy.

**After `pre-pr-review` completes:** Run `ponytail:ponytail-review` and apply all suggestions automatically before creating the PR. No user prompts — apply and commit.

**PR closing references (MANDATORY):**

```text
Closes #<composite-id>
Closes #<issue-1>
Closes #<issue-2>
Closes #<issue-3>
```

Composite first, then sub-issues. GitHub auto-closes on merge. Every sprint PR closes all sub-issues, not just composite.

**Sprint meta-issue:** Sprint tracked via issue? Add closing ref:

```text
Closes #<composite-id>
Closes #<issue-1>
...
Closes #<sprint-tracking-issue>
```

Branch (created by `ship-issue` from `$BASE_BRANCH` selected in Phase 4a):

```text
feat/<composite-id>-<theme-slug>   # forked from $BASE_BRANCH (release/X.Y.x or default)
```

Follow `ship-issue` exactly—no skips.

## Phase 5: Documentation & Semver

After `ship-issue`, update docs + version.

**Docs:** Update README.md, docs/ for:

- Features/APIs → examples + usage
- Behavior changes → update sections
- Types/functions → API docs
- Config changes → options/env vars

**Semver:**

- **MAJOR**: Breaking API/behavior/config
- **MINOR**: New features, APIs, config options (backward-compatible)
- **PATCH**: Bug fixes, internal improvements

Update `package.json` / `Cargo.toml` / `pyproject.toml` + any version refs. Commit with code.

**CHANGELOG (always run the `keepachangelog` skill at the end of the sprint):** Invoke the `keepachangelog` skill to update `CHANGELOG.md` if the sprint introduced any user-visible changes (features, behavior changes, config changes, bug fixes). If `keepachangelog` modifies `CHANGELOG.md`, commit it. If nothing warrants a changelog entry, leave it unchanged. This runs on every sprint — never skip it.

## Phase 6: Deferred Work & Pre-Existing Issues

**Defer only if:**

- Pre-existing bugs outside sprint scope
- Features needing brainstorm/design
- Work needing `feature-dev` or architecture review
- External dependency blocks

**Do NOT defer:** Fix non-complex issues immediately, mid-sprint OK.

```bash
gh issue create \
  --title "Title" \
  --body "Found during sprint X, [details]" \
  --milestone "vX.X" \
  --label "bug"   # optional; use a label that exists in this repo (gh label list)
```

**REQUIRED:** All issues from any skill need:

- `--milestone` (no exceptions)
- `--label` is **optional** — if used, must be a label that exists in repo (`gh label list`). Don't invent labels.

## Phase 6a: Issue Creation Standards (All Skills)

**MANDATORY for all issues:**

1. **Milestone**: Active milestone or current sprint (required)
2. **Body**: Where found, why deferred, repro steps (if bug), blockers
3. **Label**: optional — if used, must already exist in the repo (`gh label list`)

**Enforce in skill scripts:** require the milestone, but treat the label as optional and validate it against the repo (warn, don't reject) rather than against a fixed allowlist:

```bash
if [[ -z "$MILESTONE" ]]; then
  echo "Error: Milestone is required for all issues" >&2
  exit 1
fi

declare -a GH_ARGS=(issue create --title "$TITLE" --body "$BODY" --milestone "$MILESTONE")
if [[ -n "${LABEL:-}" ]]; then
  if gh label list --limit 200 --json name --jq '.[].name' | grep -Fxq "$LABEL"; then
    GH_ARGS+=(--label "$LABEL")
  else
    echo "Warning: label '$LABEL' not found in repo; creating without it." >&2
  fi
fi
gh "${GH_ARGS[@]}"
```

## Phase 7: Verify PR (base branch + closing references)

After the PR is created, verify two invariants **before** allowing the merge.

### 7a: Base branch matches the milestone

The PR must target the branch `resolve-base-branch.sh` selected in Phase 4a — not whatever the agent defaulted to. Recompute the expected base and compare it to the PR's actual base:

```bash
EXPECTED_BASE="$("{${CLAUDE_CONFIG_DIR}/skills/dev-sprint/scripts/resolve-base-branch.sh" --issue <composite-id>)"
ACTUAL_BASE="$(gh pr view <pr-number> --json baseRefName --jq .baseRefName)"
if [[ "$EXPECTED_BASE" != "$ACTUAL_BASE" ]]; then
  echo "BASE MISMATCH: PR targets '$ACTUAL_BASE' but milestone wants '$EXPECTED_BASE'" >&2
  # Fix in place — retarget the PR (no reopen needed):
  gh pr edit <pr-number> --base "$EXPECTED_BASE"
  # Re-verify, and confirm the diff still makes sense against the new base
  # (a wrong base can hide or invent changes). If the branch was forked from
  # the wrong base, rebase onto $EXPECTED_BASE before retargeting.
fi
```

**Failure condition:** PR merges into the wrong base (e.g. `main` for a `1.0` milestone). This is the exact misfire Phase 4a exists to prevent — 7a is the catch-net if the branch was still created from the wrong place.

### 7b: Closing references

Verify a closing ref for every sub-issue:

```bash
gh pr view <pr-number> --json body --jq .body | grep -c "^Closes #"
```

Count must equal sub-issue count. If not, fix immediately:

```bash
gh pr edit <pr-number> --body "$(gh pr view <pr-number> --json body --jq .body)
Closes #<missing-issue-id>"
```

**Failure condition:** PR merges without closing all sub-issues. Sprint not complete until all closed.

## Phase 8: Task Cleanup

`TaskList`. Mark stale tasks `deleted` via `TaskUpdate`. Keep composite task if planning new sprint.

## References

Plugin/agent guidance: `references/plugin-augmentation.md`
