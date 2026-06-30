---
name: cleave-scan
description: This skill should be used when the user asks to "run cleave", "scan with cleave", "cleave this project", "check for supply-chain issues", "run a security scan", or "find malware indicators". Runs cleave static analysis on a project and files GitHub issues for findings. Uses ICM memory to track the last-scanned git revision per project and uses `cleave diff` on subsequent runs instead of a full scan.
---

# Cleave Scan

Runs `cleave` (`~/git/reference/cleave`) static analysis on a project and creates GitHub issues for notable findings. On first scan: full `cleave analyze`. On repeat scans: `cleave diff` against the last-scanned revision, creating issues only for new or changed findings.

Cleave binary location: `~/git/reference/cleave` (built with `cargo build --release`). Check if `cleave` is on PATH first; fall back to `~/git/reference/cleave/target/release/cleave`.

## Step 1 — Update Rules

Before scanning, ensure traits are current:

```bash
cleave update-rules
```

## Step 2 — Resolve Project

Determine the target path (default: current directory). Derive a canonical project name:

```bash
PROJECT_PATH=$(git -C "${TARGET_PATH}" rev-parse --show-toplevel 2>/dev/null || realpath "${TARGET_PATH}")
PROJECT_NAME=$(basename "$PROJECT_PATH")
CURRENT_REV=$(git -C "$PROJECT_PATH" rev-parse HEAD 2>/dev/null || echo "")
```

If the path is not a git repo, `CURRENT_REV` stays empty — full scan always, no diff.

## Step 3 — Check ICM for Last Scan

```
mcp__icm__icm_memory_recall { "topic": "cleave:scan:<PROJECT_NAME>" }
```

Expected payload (if previously scanned):
```json
{
  "rev": "<git-sha>",
  "path": "<project-path>",
  "scanned_at": "<ISO8601>"
}
```

If recall returns nothing or an error → **full scan** (Step 4a).  
If recall returns a rev AND the current project is a git repo → **diff scan** (Step 4b).  
If recall returns a rev but the current project is not a git repo → **full scan** (Step 4a).

## Step 4a — Full Scan

Pass specific source directories to avoid scanning build artifacts (`target/`, `node_modules/`, etc.).
For a Rust project, pass `src/`, `crates/`, `.github/`, `scripts/`, `charts/`, `Dockerfile`,
`Cargo.toml`, `Cargo.lock`, and any domain-specific config files. Check that each path exists first.

```bash
REPORT=/tmp/cleave-scan-${PROJECT_NAME}.json

# Build target list from paths that actually exist
TARGETS=()
for p in "$PROJECT_PATH/src" "$PROJECT_PATH/crates" "$PROJECT_PATH/.github" \
          "$PROJECT_PATH/scripts" "$PROJECT_PATH/charts" "$PROJECT_PATH/Dockerfile" \
          "$PROJECT_PATH/Cargo.toml" "$PROJECT_PATH/Cargo.lock"; do
  [ -e "$p" ] && TARGETS+=("$p")
done

# -o flag is broken in this cleave version; redirect stdout instead
cleave --format json --no-update-check "${TARGETS[@]}" > "$REPORT" 2>/tmp/cleave-stderr.log
```

Parse findings (see Step 5). All findings with `crit >= 3` are candidates for issues.

## Step 4b — Diff Scan

Use `git worktree` to create a clean checkout of the baseline revision, run `cleave diff`, then remove the worktree.

```bash
BASELINE_DIR=/tmp/cleave-baseline-${PROJECT_NAME}
REPORT=/tmp/cleave-diff-${PROJECT_NAME}.json

# Clean up any stale worktree from a previous interrupted run
git -C "$PROJECT_PATH" worktree remove --force "$BASELINE_DIR" 2>/dev/null || true
rm -rf "$BASELINE_DIR"

git -C "$PROJECT_PATH" worktree add "$BASELINE_DIR" "$LAST_REV"
# -o flag is broken in this cleave version; redirect stdout instead
cleave --format json --no-update-check diff "$BASELINE_DIR" "$PROJECT_PATH" > "$REPORT" 2>/tmp/cleave-stderr.log
git -C "$PROJECT_PATH" worktree remove --force "$BASELINE_DIR"
rm -rf "$BASELINE_DIR"
```

Parse findings (see Step 4). Only findings in `.diff.findings.added[]` with `crit >= 3` are candidates. Changed findings (`.diff.findings.changed[]`) are candidates if the new criticality is >= 3.

## Step 5 — Parse Findings

**Important:** `--format json` outputs one JSON object per analyzed file, concatenated without
newlines (effectively a stream). Use `jq -s` (slurp) to collect all objects and query across them.
Findings live in `.files[].traits[]`, not `.findings[]`.

For full scans:

```bash
# Count by criticality
jq -s '[.[].files[].traits[]? | select(.crit >= 3)] | group_by(.crit) | map({crit: .[0].crit, count: length})' "$REPORT"

# Extract actionable findings with file context
jq -s '[.[].files[] | {file: .path, traits: [.traits[]? | select(.crit >= 3)]} | select(.traits | length > 0)]' "$REPORT"
```

