# Cleave JSON Output Reference

Quick reference for parsing cleave JSON output. Full docs: `~/git/reference/cleave/docs/JSON.md`.

## Finding Fields

| Field         | Type        | Notes                                                                  |
|---------------|-------------|------------------------------------------------------------------------|
| `id`          | string      | e.g. `objectives/evasion/process::injection`                           |
| `kind`        | string      | `capability` (default, omitted), `structural`, `indicator`, `weakness` |
| `desc`        | string      | Human-readable description                                             |
| `conf`        | float [0,1] | 0.5 = heuristic, 1.0 = definitive                                      |
| `crit`        | int 0-5     | See criticality table below                                            |
| `mbc`         | string      | MBC code, e.g. `C0002` (omitted if unset)                              |
| `attack`      | string      | ATT&CK technique, e.g. `T1055` (omitted if unset)                      |
| `trait_refs`  | string[]    | Trait IDs that contributed                                             |
| `evidence`    | Evidence[]  | Supporting evidence items                                              |
| `match_count` | int         | Total matches when evidence is truncated                               |

## Criticality

| Value | Name       | Weight | Action                         |
|-------|------------|--------|--------------------------------|
| 0     | Filtered   | 0      | Skip (hidden from humans)      |
| 1     | Component  | 0      | Skip (building block only)     |
| 2     | Baseline   | 0      | Skip (universal noise)         |
| 3     | Notable    | 1      | File issue in diff mode        |
| 4     | Suspicious | 40     | File issue always              |
| 5     | Hostile    | 120    | File issue always, priority    |

## Evidence Fields

| Field    | Type   | Notes                                  |
|----------|--------|----------------------------------------|
| `file`   | string | Source file path                       |
| `offset` | string | Hex offset or line number              |
| `value`  | string | Matched string or description          |
| `rule`   | string | Rule/trait that matched                |

## Top-level JSON Shape (full scan)

```json
{
  "version": "3",
  "analysis_timestamp": "<RFC3339>",
  "target": { "path": ..., "type": ..., "sha256": ... },
  "findings": [...],          // top-level findings
  "traits": [...],
  "files": [                  // per-file for directory scans
    {
      "target": { "path": ... },
      "findings": [...],
      ...
    }
  ],
  "summary": { ... },
  "metadata": { ... }
}
```

## Diff Output Shape

When running `cleave diff`, the top-level `diff` field is populated:

```json
{
  ...,                        // same as full scan
  "diff": {
    "findings": {
      "added": [...],         // new findings in NEW version
      "removed": [...],       // findings present in OLD but not NEW
      "changed": [            // findings present in both, criticality changed
        { "old": {...}, "new": {...} }
      ]
    },
    "traits": { "added": [...], "removed": [...] },
    "symbols": { "added": [...], "removed": [...] },
    "strings": { "added": [...], "removed": [...] },
    "metrics": { ... },
    "kv": { ... }
  }
}
```

## Useful jq Patterns

```bash
# All high-priority findings from a full scan
jq '[.findings[], (.files[]?.findings[]? // empty)] | map(select(.crit >= 4))' report.json

# New suspicious findings from a diff
jq '[.diff.findings.added[] | select(.crit >= 3)]' diff.json

# Changed findings where new crit is worse
jq '[.diff.findings.changed[] | select(.new.crit >= 3)]' diff.json

# Compact summary: id + crit + desc
jq '.findings[] | select(.crit >= 3) | "\(.crit) \(.id): \(.desc // "")"' report.json

# Count by criticality
jq '[.findings[] | select(.crit >= 3)] | group_by(.crit) | map({(.[0].crit|tostring): length}) | add' report.json
```
