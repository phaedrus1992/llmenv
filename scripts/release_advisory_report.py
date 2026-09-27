#!/usr/bin/env python3
"""Build a markdown advisory report for one release branch.

Usage: release_advisory_report.py <branch> <deny-outcome> <npm-audit-json>

<deny-outcome> is the `cargo deny check advisories` step's GitHub Actions
outcome: success, failure, cancelled, or skipped.

<npm-audit-json> is a path to `npm audit --package-lock-only --audit-level=moderate
--json` output, or "-" when the branch has no website/package-lock.json.

Exit 0: no findings. Nothing is printed.
Exit 1: findings exist. A markdown report is printed on stdout.
Exit 2: the input could not be understood (the npm audit file could not be
read or parsed, was not shaped as expected, or <deny-outcome> is not one of
the known values). An error naming the problem is printed on stderr.

Design: docs/design/issue-2165-release-branch-advisory-scan.md
"""

import argparse
import json
import sys
from pathlib import Path
from typing import Any

KNOWN_DENY_OUTCOMES = frozenset({"success", "failure", "cancelled", "skipped"})


def parse_npm_audit(path: str) -> list[dict[str, str]]:
    """Return one row per npm advisory. Raise ValueError naming the path on failure."""
    if path == "-":
        return []
    try:
        text = Path(path).read_text(encoding="utf-8")
        data = json.loads(text)
    except (OSError, json.JSONDecodeError) as err:
        raise ValueError(f"cannot read {path}: {err}") from err

    if not isinstance(data, dict):
        raise ValueError(
            f"{path}: expected a JSON object at the top level, got {type(data).__name__}"
        )
    if "error" in data:
        # npm audit exits non-zero and prints {"error": {...}} (not
        # {"vulnerabilities": {...}}) when the audit itself could not run --
        # a registry/auth/network failure, not a clean result. Treating this
        # as "no vulnerabilities" would auto-close a real open advisory the
        # next time the scan runs clean by coincidence of this same failure.
        raise ValueError(f"npm audit reported an error in {path}: {data['error']}")

    vulnerabilities = data.get("vulnerabilities", {})
    if not isinstance(vulnerabilities, dict):
        raise ValueError(
            f"{path}: 'vulnerabilities' must be an object, got {type(vulnerabilities).__name__}"
        )

    rows = []
    for name, info in vulnerabilities.items():
        if not isinstance(info, dict):
            raise ValueError(
                f"{path}: vulnerability entry {name!r} must be an object, got {type(info).__name__}"
            )
        rows.append(
            {
                "package": name,
                "severity": info.get("severity", "unknown"),
                "range": info.get("range", "unknown"),
            }
        )
    return rows


def escape_table_cell(value: Any) -> str:
    """Make a value safe as one markdown table cell: no `|`, no newline."""
    return str(value).replace("|", "\\|").replace("\n", " ")


def build_report(branch: str, deny_outcome: str, npm_rows: list[dict[str, str]]) -> str:
    """Return the markdown report body, or "" when there is nothing to report."""
    sections = []
    if deny_outcome == "failure":
        sections.append(
            "## Rust advisories (`cargo deny check advisories`)\n\n"
            "The check found an advisory on this branch. See the workflow run for details."
        )
    if npm_rows:
        rows = "\n".join(
            "| {package} | {severity} | {range} |".format(
                package=escape_table_cell(row["package"]),
                severity=escape_table_cell(row["severity"]),
                range=escape_table_cell(row["range"]),
            )
            for row in sorted(npm_rows, key=lambda r: r["package"])
        )
        sections.append(
            "## npm advisories (`website/`)\n\n"
            "| Package | Severity | Range |\n"
            "| --- | --- | --- |\n" + rows
        )
    if not sections:
        return ""
    return f"# Advisories on `{branch}`\n\n" + "\n\n".join(sections)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").split("\n\n")[0])
    parser.add_argument("branch")
    parser.add_argument("deny_outcome")
    parser.add_argument("npm_audit_json")
    args = parser.parse_args(argv)

    if args.deny_outcome not in KNOWN_DENY_OUTCOMES:
        print(
            f"unrecognized cargo-deny outcome {args.deny_outcome!r}, expected one of "
            f"{sorted(KNOWN_DENY_OUTCOMES)}",
            file=sys.stderr,
        )
        return 2

    try:
        npm_rows = parse_npm_audit(args.npm_audit_json)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2

    report = build_report(args.branch, args.deny_outcome, npm_rows)
    if not report:
        return 0
    print(report)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
