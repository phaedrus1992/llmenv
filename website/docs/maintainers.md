<!-- markdownlint-disable MD013 -->

# Maintainers

Operational docs for releasing and packaging llmenv.

- [Release process](release.md) — cutting a version: changelog, `Cargo.toml`
  bump, tagging, and the release workflow. **Read this before touching the
  version number, `CHANGELOG-<major>.md`, or a release.**
- [Homebrew tap setup](homebrew-tap-setup.md) — configuring and publishing the
  Homebrew tap.

## Branch strategy

Feature development happens on `main`. Each major version gets a
`release/X.x` long-lived branch for bug fixes and small enhancements. Fix in the oldest applicable
branch first, then merge forward — the `forward-merge-release` workflow carries the fix and its CHANGELOG entry
up to `main`. See [release.md](release.md#branch-strategy) for the full policy
and patch-release workflow.

## Versioning invariant

A version exists only once it has been git-tagged. Until then, every change goes
under `## [Unreleased]` in
the `CHANGELOG-<major>.md` file for the line you are on (see [changelog](changelog.md)). `git tag -l`
is the source of truth — no tag means no version section and no `Cargo.toml` bump. Full
details in [release.md](release.md).

## Design docs

- [Engine capabilities](https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md)
  — the two-layer <!-- markdownlint-disable-line MD013 -->
  (neutral + per-engine `native`) capability model.

## Continuous integration

The workflows are in `.github/workflows/`.

- `ci` runs on each pull request. The `test` job runs `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo nextest run --profile ci`. The `deny` and `hawk` jobs run `cargo deny` and `scripts/hawk-check.sh`. `test`, `deny`, and `hawk` are required checks, and each job skips itself when its files did not change.
- `coverage` runs `cargo llvm-cov` on a pull request that changes Rust, test, script, or changelog files. It fails when total line coverage is below 64%. Raise the floor when the coverage rises; never lower it.
- `mutants` runs `cargo mutants` on a pull request that touches `crates/llmenv-config`, `src/merge`, or `src/hook_run`. It tests only the mutants on the changed lines, and the job fails when one survives. A full sweep runs every Monday at 04:00 UTC and only reports.
- `codeql` scans Python, JavaScript and TypeScript, Rust, and the workflows. A pull request scans only the languages whose files changed, and uploads an empty result for the others. A push to `main` or `release/**` and the weekly run (Monday 05:00 UTC) scan every language, so an unchanged language never closes its open alerts.
- `forward-merge-release` merges each `release/X.x` push into the next branch up to `main`. See [Forward-merge workflow](release.md#forward-merge-workflow).
- `release` runs when a `v*` tag is pushed. See [Release process](release.md).
- `docs` builds the website, and deploys it when `main` changes `website/`.

## Developer tooling

- `scripts/hawk-check.sh` (the CI `hawk` job and the `cargo-hawk` pre-push hook) builds into `target/hawk`,
  so `cargo clean` removes the hawk build.
  Before this, hawk built into `$TMPDIR/cargo-hawk-target/`, which nothing cleaned.
  Remove old directories once with `trash "${TMPDIR:-/tmp}/cargo-hawk-target"`.
  A `--target-dir` argument or `CARGO_TARGET_DIR` overrides the default.
- `.config/nextest.toml` sets a 2 s `leak-timeout`, because the 100 ms default flags file-I/O tests
  under full parallel load.
  CI runs `cargo nextest run --profile ci`, where a leaked handle fails the run.
