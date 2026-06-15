#!/usr/bin/env bash
# Derive website/docs/changelog.md from the root CHANGELOG.md.
#
# Run this whenever CHANGELOG.md changes and commit both files together.
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

# Drop everything from <!-- next-url --> to EOF (the reference-link footer),
# strip all versioned next-header sentinel lines, then trim surrounding blank lines.
perl -0777 -pe '
  s/<!--\s*next-url\s*-->.*$//ms;  # drop URL block
  s/^<!--\s*[\d.]+\s+next-header\s*-->\n//mg; # strip sentinel lines
  s/^\n+//;                          # trim leading blank lines
  s/\n+$/\n/;                        # trim trailing blank lines (keep one \n)
' CHANGELOG.md >> website/docs/changelog.md

echo "Done. website/docs/changelog.md regenerated."
