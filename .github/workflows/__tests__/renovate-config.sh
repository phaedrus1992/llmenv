#!/usr/bin/env bash
# Checks .github/renovate.json5 with a pinned Renovate config validator.
#
# An invalid option stops Renovate before it opens any PR, and it does so
# without a CI failure (#3114). This test fails the build instead. Two control
# runs guard it: an empty config must pass, and a known-bad option must fail
# with the validator's own message.
#
# Usage: bash .github/workflows/__tests__/renovate-config.sh
set -euo pipefail

# Exact version: a floating range would let the validator change under CI.
# Chosen only after the 24-hour publish delay (see #3114).
readonly RENOVATE_VERSION="44.195.1"
# npm resolves the dependency tree on every run. The cutoff keeps it from
# choosing a package published after the pinned release. Move it with the pin.
readonly NPM_CUTOFF="2026-10-30T00:00:00Z"
readonly CONFIG=".github/renovate.json5"
readonly FIXTURE_DIR=".github/workflows/__tests__/fixtures"
readonly EMPTY_CONTROL="${FIXTURE_DIR}/valid-empty.json5"
readonly BAD_CONTROL="${FIXTURE_DIR}/bad-post-update-option.json5"

# Install scripts are off, so no postinstall code runs. The validator then uses
# its JavaScript regex fallback. Its notice is printed below, not hidden.
validator() {
  NPM_CONFIG_IGNORE_SCRIPTS=true NPM_CONFIG_BEFORE="$NPM_CUTOFF" \
    npx --yes --package "renovate@${RENOVATE_VERSION}" renovate-config-validator "$@"
}

if ! control_out="$(validator "$EMPTY_CONTROL" 2>&1)"; then
  printf '%s\n' "$control_out" >&2
  echo "FAIL: could not run renovate@${RENOVATE_VERSION} on ${EMPTY_CONTROL}." >&2
  echo "Fix: read the output above. It is an install or run failure, not a config failure." >&2
  exit 1
fi
if [[ "$control_out" == *"RE2 not usable"* ]]; then
  echo "NOTE: regex checks use the JavaScript RegExp fallback (install scripts are off)." >&2
fi

if ! config_out="$(validator "$CONFIG" 2>&1)"; then
  printf '%s\n' "$config_out" >&2
  echo "FAIL: ${CONFIG} is invalid for renovate@${RENOVATE_VERSION}." >&2
  echo "Fix: remove the option the validator names above." >&2
  exit 1
fi

if bad_out="$(validator "$BAD_CONTROL" 2>&1)"; then
  echo "FAIL: the validator accepted ${BAD_CONTROL}, which holds a known-invalid option." >&2
  echo "Fix: check that renovate@${RENOVATE_VERSION} still validates postUpdateOptions." >&2
  exit 1
fi
if [[ "$bad_out" != *"Invalid postUpdateOptions"* || "$bad_out" != *cargoLockPackages* ]]; then
  printf '%s\n' "$bad_out" >&2
  echo "FAIL: the validator rejected ${BAD_CONTROL}, but not with the expected message." >&2
  echo "Fix: read the output above and update the expected message in this script." >&2
  exit 1
fi

echo "OK: ${CONFIG} validates, and the validator rejects the known-bad option."
