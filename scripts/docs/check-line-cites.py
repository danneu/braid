#!/usr/bin/env python3
"""Forbid durable artifacts from citing tracked repo files by line number."""

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
FROZEN_DECISION_STATUSES = {"Superseded", "Deprecated"}
PATH_LINE_CITE = re.compile(
    r"(?P<path>[\w./-]+\.(?:rs|py|nix|md)):\d+(?:-\d+)?"
)
ADR_LINE_CITE = re.compile(r"\bADR\s+\d+:\d+\b")
PAREN_LINE_CITE = re.compile(r"\(lines?\s+\d+(?:-\d+)?\)")
TRACKED_PATH = re.compile(r"[\w./-]+\.(?:rs|py|nix|md)")
TILDE_LINE_CITE = re.compile(r"~\s*line\s+\d+")
NEAR_LINE_CITE = re.compile(r"\bnear\s+line\s+\d+")


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


def rel_display(path: Path, display_root: Path) -> str:
    try:
        return path.relative_to(display_root).as_posix()
    except ValueError:
        return path.name


def frontmatter_status(text: str) -> str | None:
    """Return the status from a leading frontmatter block, if present."""
    match = re.match(r"\A---\n(.*?)\n---\n", text, re.DOTALL)
    if not match:
        return None
    status = re.search(r"^status:\s*(\w+)\s*$", match.group(1), re.MULTILINE)
    if not status:
        return None
    return status.group(1)


def is_frozen_decision_doc(path: Path, text: str, display_root: Path) -> bool:
    """Return true for frozen ADRs whose historical body must not be repointed."""
    try:
        rel_parts = path.relative_to(display_root).parts
    except ValueError:
        return False
    if rel_parts[:3] != ("docs", "design", "decisions"):
        return False
    return frontmatter_status(text) in FROZEN_DECISION_STATUSES


def is_doc_citations_page(path: Path, display_root: Path) -> bool:
    try:
        return path.relative_to(display_root).as_posix() == "docs/dev/doc-citations.md"
    except ValueError:
        return path.name == "doc-citations.md"


def line_has_tracked_path(line: str) -> bool:
    for match in TRACKED_PATH.finditer(line):
        if "reference/" not in match.group(0):
            return True
    return False


def lint_file(path: Path, display_root: Path) -> list[str]:
    """Return line-number citation failures for one text file."""
    failures: list[str] = []
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return failures

    if is_doc_citations_page(path, display_root):
        return failures
    if is_frozen_decision_doc(path, text, display_root):
        return failures

    rel = rel_display(path, display_root)

    for line_no, line in enumerate(text.splitlines(), start=1):
        matches: list[str] = []
        for match in PATH_LINE_CITE.finditer(line):
            cited_path = match.group("path")
            if "reference/" not in cited_path:
                matches.append(match.group(0))
        matches.extend(match.group(0) for match in ADR_LINE_CITE.finditer(line))
        if line_has_tracked_path(line):
            matches.extend(match.group(0) for match in PAREN_LINE_CITE.finditer(line))
        matches.extend(match.group(0) for match in TILDE_LINE_CITE.finditer(line))
        matches.extend(match.group(0) for match in NEAR_LINE_CITE.finditer(line))

        for citation in matches:
            failures.append(
                f"{rel}:{line_no}: line-number citation of a tracked file "
                f"`{citation}`; cite `path#symbol` or `path#heading-slug` "
                f"(see docs/dev/doc-citations.md)"
            )
    return failures


def lint_roots(search_roots: list[Path], display_root: Path) -> list[str]:
    """Return line-number citation failures for all files under roots."""
    failures: list[str] = []
    for root in search_roots:
        for path in iter_files(root):
            failures.extend(lint_file(path, display_root))
    return failures


def _selftest() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "cli" / "src").mkdir(parents=True)
        (root / "docs" / "dev").mkdir(parents=True)
        (root / "docs" / "design" / "decisions").mkdir(parents=True)
        (root / "cli" / "src" / "fixture.rs").write_text(
            "\n".join(
                [
                    "bad: remove_missing.rs ~line 243",
                    "bad: replace.rs near line 214",
                    "bad: foo.rs:142",
                    "bad: bar.md:64-72",
                    "bad: ADR 014:74",
                    "bad: `x.nix` (lines 1-9)",
                    "ok: journal::write_journal",
                    "ok: reference/btrfs-progs/cmds/balance.c:558",
                    "ok: bare balance.c:558-561",
                    "ok: plan line 446",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        (root / "docs" / "design" / "decisions" / "021-old.md").write_text(
            "\n".join(
                [
                    "---",
                    "intent: Frozen fixture.",
                    "status: Superseded",
                    "---",
                    "",
                    "ok because frozen: cli/src/unlock.rs:93-96",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        (root / "docs" / "dev" / "doc-citations.md").write_text(
            "ok because negative example: cli/src/cmd/unlock.rs:142\n",
            encoding="utf-8",
        )
        failures = lint_roots([root / "cli", root / "docs"], root)

    offending = {
        re.search(r"`([^`]+)`; cite", failure).group(1) for failure in failures
    }
    expected = {
        "~line 243",
        "near line 214",
        "foo.rs:142",
        "bar.md:64-72",
        "ADR 014:74",
        "(lines 1-9)",
    }
    if offending != expected:
        print("line cites selftest FAILED:", file=sys.stderr)
        print(f"  expected failing refs: {sorted(expected)}", file=sys.stderr)
        print(f"  actual failing refs:   {sorted(offending)}", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("line cites selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    failures = lint_roots(SEARCH_ROOTS, ROOT)

    if failures:
        print("line cites check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("line cites check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
