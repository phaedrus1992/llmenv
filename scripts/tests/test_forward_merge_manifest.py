"""Tests for scripts/forward_merge_manifest.py."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "forward_merge_manifest.py"

BASE = """
[package]
name = "llmenv"
version = "3.11.1"

[dependencies]
llmenv-paths = { path = "crates/llmenv-paths", version = "3.11.1" }
clap = { version = "=4.6.6", features = ["derive"] }
clap_complete = "=4.6.9"
"""

SOURCE = BASE.replace("=4.6.6", "=4.6.7").replace("=4.6.9", "=4.6.11")

TARGET = """
[package]
name = "llmenv"
version = "4.0.0-alpha.1"

[dependencies]
llmenv-paths = { path = "crates/llmenv-paths", version = "4.0.0-alpha.1" }
clap = { version = "=4.6.7", features = ["derive"] }
clap_complete = "=4.6.11"
"""


def run(base: str, source: str, target: str) -> tuple[int, str]:
    """Run the script on three manifests. Return the exit code and stderr."""
    with tempfile.TemporaryDirectory() as tmp:
        paths = []
        for name, text in (("base", base), ("source", source), ("target", target)):
            path = Path(tmp) / f"{name}.toml"
            path.write_text(text, encoding="utf-8")
            paths.append(str(path))
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), *paths],
            capture_output=True,
            text=True,
            check=False,
        )
    return proc.returncode, proc.stderr


class ForwardMergeManifestTest(unittest.TestCase):
    def test_same_pin_bumped_on_both_branches_is_accepted(self) -> None:
        code, err = run(BASE, SOURCE, TARGET)
        self.assertEqual(code, 0, err)

    def test_source_newer_than_target_is_blocked(self) -> None:
        source = SOURCE.replace("=4.6.7", "=4.6.8")
        code, err = run(BASE, source, TARGET)
        self.assertEqual(code, 1)
        self.assertIn("clap", err)

    def test_added_dependency_is_blocked(self) -> None:
        code, err = run(
            BASE, SOURCE + 'serde = "=1.0.0"\n', TARGET + 'serde = "=1.0.0"\n'
        )
        self.assertEqual(code, 1)
        self.assertIn("added by source", err)

    def test_removed_dependency_is_blocked(self) -> None:
        code, err = run(BASE, SOURCE.replace('clap_complete = "=4.6.11"\n', ""), TARGET)
        self.assertEqual(code, 1)
        self.assertIn("removed by source", err)

    def test_changed_features_is_blocked(self) -> None:
        source = SOURCE.replace('["derive"]', '["derive", "env"]')
        code, err = run(BASE, source, TARGET)
        self.assertEqual(code, 1)
        self.assertIn("fields other than version", err)

    def test_string_and_table_forms_compare_equal(self) -> None:
        target = TARGET.replace(
            'clap_complete = "=4.6.11"', 'clap_complete = { version = "=4.6.11" }'
        )
        code, err = run(BASE, SOURCE, target)
        self.assertEqual(code, 0, err)

    def test_package_version_difference_is_ignored(self) -> None:
        base = BASE + '\n[workspace.package]\nversion = "3.11.1"\n'
        source = SOURCE + '\n[workspace.package]\nversion = "3.11.2"\n'
        target = TARGET + '\n[workspace.package]\nversion = "4.0.0-alpha.1"\n'
        code, err = run(base, source, target)
        self.assertEqual(code, 0, err)

    def test_other_key_changed_by_source_is_blocked(self) -> None:
        source = SOURCE.replace('name = "llmenv"', 'name = "renamed"')
        code, err = run(BASE, source, TARGET)
        self.assertEqual(code, 1)
        self.assertIn("package.name", err)

    def test_unparseable_version_is_blocked(self) -> None:
        source = SOURCE.replace("=4.6.7", "=4.6.x")
        code, err = run(BASE, source, TARGET)
        self.assertEqual(code, 1)
        self.assertIn("cannot compare", err)

    def test_operator_difference_is_blocked(self) -> None:
        source = SOURCE.replace('"=4.6.11"', '"^4.6.10"')
        code, err = run(BASE, source, TARGET)
        self.assertEqual(code, 1)
        self.assertIn("operator", err)

    def test_dependency_missing_on_target_is_blocked(self) -> None:
        code, err = run(BASE, SOURCE, TARGET.replace('clap_complete = "=4.6.11"\n', ""))
        self.assertEqual(code, 1)
        self.assertIn("missing on target", err)

    def test_target_specific_dependency_table_is_compared(self) -> None:
        table = "\n[target.'cfg(unix)'.dependencies]\nlibc = \"=0.2.{}\"\n"
        code, err = run(
            BASE + table.format(1), SOURCE + table.format(3), TARGET + table.format(2)
        )
        self.assertEqual(code, 1)
        self.assertIn("libc", err)

    def test_target_hoisted_dependency_is_resolved(self) -> None:
        base = BASE + 'rustix = { version = "=1.1.4", features = ["fs"] }\n'
        source = SOURCE + 'rustix = { version = "=1.1.5", features = ["fs"] }\n'
        target = (
            TARGET
            + "rustix = { workspace = true }\n"
            + '[workspace.dependencies]\nrustix = { version = "=1.1.5", features = ["fs"] }\n'
        )
        code, err = run(base, source, target)
        self.assertEqual(code, 0, err)

    def test_hoisted_dependency_older_than_source_is_blocked(self) -> None:
        base = BASE + 'rustix = "=1.1.4"\n'
        source = SOURCE + 'rustix = "=1.1.6"\n'
        target = (
            TARGET
            + 'rustix = { workspace = true }\n[workspace.dependencies]\nrustix = "=1.1.5"\n'
        )
        code, err = run(base, source, target)
        self.assertEqual(code, 1)
        self.assertIn("rustix", err)

    def test_invalid_toml_exits_two(self) -> None:
        code, err = run(BASE, "not = [valid", TARGET)
        self.assertEqual(code, 2)
        self.assertIn("cannot read", err)


if __name__ == "__main__":
    unittest.main()
