# llmenv — Agent Rules

## Versioning, Changelog & Releases

Before doing **anything** that touches the version number, `CHANGELOG.md`, or a
release — read [`docs/release.md`](docs/release.md) and follow it.

Key invariant (full details in `docs/release.md`):

- **A version only exists once it has been git-tagged.** Until then every change
  goes under `## [Unreleased]` in `CHANGELOG.md`.
- Never bump `Cargo.toml`'s `version` or create a `## [X.Y.Z]` changelog heading
  unless you are cutting a release (renaming `[Unreleased]` → `[X.Y.Z]`, bumping
  `Cargo.toml`, regenerating `Cargo.lock`, all in one release commit, then
  tagging `vX.Y.Z`).
- `git tag -l` is the source of truth. No tag → no version section, no bump.
