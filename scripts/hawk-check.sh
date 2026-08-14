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

exec cargo hawk check "${args[@]}" "$@"
