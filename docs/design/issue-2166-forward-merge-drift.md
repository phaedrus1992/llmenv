# Issue #2166 — forward-merge fails on Renovate pin and lockfile drift

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2166
- **Milestone:** `v3.11.2`
- **Branch:** `release/3.x` (the workflow file runs from the branch that was pushed, so the fix must be on the source branch; it forward-merges up)
- **Type:** CI bug, P1

This is a spec, not a plan.

## Problem

Since 2026-09-25, every push to `release/3.x` fails the **Forward-merge release branches** workflow at the `release/3.x` → `release/4.x` step.
No 3.x fix reaches 4.x or `main` until this is fixed, including #2164's security fix.

Reproduced locally (merge `origin/release/3.x` into `origin/release/4.x`, 2026-09-26): conflicts in `Cargo.toml`, `Cargo.lock` and `website/package-lock.json`.
The `Cargo.toml` conflict is only:

- llmenv's own path dependencies: `version = "3.11.1"` against `"4.0.0-alpha.1"` (expected; the workflow already handles this), and
- third-party pins that Renovate bumped separately on both branches: `clap = "=4.6.6"` → `"=4.6.7"` and `clap_complete = "=4.6.9"` → `"=4.6.11"` on 3.x, while 4.x already has `=4.6.7` and `=4.6.11`.

Renovate runs on every `release/*` branch (`baseBranchPatterns`), so the same bump lands twice, and weekly lock-file maintenance rewrites both lockfiles.
This will recur every week.

## Verified facts (`.github/workflows/forward-merge-release.yml` on release/3.x)

| Fact | Location |
| --- | --- |
| `auto_resolve_conflicts` handles `website/docs/changelog.md` (regenerate) and `Cargo.toml`, `Cargo.lock`, `crates/*/Cargo.toml` (keep target) only when `version_only_change` passes; any other conflicting file bails | near line 235 |
| `version_only_change` blanks every `version = "…"` in the merge-base and source copies and compares; it treats `clap = "=4.6.7"` as a real change because that form has no `version =` key | near line 209 |
| On success it pushes to the target; if branch rules reject the push, it pushes `forward-merge/<source>-to-<target>` and opens a PR | near lines 335 to 410 |
| The job has only `actions/checkout`; no Rust or Node toolchain step | line 100 |
| Pins available in the repo: `dtolnay/rust-toolchain@02cb101ec7c40f2c49e1d9714d64511d8e1b74de` (`ci.yml` line 78), `actions/setup-node@820762786026740c76f36085b0efc47a31fe502…` (`docs.yml` line 26) | |
| A guard test script exists: `.github/workflows/__tests__/forward-merge-release-guards.sh` | |

## Decisions

1. **Unblock by hand now.** Resolve the current 3.x → 4.x merge manually, as the workflow's error message instructs. This is the first task, before the code change.
2. **Target wins for a dependency the target already has at the same or a newer version.** A forward-merge must not downgrade the target. If the source moved a dependency to a version the target already has or passes, keeping the target's line loses nothing.
3. **Regenerate lockfiles; never merge them by hand.** After the manifests are resolved, start from the target's lockfile and let the tool reconcile it with the merged manifests.
4. **Everything else still bails.** A new dependency, a removed dependency, a feature change, or a source version newer than the target's is a real change and needs a human.

## Design

### 1. Manual resolution now

On a branch `forward-merge/release/3.x-to-release/4.x` from `origin/release/4.x`, run `git merge origin/release/3.x` and resolve:

- `Cargo.toml`: keep `release/4.x`'s side (`4.0.0-alpha.1` path versions, `clap =4.6.7`, `clap_complete =4.6.11`); check that every other 3.x change in the file is already present on 4.x.
- `Cargo.lock`: `git checkout --ours Cargo.lock`, then `cargo update --workspace`.
- `website/package-lock.json`: `git checkout --ours website/package-lock.json`, then `npm install --package-lock-only --ignore-scripts` in `website/`.
- Build, test, open the PR against `release/4.x`, merge when checks pass. The 4.x → `main` step then runs from 4.x's own workflow.

### 2. Manifest rule: `scripts/forward_merge_manifest.py`

A Python 3 standard-library script (`tomllib`), called from the workflow:

```
python3 scripts/forward_merge_manifest.py <base.toml> <source.toml> <target.toml>
```

