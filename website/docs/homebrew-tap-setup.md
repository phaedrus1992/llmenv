# Homebrew Tap Setup

This document guides the creation and maintenance of the `phaedrus1992/homebrew-tap` repository.

## Initial Setup

### 1. Create the Tap Repository

On GitHub, create a new repository named `homebrew-tap` in the `phaedrus1992` account:
- Visibility: Public
- Initialize with README

### 2. Repository Structure

```
homebrew-tap/
├── Formula/
│   └── llmenv.rb
└── README.md
```

### 3. Add the Formula

Create `Formula/llmenv.rb` using the template from the llmenv docs. The formula:
- Downloads pre-built binaries from GitHub releases
- Installs to `/usr/local/bin/llmenv`
- Includes version detection and architecture-specific URLs

**Template:** See `.tmp/homebrew-formula-llmenv.rb` in the llmenv repository.

### 4. SHA256 Hashes

SHA256 hashes are updated automatically by the `update-homebrew` job in
`release.yml`. When a `v*` tag is pushed, that job reads the checksums from the
build artifacts and triggers the `update-formula.yml` workflow in
`phaedrus1992/homebrew-tap` via the `HOMEBREW_TAP_TOKEN` secret.

For the initial setup (before automation is wired), compute hashes manually:

```bash
curl -L https://github.com/phaedrus1992/llmenv/releases/download/vX.Y.Z/llmenv-macos-aarch64 | shasum -a 256
curl -L https://github.com/phaedrus1992/llmenv/releases/download/vX.Y.Z/llmenv-macos-x86_64 | shasum -a 256
```

### 5. Test Locally

```bash
# Uninstall any existing installation
brew uninstall llmenv || true

# Link the local formula
brew install --build-from-source ./Formula/llmenv.rb

# Verify
llmenv --version
```

### 6. CI/CD

Add a GitHub Actions workflow to the tap repo to validate the formula on each PR:

```yaml
name: Validate Formula

on: [push, pull_request]

jobs:
  validate:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v6
      - name: Validate formula
        run: brew audit --online Formula/llmenv.rb
      - name: Test install
        run: brew install --build-from-source ./Formula/llmenv.rb && llmenv --version
```

## Workflow Integration

The llmenv main repository's `release.yml` workflow is fully automated:
1. Builds binaries for macOS (arm64, x86_64) and Linux (x86_64)
2. Creates a GitHub Release with binaries, checksums, and SLSA provenance
3. Publishes to crates.io
4. **Automatically** triggers `update-formula.yml` in `phaedrus1992/homebrew-tap`
   with the new version and SHA256 hashes via the `HOMEBREW_TAP_TOKEN` secret

No manual formula update is needed after a release.

## Installation Instructions

Users install via:

```bash
brew tap phaedrus1992/homebrew-tap
brew install llmenv
```

Or in one command:

```bash
brew install phaedrus1992/homebrew-tap/llmenv
```

## Maintenance

### Upgrade Release

Formula updates are handled automatically by the release workflow (see
[Workflow Integration](#workflow-integration)). If you need to update manually
(e.g. to recover from a failed automation run):

1. Download the new binaries from the GitHub release
2. Compute SHA256: `shasum -a 256 llmenv-macos-*`
3. Update `version` and `sha256` values in `Formula/llmenv.rb`
4. Test locally: `brew install --build-from-source ./Formula/llmenv.rb`
5. Open a PR in the tap repo

### Troubleshooting

**Installation fails with "not found"**
- Verify the formula's `url` points to a valid release binary
- Check SHA256 hashes are correct

**Formula audit fails**
- Run `brew audit --online Formula/llmenv.rb` for details
- Common issues: missing description, invalid license format

**Binary doesn't work on M1/M2 (arm64)**
- Ensure the arm64 binary was built (check llmenv release CI)
- Verify architecture detection in the formula (`on_arm` block)
