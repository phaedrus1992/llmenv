# Release Process

llmenv follows semantic versioning and automates release distribution via GitHub Actions.

## Overview

Releases are triggered by pushing a version tag (`v*`) to the **main branch only**. The release workflow:

1. **Builds binaries** for macOS (arm64, x86_64) and Linux (x86_64)
2. **Generates SHA256 checksums** for all binaries (automatic)
3. **Publishes to crates.io** (requires valid `CARGO_REGISTRY_TOKEN` secret)
4. **Creates a GitHub Release** with pre-built binaries and checksums attached

## Version Tags

Create a tag in the format `vX.Y.Z` and push it to the **main branch**:

```bash
# Update version in Cargo.toml
cargo build --release  # Test locally first
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push origin main vX.Y.Z  # Push to main branch
```

**Important:** Tags must be pushed from the main branch. Releases triggered from feature branches are blocked by the workflow.

## Binary Distribution

### GitHub Releases

Pre-built binaries are attached to each release on GitHub:
- `llmenv-linux-x86_64` — Linux x86_64
- `llmenv-macos-x86_64` — macOS Intel (x86_64)
- `llmenv-macos-aarch64` — macOS Apple Silicon (arm64)
- `checksums.txt` — SHA256 checksums for all binaries

### Verify Binary Integrity

Each release includes a `checksums.txt` file. Verify downloaded binaries:

```bash
sha256sum -c checksums.txt
```

All binaries are automatically checksummed during the release build.

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

The release workflow requires:
- `CARGO_REGISTRY_TOKEN` — crates.io API token (scoped to publish only)

Set this in the repository settings under Secrets and variables → Actions.

**Security notes:**
- The token is passed via environment variable (never command-line arguments)
- GitHub Actions automatically masks secret values in logs
- Always use fine-grained tokens with minimal scope (publish-only)

## Future Work: SLSA Provenance and Homebrew Automation

The following enhancements are tracked for future releases:

### SLSA Build Provenance
- Integrate `slsa-framework/slsa-github-generator` for cryptographic proof of build chain
- Attach SLSA provenance to GitHub releases
- Enables users to verify binaries were built from claimed source by GitHub Actions

### Homebrew Automation
- Auto-generate and update Formula/llmenv.rb SHA256 hashes after release
- Reduce manual steps and error potential in the homebrew-tap repo
- Trigger automation from llmenv release workflow

## Troubleshooting

**Release workflow doesn't trigger**
- Verify tag was pushed to the **main branch**
- Workflow only runs when tag is pushed to main, not feature branches
- Check GitHub Actions tab for workflow run details

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
