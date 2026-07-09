# Releasing llmenv

Releases are **tag-triggered**. Pushing a `v*` tag to GitHub fires
[`.github/workflows/release.yml`](.github/workflows/release.yml), which does all
the publishing work:

- builds the cross-platform binaries (Linux x86_64, macOS x86_64, macOS arm64)
  with SHA-256 checksums and SLSA provenance,
- publishes the crate to [crates.io](https://crates.io/crates/llmenv)
  (`publish-crate` job),
- creates the GitHub Release with the binaries attached,
- bumps the Homebrew formula in `phaedrus1992/homebrew-tap`.

[`cargo-release`](https://github.com/crate-ci/cargo-release) owns only the local
prep: bumping the version and rolling `CHANGELOG.md` into a single commit. It is
configured (in [`release.toml`](release.toml)) to **not** publish, tag, or push
— see [Why cargo-release does so little](#why-cargo-release-does-so-little).

## Branch strategy

Feature development happens on `main`. Each major version gets a long-lived
`release/X.x` branch (created from the first release tag of that major) for
bug fixes and small enhancements without picking up feature work.

**Backport policy** — fixes are applied (when feasible) to:

| Branch | Description |
|--------|-------------|
| `release/X.x` | Current major — always patched |
| `release/(X-1).x` | Previous major — always patched |

Fix in the **oldest applicable branch** first, then merge forward through the
chain to carry the fix (and its CHANGELOG entry) into newer branches
automatically. Only skip a backport when the fix does not apply to an older
branch — document the skip in the PR description.

**Example:** a fix that applies to major 1, major 2, and main:
1. Land fix on `release/1.x`
2. Merge `release/1.x` → `release/2.x`
3. Merge `release/2.x` → `main`

The CHANGELOG entry written on `release/1.x` propagates forward via the merges
while it still lives under `## [Unreleased]` — no cherry-picking needed. Once a
branch **cuts a release**, that entry is frozen under a versioned heading, and
the cross-listing rule below takes over.

**Do not manually cherry-pick or re-apply the fix to newer branches.** The
`forward-merge-release` workflow does this automatically after every push to a
release branch. If it fails (conflict, skipped branch), resolve the conflict
in the merge PR it opens — don't work around it by applying the change twice.

**Docs-only edits on a `release/X.x` branch don't need a branch or PR.** The
feature-branch + PR rule exists to gate *code* changes. A simple documentation
update on a release branch — reconciling `CHANGELOG.md`, editing `RELEASING.md`,
`docs/`, or `README.md` — can be edited, committed, and pushed **directly** to
the `release/X.x` branch. Code changes still go through a branch + PR.

### Forward-merged fixes appear in every release that ships them

A fix lands once (on the oldest branch) but **ships in a separate release on
every branch it reaches**. Each of those releases is a distinct version with its
own changelog section, and a user reading any one of them must see the fix that
shipped in it. So:

> **Rule:** When a release inherits a fix via forward-merge, that fix's entry
> must appear under that release's version heading too — referencing the oldest
> version it was first fixed in.

This is *not* the duplicate-entry case the workflow avoids. Forward-merge keeps
the entry flowing **while it sits in `[Unreleased]`**. The gap appears later:
the older branch cuts its release first (entry freezes under, say, `[1.0.13]`),
then weeks later the newer branch cuts *its* release — and the fix, long since
merged into its code, is invisible in the new version's section because the
entry froze on the older branch.

When cutting a release on a newer branch, check what forward-merged in since its
last tag (`git log --no-merges <lasttag>..HEAD`) and add an entry for any
user-facing fix that originated downstream, attributing the origin:

```markdown
## [2.0.4]

### Fixed

- Fix `llmenv plugin-sync` dropping object-form marketplace sources
  (originally fixed in 1.0.13)
```

The `(originally fixed in X.Y.Z)` back-reference tells the user the fix is not
new behavior unique to this line — it is the same fix that shipped earlier on an
older release line, now also in this version. Reference the **oldest** version
that carried it, not the immediately-preceding branch.

**No automation enforces this — so check on every changelog edit.** Nothing
triggers a docs update when a forward-merge lands, so the cross-listing can only
be caught by hand. Make it a reflex: **any time you modify `CHANGELOG.md`** (not
only when cutting a release), first reconcile against what has forward-merged in:

```bash
# What landed since this branch's last tag, and from where?
git log --no-merges <last-tag>..HEAD
# What user-facing entries exist on the older line that aren't here yet?
git show origin/release/<older-major>.x:CHANGELOG.md
```

Add any missing user-facing fix to the appropriate section with its
`(originally fixed in X.Y.Z)` back-reference before finishing your edit. A
changelog edit that ignores an unlisted forward-merged fix is incomplete.

### Keep CI-only and internal changes out of the changelog

The changelog is for **users of the released binary**, not contributors. Per
[Keep a Changelog](https://keepachangelog.com/), omit anything with no
user-facing effect:

- CI/CD and GitHub Actions workflow changes (including the
  `forward-merge-release` workflow itself)
- test-only changes and internal refactors
- `examples/` config tweaks (illustrative, not shipped in the binary)
- dependency bumps that don't change behavior — a security bump for an advisory
  that isn't reachable from llmenv's own code is still omitted; note it in the
  PR description, not the changelog

When in doubt, ask: *would someone running the released binary notice or care?*
If not, leave it out. This applies retroactively to `[Unreleased]` — strip such
entries before cutting a release rather than freezing them under a version.

### Creating a release branch

After tagging the first release of a new major (e.g. `v2.0.0`), branch
immediately from that tag so the branch starts at exactly what was released:

```bash
git checkout -b release/2.x v2.0.0
git push -u origin release/2.x
```

The same `release/X.x` branch hosts all subsequent patch and minor releases
within that major (2.0.1, 2.1.0, …). A new branch is only created when the
major increments.

### Cutting a patch or minor release from a release branch

```bash
git switch release/2.x && git pull
cargo release patch --workspace            # dry-run preview (patch: 2.0.0 → 2.0.1)
cargo release patch --workspace --execute  # bump all crates + roll CHANGELOG + commit
git push -u origin HEAD
gh pr create --base release/2.x --fill
# After merge, tag the merged commit:
git switch release/2.x && git pull
git tag -a "v2.0.1" -m "v2.0.1"
git push origin "v2.0.1"
```

Use `cargo release minor` instead of `patch` when the accumulated changes on
`release/X.x` warrant a minor bump (e.g. `2.0.x` → `2.1.0`).

After the patch tag is pushed, merge forward into the next release branch (or
`main`) so the fix and its CHANGELOG entry propagate.

## One-time setup

```bash
cargo install cargo-release@1.1.2
```

Repo prerequisites (already in place, listed so they are not forgotten):

- **`CARGO_REGISTRY_TOKEN`** secret in the repo settings — the `publish-crate`
  job reads it. Without it, the tag build fails at publish.
- **`HOMEBREW_TAP_TOKEN`** secret — used by the `update-homebrew` job.
- The crate name **`llmenv`** must be owned by the publishing account on
  crates.io. The first publish claims it; confirm it is available beforehand.

## Cutting a release

`main` is protected (PR-only), so the version-bump commit lands through a PR and
the tag is cut on the merged commit.

### 1. Verify the changelog and prepare the bump on a branch

Before running `cargo release`, verify every user-facing change since the last
tag is listed under `[Unreleased]`. Check for and remove any CI-only, test-only,
or internal-only entries that don't affect users of the released binary.

```bash
git switch main && git pull
git log --no-merges <last-tag>..HEAD  # review what landed; compare against CHANGELOG.md
git switch -c chore/release-<next-version>
cargo release <minor|major> --workspace            # dry-run preview (default)
cargo release <minor|major> --workspace --execute  # apply: bump all crates + roll CHANGELOG, commit
```

For pre-releases, use `--version` instead of `minor`/`major`:

```bash
cargo release --version 3.0.0-rc.1 --workspace        # dry-run
cargo release --version 3.0.0-rc.1 --workspace --execute
```

`cargo release --workspace` bumps all workspace crates to the same version,
turns the `[Unreleased]` CHANGELOG section into a dated `[<version>]` section,
re-seeds a fresh `[Unreleased]` + compare link, and makes one
`chore(release): <version>` commit. The `--workspace` flag is required — without
it only the root crate is bumped, leaving sub-crates at the old version.

### 2. PR and merge

```bash
git push -u origin HEAD
gh pr create --fill
# ... review, then merge to main
```

### 3. Tag the merged commit

After the PR merges, tag the resulting `main` commit and push the tag. **This is
what triggers the release.**

```bash
git switch main && git pull
git tag -a "v<version>" -m "v<version>"
git push origin "v<version>"
```

### 4. Watch the release

```bash
gh run watch
```

CI publishes to crates.io, creates the GitHub Release, and updates Homebrew. The
crates.io publish runs exactly once per tag — re-pushing an existing tag will
fail at publish because that version already exists on the registry.

Before cutting the tag, confirm `CHANGELOG.md` is complete for the version
you're shipping — every user-facing change since the last tag must be listed
under its `[Unreleased]` section. This applies equally to pre-releases (see below).

## Pre-releases (RC, beta, alpha)

Pre-releases use standard semantic versioning format: `v3.0.0-rc.1`, `v3.0.0-beta.1`, `v3.0.0-alpha.1`.

Follow the same release process as a stable release, but **tag the prerelease version instead** of the stable version. The suffix (anything after a `-` in the version) signals to the release pipeline:

1. **GitHub Release** — marked as a prerelease, not as the latest release, so
   `/releases/latest` keeps pointing at the last stable version. Users can still
   access the prerelease via GitHub's prerelease filter or direct link, but
   third-party tools checking for latest won't auto-upgrade to an RC.
2. **crates.io** — prerelease versions publish normally (semver.org support is
   native). Users opting into the prerelease add `=3.0.0-rc.1` (with `=` pinning,
   not `^` range) to `Cargo.toml` to depend on it.
3. **Homebrew** — prerelease versions skip the Homebrew tap update entirely. Users
   testing a prerelease download the binary directly from the GitHub Release. No
   separate `--devel` formula or versioned tap formula exists today; document in
   your RC announcement that prerelease testers use `curl` / direct release downloads
   rather than Homebrew.

**Example RC workflow:**

```bash
git switch main && git pull
cargo release --version 3.0.0-rc.1 --workspace        # dry-run preview (bumps to RC)
cargo release --version 3.0.0-rc.1 --workspace --execute  # apply: bump all crates + CHANGELOG + commit
git push -u origin HEAD
gh pr create --fill  # Create PR w/ title "chore(release): 3.0.0-rc.1"
# After merge:
git switch main && git pull
git tag -a "v3.0.0-rc.1" -m "v3.0.0-rc.1"   # Prerelease tag
git push origin "v3.0.0-rc.1"
gh run watch                                           # Release workflow executes
```

After the RC test period:

1. Decide what patches/fixes are needed post-RC.
2. Apply them to `main` (on a feature branch + PR, as usual).
3. Tag the final stable version (`v3.0.0`, not another `-rc.2`) on the post-RC commit.

If the post-RC changes are substantial, consider a second RC (`v3.0.0-rc.2`) before cutting stable.

### Changelog management for pre-releases

Pre-releases get the same changelog treatment as stable releases — their
changes matter to testers and should be visible on the release page.

**1. Every pre-release gets its own `CHANGELOG.md` section.** Cut from
`[Unreleased]` at tag time, same as a stable release. The section stays in the
file permanently; pre-release sections are not deleted when the final release
ships. `cargo-release` skips `pre-release-replacements` for pre-release suffix
versions (it treats `-rc.1` as not-yet-stable and won't roll the changelog),
so `scripts/roll-prerelease-changelog.sh` — called from the `pre-release-hook`
in `release.toml` — does the same replacements manually.

**2. The GitHub Release body carries the actual changelog** — not a placeholder.
After tagging, edit the release with the content from the new `CHANGELOG.md`
section. The `## Binary Checksums` block added by CI stays; replace the
placeholder text above it. Unwrap the hard line breaks so each bullet point is
a single flowing paragraph — the fixed-width formatting in `CHANGELOG.md` is
for source readability, not the release page.

**3. On final release, roll up the beta/RC sections into a high-level summary.**
The final `X.Y.0` changelog section should be an abstracted, readable summary
of what the release line delivers — not a concatenation of every sub-release's
granular entries. The individual beta/RC sections remain in the file for
traceability; the final section is the curated version for new adopters reading
the changelog top-down. This is a manual editorial step, not automated.

**4. Breaking changes are always called out explicitly** in changelog entries,
regardless of release stage. Any `### Changed` or `### Removed` entry that
breaks backward compatibility must carry a `**BREAKING:**` prefix so testers
and release-note readers can spot it immediately.
## Why cargo-release does so little

`cargo-release` is fully capable of tagging, pushing, and publishing. We disable
all three deliberately:

- **`publish = false`** — crates.io publishing is owned by the `publish-crate`
  job in `release.yml`. If `cargo release` also published, every release would
  attempt to publish twice.
- **`tag = false` / `push = false`** — `main` is protected, so the bump commit
  must go through a PR. Tagging on the prep branch would point the tag at a
  commit that is not on `main` after merge. Cutting the tag on the merged commit
  (step 3) keeps the `v*` tag pointing at exactly what shipped.

If branch protection is ever lifted for the maintainer, `release.toml` can be
switched to `tag = true` / `push = true` for a single-command release.

## Security of the release trigger

The release is fired by the `v*` tag, and `release.yml` hands `CARGO_REGISTRY_TOKEN`
and `HOMEBREW_TAP_TOKEN` to whatever commit that tag points at. Two protections
keep an attacker from pushing a malicious tag straight to a publish:

- **`main` is branch-protected** so release content lands through review.
- **Add a tag protection rule for `v*`** (repo Settings → Tags) so only
  maintainers can create release tags.

If either protection is ever lifted, **rotate both secrets** — a contributor who
can push a `v*` tag or an arbitrary `main` commit can otherwise publish under the
project's crates.io and Homebrew credentials.

## The 1.0.0 release

1.0.0 is already prepared on `main`: `Cargo.toml` is at `1.0.0` and `CHANGELOG.md`
has a dated `[1.0.0]` section. To publish it, run **step 3** above with
`v1.0.0` — no `cargo release` run is needed for this first release. Every release
after 1.0.0 uses the full flow.
