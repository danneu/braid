#!/usr/bin/env python3
"""Guard public command entry points with boundary doc comments."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CMD_FN_RE = re.compile(r"^\s*pub(?:\(crate\))?\s+fn\s+(cmd_[A-Za-z0-9_]*)\b")
OUTER_DOC_RE = re.compile(r"^\s*///(?!/)")
ATTR_RE = re.compile(r"^\s*#\[")


def scan_rust_file(rel: str, lines: list[str]) -> list[str]:
    """Return missing-boundary-doc failures for public cmd_* functions."""
    failures: list[str] = []

    for index, line in enumerate(lines):
        match = CMD_FN_RE.match(line)
        if not match:
            continue

        name = match.group(1)
        before = index - 1
        while before >= 0:
            stripped = lines[before].strip()
            if stripped == "" or ATTR_RE.match(stripped):
                before -= 1
                continue
            break

        if before < 0 or not OUTER_DOC_RE.match(lines[before]):
            failures.append(f"{rel}:{index + 1}: {name} lacks a /// boundary doc comment")

    return failures


def _tracked_cli_rust_files():
    result = subprocess.run(
        ["git", "ls-files", "cli/src"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    for rel in sorted(result.stdout.splitlines()):
        if rel.endswith(".rs"):
            yield ROOT / rel, rel


def _read_lines(path: Path) -> list[str]:
    return path.read_text(encoding="utf-8").splitlines()


def _selftest() -> int:
    failures: list[str] = []

    def check(name: str, ok: bool) -> None:
        if not ok:
            failures.append(name)

    def rs(src: str) -> list[str]:
        return scan_rust_file("cli/src/sample.rs", src.splitlines())

    check(
        "undocumented pub fn cmd_x",
        any("cmd_x lacks" in failure for failure in rs("pub fn cmd_x() {}\n")),
    )
    check(
        "undocumented pub(crate) fn cmd_x",
        any("cmd_x lacks" in failure for failure in rs("pub(crate) fn cmd_x() {}\n")),
    )
    check(
        "block doc does not satisfy",
        any(
            "cmd_x lacks" in failure
            for failure in rs("/** Boundary reason. */\npub fn cmd_x() {}\n")
        ),
    )
    check(
        "documented pub fn cmd_x",
        rs("/// Boundary reason.\npub fn cmd_x() {}\n") == [],
    )
    check(
        "documented with blank line",
        rs("/// Boundary reason.\n\npub fn cmd_x() {}\n") == [],
    )
    check(
        "documented with attribute",
        rs("/// Boundary reason.\n#[inline]\npub fn cmd_x() {}\n") == [],
    )
    check(
        "documented pub(crate) fn cmd_x",
        rs("/// Boundary reason.\npub(crate) fn cmd_x() {}\n") == [],
    )

    if failures:
        print("cmd doc comments selftest FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("cmd doc comments selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    failures: list[str] = []
    for path, rel in _tracked_cli_rust_files():
        failures.extend(scan_rust_file(rel, _read_lines(path)))

    if failures:
        for failure in failures:
            print(failure, file=sys.stderr)
        return 1

    print("cmd doc comments check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
