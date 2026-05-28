# Maintainers

Operational docs for releasing and packaging llmenv.

- [Release process](release.md) — cutting a version: changelog, `Cargo.toml`
  bump, tagging, and the release workflow. **Read this before touching the
  version number, `CHANGELOG.md`, or a release.**
- [Homebrew tap setup](homebrew-tap-setup.md) — configuring and publishing the
  Homebrew tap.

## Versioning invariant

A version exists only once it has been git-tagged. Until then, every change goes
under `## [Unreleased]` in [`CHANGELOG.md`](../CHANGELOG.md). `git tag -l` is the
source of truth — no tag means no version section and no `Cargo.toml` bump. Full
details in [release.md](release.md).

## Design docs

- [Engine capabilities](design/engine-capabilities.md) — the two-layer
  (neutral + per-engine `native`) capability model.
