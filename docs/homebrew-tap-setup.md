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
│   └── llme.rb
└── README.md
```

### 3. Add the Formula

Create `Formula/llme.rb` using the template from the llmenv docs. The formula:
- Downloads pre-built binaries from GitHub releases
- Installs to `/usr/local/bin/llme`
- Includes version detection and architecture-specific URLs

**Template:** See `.tmp/homebrew-formula-llme.rb` in the llmenv repository.

### 4. Update SHA256 Hashes

After each release, update the SHA256 hashes in `Formula/llme.rb`:

```bash
# Download the binary and compute its hash
curl -L https://github.com/phaedrus1992/llmenv/releases/download/vX.Y.Z/llme-macos-aarch64 | shasum -a 256
curl -L https://github.com/phaedrus1992/llmenv/releases/download/vX.Y.Z/llme-macos-x86_64 | shasum -a 256
```

Update the `sha256` values in `Formula/llme.rb`.

### 5. Test Locally

```bash
# Uninstall any existing installation
brew uninstall llme || true

# Link the local formula
brew install --build-from-source ./Formula/llme.rb

# Verify
llme --version
```

### 6. CI/CD (Optional)

Add a GitHub Actions workflow to the tap repo to validate the formula:

```yaml
name: Validate Formula

on: [push, pull_request]

jobs:
  validate:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v6
      - name: Validate formula
        run: brew audit --online Formula/llme.rb
      - name: Test install
        run: brew install --build-from-source ./Formula/llme.rb && llme --version
```

## Workflow Integration

The llmenv main repository's `release.yml` workflow handles:
1. Building binaries for macOS (arm64, x86_64)
2. Creating GitHub releases with binaries attached
3. Publishing to crates.io

**Manual step:** After release, update `Formula/llme.rb` with new SHA256 hashes and open a PR in the tap repo.

**Automation opportunity:** Future enhancement could auto-generate and update the formula via a workflow.

## Installation Instructions

Users install via:

```bash
brew tap phaedrus1992/homebrew-tap
brew install llme
```

Or in one command:

```bash
brew install phaedrus1992/homebrew-tap/llme
```

## Maintenance

### Upgrade Release

When llmenv releases a new version:

1. Check GitHub releases for new binaries
2. Update `version` in `Formula/llme.rb`
3. Compute and update SHA256 hashes
4. Test locally
5. Commit and push to the tap repo

### Troubleshooting

**Installation fails with "not found"**
- Verify the formula's `url` points to a valid release binary
- Check SHA256 hashes are correct

**Formula audit fails**
- Run `brew audit --online Formula/llme.rb` for details
- Common issues: missing description, invalid license format

**Binary doesn't work on M1/M2 (arm64)**
- Ensure the arm64 binary was built (check llmenv release CI)
- Verify architecture detection in the formula (`on_arm` block)
