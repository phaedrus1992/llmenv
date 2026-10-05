#!/usr/bin/env bash
# check-binary-version.sh — fail when a built llmenv binary reports a version that differs from
# the workspace source (#2438), so a stale binary cannot pass the tests unnoticed.
#
# Usage: scripts/check-binary-version.sh [--binary PATH] [--expected VERSION]
#   --binary PATH       the binary to run (default: target/debug/llmenv)
#   --expected VERSION  the source version (default: the `llmenv` package in `cargo metadata`)
# Prints the version on success. Exits 1 on a mismatch, 2 on a usage or tool error.
set -euo pipefail

binary="target/debug/llmenv"
expected=""

usage() {
  sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
  --binary)
    [[ $# -ge 2 ]] || {
      echo "check-binary-version: --binary needs a path" >&2
      exit 2
    }
    binary="$2"
    shift 2
    ;;
  --expected)
    [[ $# -ge 2 ]] || {
      echo "check-binary-version: --expected needs a version" >&2
      exit 2
    }
    expected="$2"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "check-binary-version: unknown argument '$1'. Run with --help." >&2
    exit 2
    ;;
  esac
done

if [[ -z "$expected" ]]; then
  command -v jq >/dev/null || {
    echo "check-binary-version: jq is required" >&2
    exit 2
  }
  expected="$(cargo metadata --no-deps --format-version 1 |
    jq -r '.packages[] | select(.name == "llmenv") | .version')"
  [[ -n "$expected" ]] || {
    echo "check-binary-version: no llmenv package in cargo metadata" >&2
    exit 2
  }
fi

[[ -x "$binary" ]] || {
  echo "check-binary-version: $binary is not an executable. Build it first." >&2
  exit 2
}

# `llmenv --version` prints "llmenv <version> (<commit>)".
actual="$("$binary" --version | awk '{print $2}')"
if [[ "$actual" != "$expected" ]]; then
  echo "check-binary-version: $binary reports $actual, but the source version is $expected." >&2
  echo "Rebuild the binary, or fix the version in Cargo.toml." >&2
  exit 1
fi
echo "$actual"