Exit 0 means "keeping the target's file loses nothing"; exit 1 means a real change; exit 2 means it could not decide (parse error), which the workflow treats as exit 1.
Print one line per blocking difference on stderr, naming the table and key.

Rules:

1. Compare these tables in all three files: `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, `[workspace.dependencies]`, and every `[target.<cfg>.dependencies]` family.
2. Ignore `[package].version` and `[workspace.package].version`, and the `version` field of dependencies that have a `path` key (llmenv's own crates). These are the version-only differences the workflow already accepts.
3. For each dependency where the source differs from the base:
   - It must exist in the target.
   - Every field except `version` must be equal between source and target (a string dependency `"=1.2.3"` counts as `{version = "=1.2.3"}`).
   - The version must parse on both sides as an optional operator (`=`, `^`, `~`, `>=`) followed by `MAJOR.MINOR.PATCH` with ASCII digits (shape check with a regex, then parse the numbers).
   - The target's version must be greater than or equal to the source's.
4. A dependency removed or added by the source since the base: exit 1.
5. Any other key that differs between base and source outside the tables in rule 1 and the ignores in rule 2: exit 1.

In `auto_resolve_conflicts`, for `Cargo.toml` and `crates/*/Cargo.toml`: accept the target's copy only when the script exits 0.
The workflow no longer uses `version_only_change`.
That check blanks every `version = "…"`, so it called a source bump to a newer third-party pin "version-only", and keeping the target's copy then dropped the bump.
The script already ignores the version differences the old check was for (package versions and path dependencies).

Two details the first draft did not cover:

- A `{ workspace = true }` dependency is resolved against `[workspace.dependencies]` before the comparison, because a branch can hoist a pin there (4.x did this for `rustix`).
- The operator (`=`, `^`, `~`, `>=`) must be equal on both sides, and only the numbers are ordered.

The workflow copies the script from the pushed branch at the start of the job, because a target branch may not carry it yet.
On release/4.x the workflow holds `FORWARD_MERGE_PAT`, so it runs the target's own copy of the script instead (#1532, #2171).
A target without the script has no rule for a manifest conflict, and one manual merge puts the script there.
Write the three versions to temp files with `git show <ref>:<path>`.

### 3. Lockfile rules

Resolve lockfiles after all other conflicts, in this order:

- `Cargo.lock`: `git checkout --ours -- Cargo.lock`, then `cargo update --workspace`, then `git add -- Cargo.lock`. Add a `dtolnay/rust-toolchain` step (same pin as `ci.yml`, stable) to the job, before the merge step.
- `website/package-lock.json`: only if `website/package.json` has no remaining conflict. `git checkout --ours -- website/package-lock.json`, run `npm install --package-lock-only --ignore-scripts` in `website/`, `git add`. Add an `actions/setup-node` step (same pin as `docs.yml`, Node 24).
- If a lockfile command fails, bail with an error naming the command.

A lockfile-only change on the source (for example a transitive security bump from lock-file maintenance) is not carried over by this rule; the target keeps its own lockfile.
The target branch's own lock-file maintenance and #2165's scan cover that.

### 4. Workflow hygiene

Run `actionlint` and `zizmor` on the changed workflow; fix all findings.
Keep every action pinned to a full SHA with a version comment.

## Tests

1. `scripts/tests/test_forward_merge_manifest.py` (`python3 -m unittest`): the exact `clap`/`clap_complete` case from 2026-09-26 exits 0; a source version newer than the target exits 1; an added dependency exits 1; a changed `features` list exits 1; a string versus table dependency with the same version exits 0; `workspace.package.version` differences are ignored; an unparseable version exits 1; invalid TOML exits 2.
2. Extend `.github/workflows/__tests__/forward-merge-release-guards.sh` with a fixture repo where the source and target bump the same pin, and assert the merge completes.
3. `actionlint` and `zizmor` pass.

## Acceptance criteria

1. The manual forward-merge PR lands, and `release/4.x` contains `release/3.x`.
2. After this change reaches `release/3.x`, the next push to `release/3.x` forward-merges to `release/4.x` and `main` with no human step when the only conflicts are pin drift and lockfiles.
3. No changelog entry (CI only).

## Out of scope

- Changing Renovate's schedule or scope per branch.
- Carrying lockfile-only changes forward.
