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
#    Matches: ## [Unreleased] - ReleaseDate, [Unreleased]: https://..., etc.
#    Must be done first — step 4/5 seed new [Unreleased] sections that must
#    not be overwritten.
count = content.count("Unreleased")
if count == 0:
    print("roll-prerelease-changelog: no 'Unreleased' found in CHANGELOG.md — already rolled?", file=sys.stderr)
    sys.exit(1)

content = content.replace("Unreleased", version)

# 2. Replace "...HEAD" with "...v<version>" (the compare link from old
#    [Unreleased] URL).  Must happen after step 1 so the replacement
#    doesn't touch the newly-seeded [Unreleased] URL.
new_compare = f"...v{version}"
content = content.replace("...HEAD", new_compare, 1)

# 3. Replace "ReleaseDate" with the actual date.
content = content.replace("ReleaseDate", date, 1)

# 4. Seed a fresh [Unreleased] section below the <!-- 3.0 next-header --> marker.
next_header = "<!-- 3.0 next-header -->"
new_section = f"{next_header}\n\n## [Unreleased] - ReleaseDate"
content = content.replace(next_header, new_section, 1)

# 5. Seed a fresh [Unreleased] compare link below the <!-- next-url --> marker.
next_url = "<!-- next-url -->"
new_url = f"{next_url}\n[Unreleased]: {repo}/compare/v{version}...HEAD"
content = content.replace(next_url, new_url, 1)

with open(changelog_path, 'w') as f:
    f.write(content)

print(f"roll-prerelease-changelog: CHANGELOG.md rolled to {version}")
PYEOF