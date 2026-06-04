# llmenv — Agent Rules

## Versioning, Changelog & Releases

Before doing **anything** that touches the version number, `CHANGELOG.md`, or a
release — read [`RELEASING.md`](RELEASING.md) and follow it.

Key invariants (full details in `RELEASING.md`):

- **A version only exists once it has been git-tagged.** Until then every change
  goes under `## [Unreleased]` in `CHANGELOG.md`.
- Never bump `Cargo.toml`'s `version` or create a `## [X.Y.Z]` changelog heading
  unless you are cutting a release (renaming `[Unreleased]` → `[X.Y.Z]`, bumping
  `Cargo.toml`, regenerating `Cargo.lock`, all in one release commit, then
  tagging `vX.Y.Z`).
- `git tag -l` is the source of truth. No tag → no version section, no bump.
- **Branch strategy:** `main` = new features. Each major.minor gets a
  `release/X.X.x` branch for bug fixes. Fix in the oldest applicable branch
  first, then merge forward — the fix and its CHANGELOG entry propagate
  automatically. See `RELEASING.md` §Branch strategy for the full policy.
- **Picking a base branch for an issue:** before branching, look at the issue's
  milestone (or version label). If a matching `release/X.Y.x` branch exists on
  the remote, branch from it — **not** from `main`. Example: an issue in the
  `1.0` milestone is a 1.0.x patch and must branch from `origin/release/1.0.x`,
  so it doesn't drag in unreleased feature work that lives only on `main`. Only
  fall back to `main` when no matching release branch exists. Check with
  `git ls-remote --heads origin 'release/*'`.

## Licensing & Attribution

llmenv is dual-licensed `MIT OR Apache-2.0` (`LICENSE-MIT`, `LICENSE-APACHE`).
Full details: [`docs/licensing.md`](docs/licensing.md).

**Hard rule — attribution notices must be documented.** Any code that carries a
license with an attribution requirement — every bundled dependency under MIT,
MIT-0, BSD-3-Clause, ISC, Apache-2.0 (NOTICE), Unicode-3.0, CDLA-Permissive-2.0,
etc., **and** any third-party source vendored/copied into this repo — must have
its copyright/permission notice reproduced in the attribution files so it ships
with the binary and is visible on the docs site.

- Two **generated** outputs, never hand-edited: `THIRD-PARTY-LICENSES.md` (ships
  with the binary/source dist) and `website/docs/third-party-licenses.md`
  (browseable on the docs site). Regenerate both with `scripts/gen-attribution.sh`.
- **Regenerate and commit them in the same change whenever `Cargo.lock`
  dependencies change** (add/remove/bump). A PR that alters dependencies but
  leaves the attribution files stale is incomplete.
- A new license id must be added to **both** `deny.toml` (`[licenses].allow`)
  and `about.toml` (`accepted`) — but only after confirming it is compatible
  with the existing set (no strong copyleft). Then regenerate.
- `cargo deny check` gates the license policy in CI and on pre-push; a rejected
  license fails the build rather than silently shipping.
