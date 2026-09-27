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


def run(branch: str, deny_outcome: str, audit) -> tuple[int, str, str]:
    """Run the script. `audit=None` passes '-' (no lockfile on the branch).

    `audit` is normally a dict (JSON-serialized as the audit file), but any
    JSON-serializable value is accepted so a test can exercise a malformed
    top-level shape (e.g. a list instead of an object).
    """
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

    def test_npm_audit_own_error_shape_is_not_read_as_clean(self) -> None:
        # npm audit exits non-zero and prints {"error": {...}} (no
        # "vulnerabilities" key) when the audit itself could not run --
        # e.g. a registry outage. This must not report clean, or the
        # workflow would auto-close a real open advisory the next time a
        # genuinely clean scan happens to coincide with this failure mode.
        audit_error = {
            "error": {"code": "ENOAUDIT", "summary": "audit endpoint returned an error"}
        }
        code, out, err = run("release/3.x", "success", audit_error)
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertIn("error", err)

    def test_non_dict_top_level_json_gives_an_error(self) -> None:
        code, out, err = run("release/3.x", "success", [1, 2, 3])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertIn("expected a JSON object", err)

    def test_non_dict_vulnerabilities_value_gives_an_error(self) -> None:
        code, out, err = run(
            "release/3.x", "success", {"vulnerabilities": ["not", "a", "dict"]}
        )
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertIn("'vulnerabilities' must be an object", err)

    def test_non_dict_vulnerability_entry_gives_an_error(self) -> None:
        code, out, err = run(
            "release/3.x", "success", {"vulnerabilities": {"foo": "not a dict"}}
        )
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertIn("foo", err)

    def test_unrecognized_deny_outcome_gives_an_error(self) -> None:
        code, out, err = run("release/3.x", "typo-outcome", CLEAN_AUDIT)
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertIn("typo-outcome", err)

    def test_missing_severity_and_range_default_to_unknown(self) -> None:
        audit = {"vulnerabilities": {"foo": {}}}
        code, out, err = run("release/3.x", "success", audit)
        self.assertEqual(code, 1, err)
        self.assertIn("foo", out)
        self.assertIn("unknown", out)

    def test_unicode_package_name_is_preserved(self) -> None:
        audit = {"vulnerabilities": {"pkg-éàü": {"severity": "low", "range": "*"}}}
        code, out, err = run("release/3.x", "success", audit)
        self.assertEqual(code, 1, err)
        self.assertIn("pkg-éàü", out)

    def test_pipe_and_newline_in_fields_are_escaped_in_table(self) -> None:
        audit = {
            "vulnerabilities": {
                "foo": {"severity": "high", "range": "1.0 | 2.0\ninjected"}
            }
        }
        code, out, err = run("release/3.x", "success", audit)
        self.assertEqual(code, 1, err)
        for line in out.splitlines():
            if line.startswith("|"):
                self.assertEqual(line.count("|") - line.count("\\|"), 4, line)


if __name__ == "__main__":
    unittest.main()
