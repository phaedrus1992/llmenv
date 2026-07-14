# Maintainers

Operational docs for releasing and packaging llmenv.

- [Release process](release.md) — cutting a version: changelog, `Cargo.toml`
  bump, tagging, and the release workflow. **Read this before touching the
  version number, `CHANGELOG.md`, or a release.**
- [Homebrew tap setup](homebrew-tap-setup.md) — configuring and publishing the
  Homebrew tap.

## Branch strategy

Feature development happens on `main`. Each major.minor version gets a
`release/X.X.x` long-lived branch for bug fixes. Fix in the oldest applicable
branch first, then merge forward — the fix and its CHANGELOG entry propagate
automatically. See [release.md](release.md#branch-strategy) for the full policy
and patch-release workflow.

## Versioning invariant

A version exists only once it has been git-tagged. Until then, every change goes
under `## [Unreleased]` in
[`CHANGELOG.md`](https://github.com/phaedrus1992/llmenv/blob/main/CHANGELOG.md). `git tag -l`
is the source of truth <!-- markdownlint-disable-line MD013 -->
source of truth — no tag means no version section and no `Cargo.toml` bump. Full
details in [release.md](release.md).

## Design docs

- [Engine capabilities](https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md)
  — the two-layer <!-- markdownlint-disable-line MD013 -->
  (neutral + per-engine `native`) capability model.
