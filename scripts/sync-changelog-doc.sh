#!/usr/bin/env bash
# Derive website/docs/changelog.md from per-major-version CHANGELOG-*.md files.
#
# Run this whenever a CHANGELOG-*.md changes and commit both files together.
# The CI test in tests/docs_sync.rs fails if they drift.
set -euo pipefail

if [[ "${DRY_RUN:-}" == "true" ]]; then
  echo "Skipping (dry-run)."
  exit 0
fi

cd "$(dirname "$0")/.."

cat > website/docs/changelog.md << 'FRONTMATTER'
---
id: changelog
title: Changelog
slug: /changelog
sidebar_label: Changelog
---

{/* GENERATED FILE — do not edit by hand. Regenerate with `scripts/sync-changelog-doc.sh`. */}

FRONTMATTER

# Concatenate CHANGELOG-*.md newest-first, stripping reference-link
# footers, versioned next-header sentinel lines, and preamble text
# (everything before the first `## [` section header) from files 2+,
# so the combined output has only one preamble.
#
# Discovers CHANGELOG-N.md files dynamically — no hardcoded list.
first=true
while IFS= read -r -d '' f; do
  f="${f#./}"
  # Skip placeholder files with no actual changelog sections
  if ! grep -q '^## \[' "$f" 2>/dev/null; then
    continue
  fi

  v=$(echo "$f" | sed 's/CHANGELOG-\([0-9]*\)\.md/\1/')

  if [[ "$first" == "true" ]]; then
    first=false
    # First file: keep preamble, insert ## Version N.x after it
    V="$v" perl -0777 -pe '
      s/<!--\s*next-url\s*-->.*$//ms;
      s/^<!--\s*[\d.]+\s+next-header\s*-->\n//mg;
      s/\n+(?=## \[)/\n\n## Version $ENV{V}.x\n\n/ms;
      s/^\n+//;
      s/\n+$/\n/;
    ' "$f" >> website/docs/changelog.md
  else
    echo "" >> website/docs/changelog.md
    echo "## Version ${v}.x" >> website/docs/changelog.md
    echo "" >> website/docs/changelog.md
    # Subsequent files: strip preamble, footer + sentinels
    perl -0777 -pe '
      s/<!--\s*next-url\s*-->.*$//ms;
      s/^.*?\n(?=## \[)//s;
      s/^<!--\s*[\d.]+\s+next-header\s*-->\n//mg;
      s/^\n+//;
      s/\n+$/\n/;
    ' "$f" >> website/docs/changelog.md
  fi
done < <(find . -maxdepth 1 -name 'CHANGELOG-*.md' -print0 | sort -t- -k2 -n -r -z)

echo "Done. website/docs/changelog.md regenerated."