For diff scans (the diff shape matches the schema reference — `.diff.findings.added[]` etc.):
```bash
jq -s '[.[].diff.findings.added[]? | select(.crit >= 3)],
       [.[].diff.findings.changed[]?.new | select(.crit >= 3)]' "$REPORT"
```

Criticality reference:
- `3` = Notable — defines program purpose, flagged in diffs
- `4` = Suspicious — unusual or evasive, investigate
- `5` = Hostile — almost certainly malicious

## Step 6 — Triage: Verify Before Filing

**Do not create an issue until you have verified the finding is a genuine concern.**

For each crit ≥ 4 finding (and any crit 3 from a diff):

1. Read the flagged file at the evidence location.
2. Determine whether the behavior is expected given the project's purpose.
3. Classify as one of:
   - **Confirmed** — the code does something unexpectedly hostile; file an issue.
   - **Uncertain** — the context is ambiguous; file an issue with the ambiguity noted.
   - **False positive** — the behavior is intentional and benign; skip, mention in summary only.

Common false positive patterns to recognize and skip:
- `cargo-path-rat-topic` triggered by workspace path deps (`path = "../..."`) in a Rust monorepo — expected.
- SSH/authorized_keys chown/chmod in a bastion or deployment operator — expected infra management.
- `exec`/`process create` calls in a CLI or operator codebase that intentionally spawns subprocesses.
- Helm/Kubernetes RBAC verbs (`create`, `delete`, `patch`) in chart templates — expected.

If uncertain, read surrounding context (function, module, test) before deciding. A high confidence
score (≥ 0.9) on a crit 5 finding warrants filing even if context is mostly benign.

## Step 7 — Deduplicate Against Existing Issues

Before creating any issue, search for existing issues with the finding ID to avoid duplicates:

```bash
gh issue list --search "cleave: <finding-id>" --state all --json number,title --limit 5
```

If an open issue already exists for a finding, skip it. If a closed issue exists, reopen it with a comment noting the re-detection and current rev.

## Step 8 — Create GitHub Issues

Only create an issue for findings classified **Confirmed** or **Uncertain** in Step 6.
False positives are skipped and noted in the Step 9 summary only.

For each verified finding without an existing open issue:

```bash
gh issue create \
  --title "cleave: <finding.id>" \
  --label "cleave" \
  --body "$(cat <<'EOF'
## Cleave Finding

**ID:** `<finding.id>`  
**Kind:** <finding.kind>  
**Criticality:** <crit-name> (<finding.crit>/5)  
**Confidence:** <finding.conf>

<finding.desc>

---

**Scanned rev:** `<CURRENT_REV>`  
**Path:** `<PROJECT_PATH>`  
<if mbc>**MBC:** `<finding.mbc>`<endif>  
<if attack>**ATT&CK:** `<finding.attack>`<endif>

<evidence section if present>
EOF
)"
```

Criticality names for the body: 3=Notable, 4=Suspicious, 5=Hostile.

**Evidence section** (include when `finding.evidence` is non-empty):
```
### Evidence
<for each evidence item: file path, offset/line, matched string or description>
```

Create issues one at a time. After each, confirm the issue number and URL before proceeding to the next. If `gh issue create` fails (e.g., no GitHub remote, not authenticated), report the findings in the conversation instead.

**Labels:** Create the `cleave` label if missing:
```bash
gh label create cleave --color "D93F0B" --description "Cleave static analysis finding" 2>/dev/null || true
```

## Step 9 — Update ICM Memory

After all issues are created (or attempted), store the current scan state:

```
mcp__icm__icm_memory_store {
  "topic": "cleave:scan:<PROJECT_NAME>",
  "content": "{\"rev\": \"<CURRENT_REV>\", \"path\": \"<PROJECT_PATH>\", \"scanned_at\": \"<ISO8601-UTC>\"}"
}
```

If no git rev exists (non-git directory), skip this step — the next run will always be a full scan.

## Step 10 — Report Summary

After completing, summarize:
- Scan type (full or diff vs `<short-rev>`)
- Total findings parsed, count by criticality
- Triage breakdown: N confirmed, N uncertain, N false positives (with brief FP reason for each)
- Issues created (with numbers/URLs)
- Issues skipped (already exist)

## Error Handling

- **cleave not found:** Try `~/git/reference/cleave/target/release/cleave`. If missing, run `cargo build --release` in `~/git/reference/cleave` first.
- **Git worktree failure:** If the old rev no longer exists (e.g., force-pushed history), fall back to a full scan and note it in the summary.
- **No GitHub remote:** Skip issue creation, print all findings to the conversation.
- **JSON parse errors:** Fall back to `--format tiny` (also a top-level flag: `cleave --format tiny --no-update-check PATHS... > report.txt`) and parse findings line-by-line for manual review.
- **`-o` flag:** Broken in v2.1.1 — always redirect stdout (`> "$REPORT"`) instead of using `-o`.
- **Scanning too slow / huge output:** Do NOT pass the repo root — pass specific source dirs to avoid build artifacts (`target/`, `node_modules/`, etc.) which can be hundreds of MB and contain false positives.

## Additional Resources

- **`references/json-schema.md`** — Cleave JSON output schema reference (Finding, Criticality, diff shape)
