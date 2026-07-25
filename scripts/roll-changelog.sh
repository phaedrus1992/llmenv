#!/usr/bin/env bash
set -euo pipefail

# Roll CHANGELOG-<major>.md's [Unreleased] section into a versioned section,
# where <major> is VERSION's major version component. Called from
# release.toml's pre-release-hook.
#
# Targets the file matching the release's own major version — NOT "whichever
# CHANGELOG-N.md happens to have an [Unreleased] section" (the old heuristic;
# see #1003). That broke as soon as two CHANGELOG-N.md files had open
# [Unreleased] sections at once — e.g. release/X.x accumulating patch-line
# work in CHANGELOG-X.md while main accumulates feature-line work in
# CHANGELOG-(X+1).md for a future major bump — because it always picked the
# higher-numbered file regardless of which one the release being cut actually
# belongs to, silently leaving the other file's real, shipped changes
# mislabeled "Unreleased" forever.
#
# Handles both stable (e.g. 3.3.0) and pre-release (e.g. 3.3.0-rc.1) versions
# identically — the major version is the first dot-separated component either
# way.
#
# Idempotent: if the version heading already exists, exits 0 without changes.
#
# Usage: roll-changelog.sh <version>
# Example: roll-changelog.sh 3.3.0

VERSION="${1:?usage: roll-changelog.sh <version>}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Major version is the first dot-separated component: "3.6.2" -> "3",
# "4.0.0-rc.1" -> "4".
MAJOR="${VERSION%%.*}"
CHANGELOG="$WORKSPACE_DIR/CHANGELOG-${MAJOR}.md"

if [[ ! -f "$CHANGELOG" ]]; then
  echo "roll-changelog: $CHANGELOG does not exist (version $VERSION -> major $MAJOR)" >&2
  exit 1
fi

if ! grep -q '## \[Unreleased\]' "$CHANGELOG"; then
  echo "roll-changelog: $CHANGELOG has no '## [Unreleased]' section to roll" >&2
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
