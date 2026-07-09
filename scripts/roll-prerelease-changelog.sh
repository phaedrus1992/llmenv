#!/usr/bin/env bash
set -euo pipefail

# Roll a CHANGELOG.md [Unreleased] section into a versioned pre-release section.
# cargo-release skips pre-release-replacements when the target version has a
# pre-release suffix (e.g. 3.0.0-rc.2), so this script does the same work
# manually.  Called from release.toml's pre-release-hook.
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

# 1. Replace ALL occurrences of "Unreleased" with the version string.
#    Must be done first — later steps seed new [Unreleased] sections that
#    must not be overwritten.
count = content.count("Unreleased")
if count == 0:
    print("roll-prerelease-changelog: no 'Unreleased' found — already rolled?", file=sys.stderr)
    sys.exit(1)

content = content.replace("Unreleased", version)

# 2. Replace the first occurrence of "...HEAD" with "...v<version>".
#    This updates the old [Unreleased] compare URL.
new_compare = f"...v{version}"
content = content.replace("...HEAD", new_compare, 1)

# 3. Replace the first occurrence of "ReleaseDate" with today's date.
content = content.replace("ReleaseDate", date, 1)

# 4. Seed a fresh [Unreleased] section below the next-header marker.
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

# 5. Seed a fresh [Unreleased] compare link below the next-url marker.
new_url = f"[Unreleased]: {repo}/compare/v{version}...HEAD"
content = content.replace("<!-- next-url -->", f"<!-- next-url -->\n{new_url}", 1)

with open(changelog_path, 'w') as f:
    f.write(content)

print(f"roll-prerelease-changelog: CHANGELOG.md rolled to {version}")
PYEOF