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

Feature development happens on `main`. Each major.minor version gets a long-lived
`release/X.X.x` branch (created from the release tag) for managing bug fixes
without picking up new feature work.

**Backport policy** — fixes are applied (when feasible) to:

| Branch | Description |
|--------|-------------|
| `release/X.X.x` | Current major.minor — always patched |
| `release/X.(X-1).x` | Previous minor of the current major — always patched |
| `release/(X-1).Y.x` | Last minor branch of the previous major — always patched |

Fix `main` first (canonical source), then cherry-pick to the applicable release
branches. Only skip a backport when the fix does not apply cleanly and the effort
to adapt it outweighs the value — document the skip in the PR description.

### Creating a release branch

After tagging a new major.minor (e.g. `v1.1.0`), branch immediately from that
tag so the branch starts at exactly what was released:

```bash
git checkout -b release/1.1.x v1.1.0
git push -u origin release/1.1.x
```

### Cutting a patch release from a release branch

The flow mirrors the main-branch release, but targets the release branch instead
of main. Fix commits should already be cherry-picked onto the branch before this.

```bash
git switch release/1.1.x && git pull
cargo release patch            # dry-run preview
cargo release patch --execute  # bump Cargo.toml + roll CHANGELOG + commit
git push -u origin HEAD
gh pr create --base release/1.1.x --fill
# After merge, tag the merged commit:
git switch release/1.1.x && git pull
git tag -a "v1.1.1" -m "v1.1.1"
git push origin "v1.1.1"
```

`cargo release patch` on the release branch bumps only within its minor line
(1.1.0 → 1.1.1). CHANGELOG entries accumulate on the release branch; do not
merge the release-branch CHANGELOG back to `main` — the two diverge intentionally.

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

### 1. Prepare the bump on a branch

```bash
git switch main && git pull
git switch -c release/<next-version>
cargo release <patch|minor|major>            # dry-run preview (default)
cargo release <patch|minor|major> --execute  # apply: bump Cargo.toml + roll CHANGELOG, commit
```

`cargo release` rewrites `Cargo.toml`'s version, turns the `[Unreleased]`
CHANGELOG section into a dated `[<version>]` section, re-seeds a fresh
`[Unreleased]` + compare link, and makes one `chore(release): <version>` commit.

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
