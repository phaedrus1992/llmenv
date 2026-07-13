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

  # Insert blank line separator between file groups
  if [[ "$first" == "false" ]]; then
    echo "" >> website/docs/changelog.md
  fi

  if [[ "$first" == "true" ]]; then
    # First file: keep preamble, strip footer + sentinels
    perl -0777 -pe '
      s/<!--\s*next-url\s*-->.*$//ms;                 # drop URL block
      s/^<!--\s*[\d.]+\s+next-header\s*-->\n//mg;     # strip sentinel lines
      s/^\n+//;                                        # trim leading blank lines
      s/\n+$/\n/;                                      # trim trailing blank lines
    ' "$f" >> website/docs/changelog.md
    first=false
  else
    # Subsequent files: strip preamble (everything before first ## [) too
    perl -0777 -pe '
      s/<!--\s*next-url\s*-->.*$//ms;                 # drop URL block
      s/^.*?\n(?=## \[)//s;                            # strip preamble up to first section
      s/^<!--\s*[\d.]+\s+next-header\s*-->\n//mg;     # strip sentinel lines
      s/^\n+//;                                        # trim leading blank lines
      s/\n+$/\n/;                                      # trim trailing blank lines
    ' "$f" >> website/docs/changelog.md
  fi
done < <(find . -maxdepth 1 -name 'CHANGELOG-*.md' -print0 | sort -t- -k2 -n -r -z)

echo "Done. website/docs/changelog.md regenerated."
