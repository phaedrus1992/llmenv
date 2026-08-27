# Contributing to llmenv

Thanks for contributing. This document covers branch and PR conventions, how
milestones map to branches, and how to run the test and lint gates locally.

## Before you start

Read [AGENTS.md](AGENTS.md) for the project's development rules (where new
features belong, versioning and changelog requirements, licensing rules) and
[RELEASING.md](RELEASING.md) for the branch and release process. Both apply
to every contribution, not just AI-assisted ones.

## Branches

Name your branch by the kind of change, with a short kebab-case slug:

- `fix/<issue>-<slug>` — bug fixes
- `feat/<issue>-<slug>` — new features
- `docs/<issue>-<slug>` — documentation-only changes
- `chore/<issue>-<slug>` — internal changes with no user-facing effect

Reference the issue number in the branch name when one exists.

**Which branch to fork from** depends on the issue's milestone — see
[RELEASING.md's "Branch strategy"](RELEASING.md#branch-strategy) for the full
policy. In short: version-numbered milestones (`vX.Y.Z`) map to a
`release/X.Y` branch when one exists; large, `main`-only features fork from
`main`.

A direct commit or push to `main` or a `release/*` branch is blocked by a git
hook — all changes go through a pull request.

## Commits

- Imperative mood, one logical change per commit (`fix: reject empty host
  before DNS lookup`, not `fixed stuff`).
- Reference the issue you're closing in the commit body, not the subject:
  `Fixes #123` (GitHub auto-closes the issue when the PR merges).

## Pull requests

- Keep the title under 70 characters.
- Describe what the code does now, not the sequence of changes that got it
  there.
- Include a closing reference (`Fixes #123` or `Closes #123`) for every issue
  the PR resolves.
- Every user-facing change (a fix, a new feature, a behavior change) needs a
  `CHANGELOG.md` entry under `## [Unreleased]` — see AGENTS.md's changelog
  rules for what counts and what's exempt.

## Running the test and lint gates locally

Install the git hooks once, right after cloning:

```bash
prek install --hook-type pre-commit --hook-type pre-push
```

- **On commit** (fast): `cargo fmt --check`, `markdownlint-cli2`.
- **On push** (slower): `cargo clippy --all-features --tests -- -D warnings`,
  a changelog-sync check, and `cargo hawk check`.

Install [`prek`](https://github.com/j178/prek) if you don't have it, and
[`markdownlint-cli2`](https://github.com/DavidAnson/markdownlint-cli2) for the
markdown hook to run.

These hooks cover what's fast enough to run on every commit or push. CI runs
the rest — `cargo test` (via `cargo nextest`), `cargo deny check`, and, for
GitHub Actions workflow changes, [`zizmor`](https://github.com/woodruffw/zizmor).
Run these locally before opening a PR if your change touches the relevant area:

```bash
cargo test --workspace       # or: cargo nextest run --workspace
cargo deny check             # dependency license/advisory policy
```

See [README.md's "Development"](README.md#development) section for optional
build-speed tooling (`sccache`, `mold`/`lld`).
