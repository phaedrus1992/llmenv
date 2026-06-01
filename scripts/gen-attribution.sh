#!/usr/bin/env bash
# Regenerate the third-party attribution files from the locked dependency graph.
#
# Produces two copies from the same cargo-about run (see AGENTS.md):
#   - THIRD-PARTY-LICENSES.md            ships with the binary / source dist
#   - website/docs/third-party-licenses.md  browseable on the docs site
#
# Run this whenever Cargo.lock dependencies change (add/remove/bump) and commit
# the result in the same change. Requires: cargo install cargo-about --locked --features cli
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo-about >/dev/null 2>&1; then
	echo "error: cargo-about not found. Install with:" >&2
	echo "  cargo install cargo-about --locked --features cli" >&2
	exit 1
fi

echo "Generating THIRD-PARTY-LICENSES.md (binary/source distribution)..."
cargo about generate --all-features about.hbs -o THIRD-PARTY-LICENSES.md

echo "Generating website/docs/third-party-licenses.md (docs site)..."
cargo about generate --all-features about-web.hbs -o website/docs/third-party-licenses.md

echo "Done. Both attribution files regenerated from the current Cargo.lock."
