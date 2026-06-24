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
- **Branch strategy:** `main` = large features. Each major version gets a
  `release/X.x` branch for bug fixes and small enhancements. Fix in the oldest
  applicable branch first, then merge forward — the fix and its CHANGELOG entry
  propagate automatically via the `forward-merge-release` workflow. **Never
  manually cherry-pick to newer branches; let the workflow do it (or resolve the
  merge conflict it opens on failure).** See `RELEASING.md` §Branch strategy for
  the full policy.
- **Forward-merged fixes ship in every release that inherits them.** When
  cutting a release on a newer branch, any user-facing fix that forward-merged
  in from an older line must also appear under the new version's changelog
  heading, back-referenced to the oldest version that carried it — e.g.
  `(originally fixed in 1.0.13)`. The auto-propagation only covers entries still
  under `[Unreleased]`; once an older branch cuts its release the entry freezes
  there, so the newer release must re-list it. See `RELEASING.md`
  §"Forward-merged fixes appear in every release that ships them".
- **No trigger keeps this in sync — so check on *every* `CHANGELOG.md` edit,**
  not just at release time. Before finishing any changelog change, reconcile
  against the older release line (`git log --no-merges <last-tag>..HEAD` and the
  older branch's `CHANGELOG.md`) and add any missing forward-merged fix with its
  back-reference. A changelog edit that leaves an inherited fix unlisted is
  incomplete.
- **Picking a base branch for an issue:** look at the issue's milestone.
  **Bug Fixes** and **Small Enhancements** branch from the newest `release/X.x`
  branch. **Large Features** branch from `main`. Check available release branches
  with `git ls-remote --heads origin 'release/*'`.

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
