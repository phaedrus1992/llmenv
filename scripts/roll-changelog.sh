#!/usr/bin/env bash
set -euo pipefail

# Roll the appropriate CHANGELOG-N.md [Unreleased] section into a versioned
# section. Called from release.toml's pre-release-hook.
#
# Auto-discovers the active CHANGELOG by finding the highest-numbered
# CHANGELOG-N.md that contains an [Unreleased] section. Handles both stable
# (e.g. 3.3.0) and pre-release (e.g. 3.3.0-rc.1) versions identically.
#
# Idempotent: if the version heading already exists, exits 0 without changes.
#
# Usage: roll-changelog.sh <version>
# Example: roll-changelog.sh 3.3.0

VERSION="${1:?usage: roll-changelog.sh <version>}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Find the highest-numbered CHANGELOG-N.md that has an [Unreleased] section.
CHANGELOG=$(grep -l '## \[Unreleased\]' "$WORKSPACE_DIR"/CHANGELOG-*.md 2>/dev/null | sort -t- -k2 -n | tail -1)

if [[ -z "$CHANGELOG" ]]; then
  echo "roll-changelog: no CHANGELOG-*.md with [Unreleased] section found" >&2
  exit 1
fi

echo "roll-changelog: using $CHANGELOG" >&2

DATE="$(date +%Y-%m-%d)"
REPO="https://github.com/phaedrus1992/llmenv"

python3 - "$CHANGELOG" "$VERSION" "$DATE" "$REPO" << 'PYEOF'
import sys, re

changelog_path, version, date, repo = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

with open(changelog_path) as f:
    content = f.read()

# Idempotency: if a section heading for this version already exists, skip.
if re.search(rf'^## \[{re.escape(version)}\]', content, re.MULTILINE):
    print(f"roll-changelog: [{version}] section already exists, skipping", file=sys.stderr)
    sys.exit(0)

# Verify there's an [Unreleased] section to roll.
if "## [Unreleased]" not in content:
    print("roll-changelog: no '## [Unreleased]' heading found — already rolled?", file=sys.stderr)
    sys.exit(1)

# 1. Replace "[Unreleased] - ReleaseDate" with the versioned heading.
content = content.replace("## [Unreleased] - ReleaseDate", f"## [{version}] - {date}", 1)

# 2. Replace the [Unreleased] compare URL with the versioned one.
old_url = re.search(r'^\[Unreleased\]: (.+?)\.\.\.HEAD$', content, re.MULTILINE)
if not old_url:
    print("roll-changelog: no [Unreleased] compare URL found", file=sys.stderr)
    sys.exit(1)
new_url_line = f"[{version}]: {old_url.group(1)}...v{version}"
content = content.replace(old_url.group(0), new_url_line, 1)

# 3. Seed a fresh [Unreleased] section below the next-header marker.
new_section = r"\1\n\n## [Unreleased] - ReleaseDate"
content, n = re.subn(
    r'(<!-- \d+\.\d+ next-header -->)',
    new_section,
    content,
    count=1,
)
if n == 0:
    print("roll-changelog: no next-header marker found", file=sys.stderr)
    sys.exit(1)

# 4. Seed a fresh [Unreleased] compare link below the next-url marker.
new_link = f"[Unreleased]: {repo}/compare/v{version}...HEAD"
content = content.replace("<!-- next-url -->", f"<!-- next-url -->\n{new_link}", 1)

with open(changelog_path, 'w') as f:
    f.write(content)

print(f"roll-changelog: {changelog_path} rolled to {version}", file=sys.stderr)
PYEOF
