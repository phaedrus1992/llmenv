#!/bin/bash
set -euo pipefail

VERSION="0.3.0"

command -v npx >/dev/null 2>&1 || {
  echo "slop-scan: npx not found — install Node.js to use slop-scan" >&2
  exit 1
}

npx "slop-scan@${VERSION}" "$@" || {
  ec=$?
  echo "slop-scan: 'npx slop-scan@${VERSION}' failed (exit ${ec})" >&2
  echo "  Verify npm is reachable and slop-scan@${VERSION} is published." >&2
  exit "${ec}"
}
