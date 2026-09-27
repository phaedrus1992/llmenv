"""Tests for scripts/release_advisory_report.py."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "release_advisory_report.py"

CLEAN_AUDIT = {"vulnerabilities": {}}

HIGH_FINDING_AUDIT = {
    "vulnerabilities": {
        "serialize-javascript": {
            "severity": "high",
            "range": "<6.0.2",
        }
    }
}


def run(branch: str, deny_outcome: str, audit: dict | None) -> tuple[int, str, str]:
    """Run the script. `audit=None` passes '-' (no lockfile on the branch)."""
    with tempfile.TemporaryDirectory() as tmp:
        if audit is None:
            audit_arg = "-"
        else:
            path = Path(tmp) / "audit.json"
            path.write_text(json.dumps(audit), encoding="utf-8")
            audit_arg = str(path)
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), branch, deny_outcome, audit_arg],
            capture_output=True,
            text=True,
            check=False,
        )
    return proc.returncode, proc.stdout, proc.stderr


class ReleaseAdvisoryReportTest(unittest.TestCase):
    def test_clean_audit_and_passing_deny_gives_no_report(self) -> None:
        code, out, err = run("release/3.x", "success", CLEAN_AUDIT)
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "")

    def test_no_lockfile_and_passing_deny_gives_no_report(self) -> None:
        code, out, err = run("release/3.x", "success", None)
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "")

    def test_npm_finding_lists_package_severity_and_range(self) -> None:
        code, out, err = run("release/3.x", "success", HIGH_FINDING_AUDIT)
        self.assertEqual(code, 1, err)
        self.assertIn("serialize-javascript", out)
        self.assertIn("high", out)
        self.assertIn("<6.0.2", out)

    def test_failed_deny_outcome_adds_a_rust_section(self) -> None:
        code, out, err = run("release/3.x", "failure", CLEAN_AUDIT)
        self.assertEqual(code, 1, err)
        self.assertIn("Rust", out)
        self.assertIn("cargo deny", out)

    def test_malformed_json_gives_an_error_naming_the_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "audit.json"
            path.write_text("not json", encoding="utf-8")
            proc = subprocess.run(
                [sys.executable, str(SCRIPT), "release/3.x", "success", str(path)],
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(proc.returncode, 2)
        self.assertIn(str(path), proc.stderr)

    def test_report_names_the_branch(self) -> None:
        code, out, err = run("release/4.x", "failure", CLEAN_AUDIT)
        self.assertEqual(code, 1, err)
        self.assertIn("release/4.x", out)


if __name__ == "__main__":
    unittest.main()
