# Release Process

llmenv follows semantic versioning and automates release distribution via GitHub Actions.

## Overview

Releases are triggered by pushing a `v*` tag. The release workflow:

1. **Builds binaries** for macOS (arm64, x86_64) and Linux (x86_64)
2. **Generates SHA256 checksums** and SLSA v1.0 provenance attestations
3. **Publishes to crates.io** (requires valid `CARGO_REGISTRY_TOKEN` secret)
4. **Creates a GitHub Release** with binaries, checksums, SLSA provenance, and the changelog notes for the version in the release body

## Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/) and
[Semantic Versioning](https://semver.org/). There is exactly one rule that
matters and it is easy to get wrong:

> **A version section only exists once that version has been git-tagged.
> Until then, everything lives under `## [Unreleased]`.**

The `Cargo.toml` version and the changelog must never run ahead of the tags.
If `git tag -l` shows no `vX.Y.Z` tag, there is no `[X.Y.Z]` changelog section
and `Cargo.toml` is not bumped to it. (This repo previously accumulated
phantom `1.0.0`/`1.1.0`/`1.2.0` sections with no tags behind them — don't
recreate that.)

### While developing (every change)

Add an entry under `## [Unreleased]` in the appropriate category. Do **not**
touch the `Cargo.toml` version and do **not** create a new version heading.

Categories (Keep a Changelog): `Added`, `Changed`, `Deprecated`, `Removed`,
`Fixed`, `Security`. This repo also uses `Documentation`. Reference the issue/PR
number in the entry, e.g. `(#63)`.

```markdown
## [Unreleased]

### Added

- New `llmenv foo` subcommand that does X. (#81)
```

### When cutting a release (and only then)

Use [`cargo-release`](https://github.com/crate-ci/cargo-release) to prepare the
bump. It atomically renames `[Unreleased]` → `[X.Y.Z] - YYYY-MM-DD`, re-seeds a
fresh `[Unreleased]`, bumps `Cargo.toml`, updates `Cargo.lock`, and makes a
single `chore(release): X.Y.Z` commit.

```bash
cargo release <patch|minor|major>            # dry-run preview (default)
cargo release <patch|minor|major> --execute  # apply
```

It is configured (in `release.toml`) to **not** tag, publish, or push — all
three happen in the steps below. Full details in [`RELEASING.md`](https://github.com/phaedrus1992/llmenv/blob/main/RELEASING.md).

## Version Tags

`main` is PR-protected, so the version-bump commit lands through a PR first.
After merge, tag the merged commit and push the tag — **that is what fires the
release workflow.**

```bash
# 1. Create a prep branch, run cargo-release, open a PR, merge it.
git switch main && git pull

# 2. Tag the merged commit and push the tag.
git tag -a "vX.Y.Z" -m "vX.Y.Z"
git push origin "vX.Y.Z"
```

The release workflow triggers on any `v*` tag push. Tags should always point at a
commit on `main`; the workflow has no branch filter, so a tag pointed at a stale
commit would still fire — make sure you're tagging the merged result.

## Binary Distribution

### GitHub Releases

Pre-built binaries are attached to each release on GitHub:
- `llmenv-linux-x86_64` — Linux x86_64
- `llmenv-macos-x86_64` — macOS Intel (x86_64)
- `llmenv-macos-aarch64` — macOS Apple Silicon (arm64)
- `checksums.txt` — SHA256 checksums for all binaries
- `*.intoto.jsonl` — SLSA v1.0 provenance attestations (one per binary)

### Verify Binary Integrity

Each release includes `checksums.txt`. Verify downloaded binaries:

```bash
sha256sum -c checksums.txt
```

### Verify Supply Chain (SLSA)

Each binary ships with a SLSA v1.0 provenance attestation. Verify that the
binary was built by GitHub Actions from the claimed source:

```bash
slsa-verifier verify-artifact \
  --artifact-path=<binary> \
  --provenance=<provenance.intoto.jsonl> \
  --source-uri=github.com/phaedrus1992/llmenv
```

### crates.io

The Rust crate is published to [crates.io](https://crates.io/crates/llmenv). Install with:

```bash
cargo install llmenv
```

**Prerequisites:**
- A valid `CARGO_REGISTRY_TOKEN` must be set as a GitHub Actions secret
- Generate tokens at https://crates.io/me

### Homebrew

A Homebrew tap is maintained at [phaedrus1992/homebrew-tap](https://github.com/phaedrus1992/homebrew-tap).

**Install:**

```bash
brew install phaedrus1992/tap/llmenv
```

**Update:**

```bash
brew upgrade llmenv
```

## Maintenance

### Adding a new platform

To add a new platform (e.g., Windows, aarch64 Linux):

1. Update `.github/workflows/release.yml`:
   - Add a new matrix entry under `build-binaries`
   - Set `os`, `target`, `asset_name`
   
2. Test locally:
   ```bash
   rustup target add <target>
   cargo build --release --target <target>
   ```

3. Update Homebrew formula if a new macOS target is added:
   - Modify Formula/llmenv.rb in `phaedrus1992/homebrew-tap`
   - Add conditional blocks for the new architecture

### Rollback

If a release needs to be pulled:

```bash
# Mark as pre-release on GitHub (manual UI)
# Unpublish from crates.io (requires crates.io owner access)
cargo yank --vers X.Y.Z
# Remove from Homebrew (PR to phaedrus1992/homebrew-tap)
```

## Secrets Configuration

The release workflow requires two secrets (repo Settings → Secrets and variables → Actions):

- `CARGO_REGISTRY_TOKEN` — crates.io API token (scoped to publish only)
- `HOMEBREW_TAP_TOKEN` — GitHub PAT with write access to `phaedrus1992/homebrew-tap`

**Security notes:**
- The token is passed via environment variable (never command-line arguments)
- GitHub Actions automatically masks secret values in logs
- Always use fine-grained tokens with minimal scope (publish-only)

## Branch Strategy

Feature development happens on `main`. Each major.minor version gets a
`release/X.X.x` branch (created from the release tag) for managing bug fixes
without picking up new feature work.

**Backport policy** — fixes are applied (when feasible) to:

| Branch | Description |
|--------|-------------|
| `release/X.X.x` | Current major.minor — always patched |
| `release/X.(X-1).x` | Previous minor of the current major — always patched |
| `release/(X-1).Y.x` | Last minor branch of the previous major — always patched |

Fix `main` first, cherry-pick to applicable release branches, then cut a patch
release from the branch. Full workflow in
[`RELEASING.md`](https://github.com/phaedrus1992/llmenv/blob/main/RELEASING.md).

## Troubleshooting

**Release workflow doesn't trigger**
- Verify the tag was pushed: `git push origin "vX.Y.Z"`
- Check GitHub Actions tab for the workflow run
- Confirm the tag format matches `v*` (the workflow trigger pattern)

**Publish fails with "unauthorized"**
- Verify `CARGO_REGISTRY_TOKEN` is valid and has `publish` scope
- Check token hasn't expired

**Binary artifacts missing from release**
- Check the `build-binaries` job succeeded in Actions tab
- Verify artifact paths match in `create-release`
- Checksums should always be generated automatically

**Checksum verification fails**
- Ensure you're on the same system/shell as the release CI
- Download the binary and checksums.txt from the same release
- Run `sha256sum -c checksums.txt` in the download directory
