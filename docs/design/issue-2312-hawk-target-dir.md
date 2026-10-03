# Issue #2312 — build cargo-hawk into `target/` so `cargo clean` removes it

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2312
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** CI and developer tooling fix

This is a spec, not a plan.

## Problem

`scripts/hawk-check.sh` runs `cargo hawk check` without `--target-dir`.
cargo-hawk then builds into `$TMPDIR/cargo-hawk-target/<workspace>-<hash>/`, one directory per checkout or worktree.
Nothing removes those directories: `cargo clean` only cleans `target/`, `cargo clean --target-dir` refuses a directory that is not a cargo target dir, and a removed worktree leaves its hawk dir behind.
A pre-push run failed with `No space left on device` while `target/` held about 199 GiB and the hawk directories about 75 GB more.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `scripts/hawk-check.sh` builds `--exclude-crate` arguments for the four published crates and runs `exec cargo hawk check "${args[@]}" "$@"`; extra arguments are forwarded (`-D warnings` from both callers) | `scripts/hawk-check.sh` |
| The CI `hawk` job installs cargo-hawk and runs `bash scripts/hawk-check.sh -D warnings`; it is a required status check | `.github/workflows/ci.yml` |
| The pre-push hook `cargo-hawk` runs `bash scripts/hawk-check.sh -D warnings` | `.pre-commit-config.yaml` |
| `cargo hawk check` accepts `--target-dir <TARGET_DIR>` (issue text, cargo-hawk 0.1.13) | cargo-hawk |
| The CI job caches Rust builds with the repo's usual cache action; whether `target/hawk` is inside the cached path depends on that step's `path` | `.github/workflows/ci.yml` |

## Decisions

1. **Default the target dir to `${CARGO_TARGET_DIR:-target}/hawk`.**
   The instrumented build then lives under the workspace target dir, `cargo clean` removes it, and each worktree cleans its own.
   A caller who passes `--target-dir` explicitly wins: scan `"$@"` for `--target-dir` or `--target-dir=` and skip the default when found.
2. **Resolve `CARGO_TARGET_DIR` relative to the workspace root**, not the caller's cwd, since the pre-push hook can run from a subdirectory.
   Use `git rev-parse --show-toplevel` or `cargo metadata --format-version 1 | jq -r .target_directory` (prefer the cargo one; it already honors `.cargo/config.toml` `build.target-dir`, and `jq` is available in CI and on dev machines; if `jq` is not guaranteed, fall back to the git form).
3. **Check the CI cache.**
   If the cache step's `path` includes `target/`, hawk's build is now cached too, which is faster.
   If the cache would grow past a budget the job cares about, exclude `target/hawk` from it in the same change and say why.
4. **Document the one-time cleanup** in the PR body: `trash "${TMPDIR:-/tmp}/cargo-hawk-target"` (or `rm -rf` where `trash` is absent), with the note that the stale per-checkout dirs are otherwise never removed.

## Design

### `scripts/hawk-check.sh`

- Keep `set -euo pipefail` and the exclusion list.
- Add a `has_target_dir` loop over `"$@"`.
- Add `target_dir="$(cargo metadata --no-deps --format-version 1 | jq -r .target_directory)/hawk"` behind the check, with the fallback described above.
- Pass `--target-dir "$target_dir"` before `"$@"`.
- Update the header comment: why the target dir is forced (this issue), and that `CARGO_TARGET_DIR` and a caller's `--target-dir` are honored.
- `shellcheck` and `shfmt` clean.

### `.github/workflows/ci.yml`

- Read the hawk job's cache step; adjust `path` only if decision 3 requires it.
- Add a comment on the hawk step that the build goes to `target/hawk`.

### Docs

- `website/docs/maintainers.md` (developer setup): one line that hawk builds into `target/hawk` and that `cargo clean` removes it. No changelog entry (tooling only).

## Tests

1. Shell test under `.github/workflows/__tests__/` or `scripts/` (follow the existing guard test's style): with a stub `cargo` on `PATH` that records its arguments, the script passes `--target-dir <root>/target/hawk` by default; with `CARGO_TARGET_DIR=/x` it passes `/x/hawk`; with a caller `--target-dir /y` it passes only `/y`; `-D warnings` is still forwarded last.
2. Local: `bash scripts/hawk-check.sh -D warnings` builds into `target/hawk`; `cargo clean` removes it.
3. CI hawk job green.

## Acceptance criteria

1. After a hawk run, no new directory appears under `$TMPDIR/cargo-hawk-target/`.
2. `cargo clean` removes the hawk build.
3. CI and the pre-push hook both go through the script and pass.
4. The PR body carries the one-time cleanup command.

## Out of scope

- Removing stale hawk directories automatically.
- Changing the exclusion list or the hawk toolchain pin.
