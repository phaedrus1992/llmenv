#!/usr/bin/env python3
"""Decide whether a forward-merge can keep the target's Cargo manifest.

Usage: forward_merge_manifest.py <base.toml> <source.toml> <target.toml>

Exit 0: keeping the target's file loses nothing the source changed.
Exit 1: the source made a real change. Print one line per difference on stderr.
Exit 2: a file could not be read or parsed. The caller treats this as exit 1.

Design: docs/design/issue-2166-forward-merge-drift.md
"""

import argparse
import re
import sys
import tomllib
from pathlib import Path
from typing import Any

DEP_KINDS = ("dependencies", "dev-dependencies", "build-dependencies")

# Shape only. The numbers are parsed in code, and the operator is compared in code.
VERSION_SHAPE = re.compile(r"(=|\^|~|>=)?([0-9]{1,9})\.([0-9]{1,9})\.([0-9]{1,9})")


def dep_tables(doc: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Return every dependency table in a manifest, keyed by its TOML table name."""
    tables: dict[str, dict[str, Any]] = {}
    for kind in DEP_KINDS:
        if kind in doc:
            tables[kind] = doc[kind]
    if "dependencies" in doc.get("workspace", {}):
        tables["workspace.dependencies"] = doc["workspace"]["dependencies"]
    for cfg, target in doc.get("target", {}).items():
        for kind in DEP_KINDS:
            if kind in target:
                tables[f"target.{cfg}.{kind}"] = target[kind]
    return tables


def normalize_dep(spec: Any) -> dict[str, Any]:
    """Return a dependency as a table. A path dependency loses its `version` field."""
    dep = {"version": spec} if isinstance(spec, str) else dict(spec)
    if "path" in dep:
        dep.pop("version", None)
    return dep


def strip_for_comparison(doc: dict[str, Any]) -> dict[str, Any]:
    """Return the manifest without dependency tables and without the package versions."""
    rest = {k: v for k, v in doc.items() if k not in DEP_KINDS}
    rest["package"] = {
        k: v for k, v in doc.get("package", {}).items() if k != "version"
    }
    workspace = dict(doc.get("workspace", {}))
    workspace.pop("dependencies", None)
    package = {k: v for k, v in workspace.get("package", {}).items() if k != "version"}
    if package:
        workspace["package"] = package
    else:
        workspace.pop("package", None)
    if workspace:
        rest["workspace"] = workspace
    else:
        rest.pop("workspace", None)
    if "target" in rest:
        rest["target"] = {
            cfg: {k: v for k, v in body.items() if k not in DEP_KINDS}
            for cfg, body in rest["target"].items()
        }
    return rest


def flatten(value: Any, prefix: str = "") -> dict[str, Any]:
    """Return a nested table as a flat mapping from dotted key path to leaf value."""
    if not isinstance(value, dict):
        return {prefix: value}
    flat: dict[str, Any] = {}
    for key, child in value.items():
        flat.update(flatten(child, f"{prefix}.{key}" if prefix else key))
    return flat


def parse_version(text: str) -> tuple[str, tuple[int, int, int]] | None:
    """Return (operator, (major, minor, patch)), or None when the shape does not match."""
    match = VERSION_SHAPE.fullmatch(text)
    if match is None:
        return None
    op, major, minor, patch = match.groups()
    return op or "", (int(major), int(minor), int(patch))


def version_problem(where: str, source: str, target: str) -> str | None:
    """Return why a differing version pair blocks the merge, or None when it is safe."""
    src, tgt = parse_version(source), parse_version(target)
    if src is None or tgt is None:
        return f"{where}: cannot compare versions {source!r} and {target!r}"
    if src[0] != tgt[0]:
        return f"{where}: version operator differs ({source!r} on source, {target!r} on target)"
    if tgt[1] < src[1]:
        return f"{where}: source version {source!r} is newer than target {target!r}"
    return None


def dep_problem(
    where: str, source: dict[str, Any], target: dict[str, Any] | None
) -> str | None:
    """Return why one changed dependency blocks the merge, or None when the target wins."""
    if target is None:
        return f"{where}: changed on source but missing on target"
    other_source = {k: v for k, v in source.items() if k != "version"}
    other_target = {k: v for k, v in target.items() if k != "version"}
    if other_source != other_target:
        return f"{where}: fields other than version differ between source and target"
    src_ver, tgt_ver = source.get("version"), target.get("version")
    if src_ver == tgt_ver:
        return None
    if not isinstance(src_ver, str) or not isinstance(tgt_ver, str):
        return f"{where}: version present on only one of source and target"
    return version_problem(where, src_ver, tgt_ver)


def table_problems(
    name: str, base: dict[str, Any], source: dict[str, Any], target: dict[str, Any]
) -> list[str]:
    """Return the blocking differences in one dependency table."""
    problems = []
    for dep in sorted(set(base) | set(source)):
        where = f"[{name}] {dep}"
        if dep not in source:
            problems.append(f"{where}: removed by source")
        elif dep not in base:
            problems.append(f"{where}: added by source")
        else:
            src, old = normalize_dep(source[dep]), normalize_dep(base[dep])
            if src != old:
                tgt = normalize_dep(target[dep]) if dep in target else None
                problem = dep_problem(where, src, tgt)
                if problem:
                    problems.append(problem)
    return problems


def check(
    base: dict[str, Any], source: dict[str, Any], target: dict[str, Any]
) -> list[str]:
    """Return every reason the target's manifest cannot stand in for the source's."""
    problems = []
    base_tables, source_tables, target_tables = map(dep_tables, (base, source, target))
    for name in sorted(set(base_tables) | set(source_tables)):
        problems += table_problems(
            name,
            base_tables.get(name, {}),
            source_tables.get(name, {}),
            target_tables.get(name, {}),
        )
    old, new = (
        flatten(strip_for_comparison(base)),
        flatten(strip_for_comparison(source)),
    )
    for key in sorted(set(old) | set(new)):
        if old.get(key) != new.get(key):
            problems.append(f"{key}: changed by source outside the dependency tables")
    return problems


def load(path: str) -> dict[str, Any]:
    """Parse a TOML file. Raise ValueError with the path on any failure."""
    try:
        return tomllib.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as err:
        raise ValueError(f"cannot read {path}: {err}") from err


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("base")
    parser.add_argument("source")
    parser.add_argument("target")
    args = parser.parse_args(argv)
    try:
        base, source, target = load(args.base), load(args.source), load(args.target)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2
    problems = check(base, source, target)
    for line in problems:
        print(line, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
