# Release Process

llmenv follows semantic versioning and automates release distribution via GitHub Actions.

## Overview

Releases are triggered by pushing a version tag (`v*`) to the main repository. The release workflow:

1. **Builds binaries** for macOS (arm64, x86_64) and Linux (x86_64)
2. **Publishes to crates.io** (requires valid `CARGO_REGISTRY_TOKEN` secret)
3. **Creates a GitHub Release** with pre-built binaries attached

## Version Tags

Create a tag in the format `vX.Y.Z`:

```bash
# Update version in Cargo.toml
cargo build --release  # Test locally first
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push origin vX.Y.Z
```

## Binary Distribution

### GitHub Releases

Pre-built binaries are attached to each release on GitHub:
- `llme-linux-x86_64` — Linux x86_64
- `llme-macos-x86_64` — macOS Intel (x86_64)
- `llme-macos-aarch64` — macOS Apple Silicon (arm64)

Users can download directly or use Homebrew (see below).

### crates.io

The Rust crate is published to [crates.io](https://crates.io/crates/llme). Install with:

```bash
cargo install llme
```

**Prerequisites:**
- A valid `CARGO_REGISTRY_TOKEN` must be set as a GitHub Actions secret
- Generate tokens at https://crates.io/me

### Homebrew

A Homebrew tap is maintained at [phaedrus1992/homebrew-tap](https://github.com/phaedrus1992/homebrew-tap).

**Install:**

```bash
brew install phaedrus1992/tap/llme
```

**Update:**

```bash
brew upgrade llme
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
   - Modify Formula/llme.rb in `phaedrus1992/homebrew-tap`
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

## Troubleshooting

**Publish fails with "unauthorized"**
- Verify `CARGO_REGISTRY_TOKEN` is valid and has `publish` scope
- Check token hasn't expired

**Binary artifacts missing from release**
- Check the `build-binaries` job succeeded
- Verify the artifact paths match in `create-release`

**Homebrew formula install fails**
- Ensure binary SHA256 hashes in Formula/llme.rb match the release binaries
- Run `brew audit --online llme` to validate the formula
