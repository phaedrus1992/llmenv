# llmenv — Agent Rules

## Where new development happens

**New features go in the `llmenv` core (the Rust crates under `src/` and
`crates/`) unless explicitly told otherwise.** Built-in capabilities (hooks,
env injection, MCP wiring) are implemented in core and ship with the binary —
see ICM (`src/icm.rs`) and the adapter-injected hooks in
`src/adapter/claude_code.rs` for the reference pattern.

`examples/` (notably `examples/my-llmenv/`) is **illustrative configuration
only — never a target for new feature development.** It demonstrates how a user
configures llmenv; it does not house product code. Do not add features there.

## ICM interaction: MCP only, never the CLI

**All runtime ICM interaction must go through the ICM MCP, not the `icm` CLI.**
llmenv may run on a machine that is **not** the primary ICM host (the resolved
`icm` MCP endpoint can be a remote `icm serve` — see `src/mcp/resolve.rs`). The
`icm` CLI writes to the *local* sqlite store, which on a non-host machine is the
wrong store and silently diverges. Always issue `icm_*` MCP tool calls against
the resolved endpoint. The only non-MCP use of the `icm` binary is launching
`icm serve` itself (the server the MCP talks to).

## Versioning, Changelog & Releases

Before doing **anything** that touches the version number, the changelog, or a
release — read [`RELEASING.md`](RELEASING.md) and follow it.

**Hard rule — every user-facing change needs a changelog entry.** Any code
change that a user can observe (bug fix, new feature, behavior change,
deprecation, performance improvement, security fix, removed feature) gets an
entry under `## [Unreleased]` following
[keepachangelog](https://keepachangelog.com) formatting. No exceptions.
Internal refactors, test-only changes, and documentation-only changes that
don't alter behavior are exempt. If unsure, write the entry — silence about a
change is worse than a changelog entry that's slightly too verbose.

**Hard rule — changelog entries must be backed by up-to-date docs.** Any time
you add or modify a changelog entry, verify that the relevant feature or
change is adequately documented in `website/docs/`. If the docs don't cover
it, or if they describe the old behavior, update them in the same change.
"Search the docs for the feature name" is not enough — the docs must
correctly describe the new behavior end-to-end.

Key invariants (full details in `RELEASING.md`):

- **A version only exists once it has been git-tagged.** Until then every change
  goes under `## [Unreleased]` in the changelog.
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
- **No trigger keeps this in sync — so check on *every* changelog edit,**
  not just at release time. Before finishing any changelog change, reconcile
  against the older release line (`git log --no-merges <last-tag>..HEAD` and the
  older branch's changelog) and add any missing forward-merged fix with its
  back-reference. A changelog edit that leaves an inherited fix unlisted is
  incomplete.
- **When wrapping up `dev-sprint` or `ship-issue`, always evaluate
  the changelog — every user-facing change (fix, feature, enhancement, deprecation, breaking, security)
  from the current work needs an entry under `[Unreleased]` following
  [keepachangelog](https://keepachangelog.com) formatting. Invoke the
  `keepachangelog` skill to check and write entries. Don't leave the task
  without either adding entries or making a deliberate call that the work
  has no user-facing changes.
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
