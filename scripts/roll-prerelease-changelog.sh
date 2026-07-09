#!/usr/bin/env bash
set -euo pipefail

# Roll a CHANGELOG.md [Unreleased] section into a versioned pre-release section.
# cargo-release skips pre-release-replacements when the target version has a
# pre-release suffix (e.g. 3.0.0-rc.2), so this script does the same work
# manually.  Called from release.toml's pre-release-hook.
#
# Idempotent: if the version heading already exists, exits 0 without changes.
# This handles cargo-release invoking the hook once per workspace crate.
#
# Known limitation: the hook runs even during cargo-release dry-run, so
# CHANGELOG.md is modified even in preview mode.  git checkout CHANGELOG.md
# restores it.  cargo-release does not expose dry-run status to hooks.
#
# Usage: roll-prerelease-changelog.sh <version>
# Example: roll-prerelease-changelog.sh 3.0.0-rc.2

VERSION="${1:?usage: roll-prerelease-changelog.sh <version>}"

# Only act on pre-release versions (anything with a hyphen: -rc.1, -beta.1, -alpha.1).
if [[ "$VERSION" != *"-"* ]]; then
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CHANGELOG="${SCRIPT_DIR}/../CHANGELOG.md"
DATE="$(date +%Y-%m-%d)"
REPO="https://github.com/phaedrus1992/llmenv"

if [[ ! -f "$CHANGELOG" ]]; then
  echo "roll-prerelease-changelog: $CHANGELOG not found" >&2
  exit 1
fi

python3 - "$CHANGELOG" "$VERSION" "$DATE" "$REPO" << 'PYEOF'
import sys, re

changelog_path, version, date, repo = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

with open(changelog_path) as f:
    content = f.read()

# Idempotency: if a section heading for this version already exists, skip.
if re.search(rf'^## \[{re.escape(version)}\]', content, re.MULTILINE):
    print(f"roll-prerelease-changelog: [{version}] section already exists, skipping", file=sys.stderr)
    sys.exit(0)

# Verify there's an [Unreleased] section to roll.
if "## [Unreleased]" not in content:
    print("roll-prerelease-changelog: no '## [Unreleased]' heading found — already rolled?", file=sys.stderr)
    sys.exit(1)

# 1. Replace "## [Unreleased] - ReleaseDate" with the versioned heading.
#    Only the section heading, not the URL reference at the bottom.
content = content.replace("## [Unreleased] - ReleaseDate", f"## [{version}] - {date}", 1)

# 2. Replace the [Unreleased] compare URL with the versioned one.
#    [Unreleased]: https://.../compare/vOLD...HEAD
#    → [<version>]: https://.../compare/vOLD...v<version>
old_url = re.search(r'^\[Unreleased\]: (.+?)\.\.\.HEAD$', content, re.MULTILINE)
if not old_url:
    print("roll-prerelease-changelog: no [Unreleased] compare URL found", file=sys.stderr)
    sys.exit(1)
new_url_line = f"[{version}]: {old_url.group(1)}...v{version}"
content = content.replace(old_url.group(0), new_url_line, 1)

# 3. Seed a fresh [Unreleased] section below the next-header marker.
#    The marker is branch-specific (e.g. <!-- 2.2 next-header --> on
#    release/2.x, <!-- 3.0 next-header --> on main).
new_section = r"\1\n\n## [Unreleased] - ReleaseDate"
content, n = re.subn(
    r'(<!-- \d+\.\d+ next-header -->)',
    new_section,
    content,
    count=1,
)
if n == 0:
    print("roll-prerelease-changelog: no next-header marker found", file=sys.stderr)
    sys.exit(1)

# 4. Seed a fresh [Unreleased] compare link below the next-url marker.
new_link = f"[Unreleased]: {repo}/compare/v{version}...HEAD"
content = content.replace("<!-- next-url -->", f"<!-- next-url -->\n{new_link}", 1)

with open(changelog_path, 'w') as f:
    f.write(content)

print(f"roll-prerelease-changelog: CHANGELOG.md rolled to {version}", file=sys.stderr)
PYEOF