#!/usr/bin/env bash
# Run `cargo hawk check` with this workspace's external-boundary crates excluded.
#
# Shared by the CI `hawk` job and the `cargo-hawk` pre-push hook so the two can't
# drift — the exclusion list is the whole point and is easy to forget in one of
# the two call sites.
#
# `crates/*` are published to crates.io by .github/workflows/release.yml, so their
# public API is a real external surface, not internal scaffolding: narrowing a
# `pub` there to `pub(crate)` is a breaking change for any downstream consumer,
# not a cleanup. hawk reaches them from the `llmenv` binary and would otherwise
# report every item the binary happens not to call as unnecessarily public
# (#1314). `--exclude-crate` marks a crate's API as an external boundary, which
# is exactly that contract; hawk.toml has no equivalent key, so it has to be
# passed on the command line.
#
# Crate names here are the *lib target* names (underscores), not package names.
#
# cargo-hawk builds into $TMPDIR/cargo-hawk-target/<workspace>-<hash>/ unless told
# otherwise, and nothing ever removes those directories: `cargo clean` skips them,
# and a deleted worktree leaves its directory behind (#2312). This script builds
# into `<target dir>/hawk` instead, so `cargo clean` removes the hawk build too.
# `CARGO_TARGET_DIR` and `build.target-dir` are honored, and a caller's own
# `--target-dir` wins.
#
# Any extra arguments are forwarded, so callers pick the level:
#   scripts/hawk-check.sh -D warnings
set -euo pipefail

EXTERNAL_BOUNDARY_CRATES=(
  llmenv_config
  llmenv_git
  llmenv_paths
  llmenv_util
)

args=()
for crate in "${EXTERNAL_BOUNDARY_CRATES[@]}"; do
  args+=(--exclude-crate "${crate}")
done

has_target_dir=no
for arg in "$@"; do
  case "${arg}" in
  --target-dir | --target-dir=*) has_target_dir=yes ;;
  esac
done

if [[ "${has_target_dir}" == no ]]; then
  # `cargo metadata` already resolves CARGO_TARGET_DIR and build.target-dir
  # relative to the workspace root, which the pre-push hook may not be run from.
  # Fall back to the git root when jq is missing.
  if command -v jq >/dev/null 2>&1; then
    target_root="$(cargo metadata --no-deps --format-version 1 | jq -r .target_directory)"
  else
    target_root="${CARGO_TARGET_DIR:-$(git rev-parse --show-toplevel)/target}"
  fi
  args+=(--target-dir "${target_root}/hawk")
fi

exec cargo hawk check "${args[@]}" "$@"
