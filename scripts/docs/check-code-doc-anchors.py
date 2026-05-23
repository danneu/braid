#!/usr/bin/env python3
"""Validate source-tree citations to principles.md anchors."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRINCIPLES = ROOT / "docs/design/principles.md"
SEARCH_ROOTS = [
    ROOT / "cli",
    ROOT / "tests",
    ROOT / "modules",
    ROOT / "AGENTS.md",
    ROOT / "README.md",
    ROOT / ".claude/agents",
    ROOT / ".claude/memory",
    ROOT / "prompts",
]
CITE_PATTERN = re.compile(r"docs/design/principles\.md#(\S+?)(?=[\"`)\s])")


def normalize_id(heading: str) -> str:
    heading = heading.strip()
    out: list[str] = []
    for ch in heading:
        if ch.isalnum() or ch in ("_", "-"):
            out.append(ch.lower())
        elif ch.isspace():
            out.append("-")
    return "".join(out)


def valid_anchors() -> set[str]:
    anchors: set[str] = set()
    for line in PRINCIPLES.read_text(encoding="utf-8").splitlines():
        if line.startswith("## "):
            anchors.add(normalize_id(line[3:]))
    return anchors


def iter_files(root: Path):
    if not root.exists():
        return
    if root.is_file():
        yield root
        return
    skip_dirs = {"target", ".git", ".direnv", "__pycache__"}
    for path in root.rglob("*"):
        if any(part in skip_dirs for part in path.relative_to(root).parts):
            continue
        if path.is_file():
            yield path


def main() -> int:
    anchors = valid_anchors()
    failures: list[str] = []

    for root in SEARCH_ROOTS:
        for path in iter_files(root):
            try:
                text = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for line_no, line in enumerate(text.splitlines(), start=1):
                for match in CITE_PATTERN.finditer(line):
                    anchor = match.group(1)
                    if anchor not in anchors:
                        failures.append(
                            f"{path.relative_to(ROOT)}:{line_no}: unresolved "
                            f"docs/design/principles.md#{anchor}"
                        )

    if failures:
        print("code doc anchor check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("code doc anchor check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
