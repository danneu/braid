#!/usr/bin/env python3
"""Forbid durable artifacts from citing transient plans/wip files.

Plans in `plans/wip/` are working notes. They are renamed when promoted or may
never be committed at all, so citations from code, tests, modules, or docs rot
quickly. Durable artifacts must cite a stable `docs/` page or the promoted
`plans/impl/<date>-*.md` record instead.
"""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SEARCH_ROOTS = [
    ROOT / "cli",
    ROOT / "tests",
    ROOT / "modules",
    ROOT / "docs",
]
TRANSIENT_PLAN = re.compile(r"plans/wip/[^`\"')\s]+\.md")


def iter_files(root: Path):
    if not root.exists():
        return
    if root.is_file():
        yield root
        return
    skip_dirs = {"target", ".git", ".direnv", "__pycache__"}
    for path in root.rglob("*"):
        rel_parts = path.relative_to(root).parts
        if any(part in skip_dirs for part in rel_parts):
            continue
        if root == ROOT / "docs" and rel_parts[:1] == ("book",):
            continue
        if path.is_file():
            yield path


def lint_file(path: Path, display_root: Path) -> list[str]:
    """Return transient-plan citation failures for one text file."""
    failures: list[str] = []
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return failures

    try:
        rel = path.relative_to(display_root).as_posix()
    except ValueError:
        rel = path.name

    for line_no, line in enumerate(text.splitlines(), start=1):
        for match in TRANSIENT_PLAN.finditer(line):
            failures.append(
                f"{rel}:{line_no}: transient plans/wip file reference "
                f"`{match.group(0)}`; cite a `docs/` page or the promoted "
                f"`plans/impl/<date>-*.md` path instead"
            )
    return failures


def lint_roots(search_roots: list[Path], display_root: Path) -> list[str]:
    """Return transient-plan citation failures for all files under roots."""
    failures: list[str] = []
    for root in search_roots:
        for path in iter_files(root):
            failures.extend(lint_file(path, display_root))
    return failures


def _selftest() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        fixture = root / "fixture.txt"
        fixture.write_text(
            "\n".join(
                [
                    "bad: plans/wip/x.md",
                    "ok: bare plans/wip/ directory token",
                    "ok: plans/impl/2026-01-01-x.md",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        failures = lint_roots([fixture], root)

    offending = {re.search(r"`([^`]+)`", f).group(1) for f in failures}
    expected = {"plans/wip/x.md"}
    if offending != expected:
        print("plans refs selftest FAILED:", file=sys.stderr)
        print(f"  expected failing refs: {sorted(expected)}", file=sys.stderr)
        print(f"  actual failing refs:   {sorted(offending)}", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("plans refs selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    failures = lint_roots(SEARCH_ROOTS, ROOT)

    if failures:
        print("plans refs check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("plans refs check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
