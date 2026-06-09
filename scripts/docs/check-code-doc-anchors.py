#!/usr/bin/env python3
"""Validate source-tree citations to docs/*.md anchors (slugs via _mdslug)."""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path

from _mdslug import anchors_of


ROOT = Path(__file__).resolve().parents[2]
SEARCH_ROOTS = [
    ROOT / "cli",
    ROOT / "tests",
    ROOT / "modules",
    ROOT / "AGENTS.md",
    ROOT / "README.md",
    ROOT / ".claude/agents",
    ROOT / "prompts",
]
CITE_PATTERN = re.compile(r"(docs/[A-Za-z0-9_./-]+\.md)#([A-Za-z0-9_-]+)")


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


def lint_file(
    path: Path,
    resolution_root: Path,
    display_root: Path,
    anchor_cache: dict[Path, set[str]] | None = None,
) -> list[str]:
    """Return unresolved docs/*.md#anchor citations for one text file."""
    failures: list[str] = []
    if anchor_cache is None:
        anchor_cache = {}

    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return failures

    try:
        rel = path.relative_to(display_root).as_posix()
    except ValueError:
        rel = path.name

    for line_no, line in enumerate(text.splitlines(), start=1):
        for match in CITE_PATTERN.finditer(line):
            doc_ref, anchor = match.groups()
            target = resolution_root / doc_ref
            if not target.exists():
                failures.append(f"{rel}:{line_no}: unresolved {doc_ref}#{anchor}")
                continue
            if target not in anchor_cache:
                anchor_cache[target] = anchors_of(target)
            if anchor not in anchor_cache[target]:
                failures.append(f"{rel}:{line_no}: unresolved {doc_ref}#{anchor}")
    return failures


def lint_roots(
    search_roots: list[Path],
    resolution_root: Path,
    display_root: Path,
) -> list[str]:
    """Return unresolved docs/*.md#anchor citations under all search roots."""
    failures: list[str] = []
    anchor_cache: dict[Path, set[str]] = {}
    for root in search_roots:
        for path in iter_files(root):
            failures.extend(lint_file(path, resolution_root, display_root, anchor_cache))
    return failures


_PRINCIPLES_MD = "\n".join(
    [
        "# Principles",
        "",
        "## 3. Safe-by-construction operations",
        "",
    ]
)

_TARGET_MD = "\n".join(
    [
        "# Target",
        "",
        "## Real Heading",
        "",
    ]
)

_FIXTURE_RS = "\n".join(
    [
        "// pass: docs/internals/target.md#real-heading",
        "// fail: docs/internals/target.md#missing-heading",
        "// fail: docs/nope.md#x",
        "// pass: docs/design/principles.md#3-safe-by-construction-operations",
        "",
    ]
)


def _selftest() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "cli" / "src").mkdir(parents=True)
        (root / "docs" / "internals").mkdir(parents=True)
        (root / "docs" / "design").mkdir(parents=True)
        (root / "docs" / "internals" / "target.md").write_text(
            _TARGET_MD,
            encoding="utf-8",
        )
        (root / "docs" / "design" / "principles.md").write_text(
            _PRINCIPLES_MD,
            encoding="utf-8",
        )
        fixture = root / "cli" / "src" / "fixture.rs"
        fixture.write_text(_FIXTURE_RS, encoding="utf-8")
        failures = lint_roots([root / "cli"], root, root)

    offending = {re.search(r"unresolved (.+)$", f).group(1) for f in failures}
    expected = {"docs/internals/target.md#missing-heading", "docs/nope.md#x"}
    if offending != expected:
        print("code doc anchor selftest FAILED:", file=sys.stderr)
        print(f"  expected failing refs: {sorted(expected)}", file=sys.stderr)
        print(f"  actual failing refs:   {sorted(offending)}", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("code doc anchor selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    failures = lint_roots(SEARCH_ROOTS, ROOT, ROOT)

    if failures:
        print("code doc anchor check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("code doc anchor check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
